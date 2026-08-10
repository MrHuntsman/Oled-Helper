mod config;
mod display_info;
mod flyout_window;
mod gamma_ramp;
mod hdr_toggle;
mod hotkeys;
mod taskbar_dimmer;
mod updater;
mod vendor_info;
mod window_icon;

#[cfg(target_os = "windows")]
use std::sync::OnceLock;
use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread::JoinHandle;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State,
};

struct TrayState {
    enabled: AtomicBool,
}

// Shared by the Debug tab's "Detailed logging" toggle. Diagnostic info from
// the vendor/EDID paths is intentionally quiet by default because those paths
// are refreshed periodically; warnings and errors remain visible regardless.
static DETAILED_LOGGING: AtomicBool = AtomicBool::new(false);

pub(crate) fn detailed_logging_enabled() -> bool {
    DETAILED_LOGGING.load(Ordering::Relaxed)
}

pub(crate) fn set_detailed_logging_enabled(enabled: bool) {
    DETAILED_LOGGING.store(enabled, Ordering::Relaxed);
}

// Toggles the vendor/EDID detailed diagnostics independently of the taskbar
// dimmer's own verbose switch (set_taskbar_dimmer_verbose_logging). The two
// were previously coupled through platform::set_verbose_logging, so enabling
// one flooded the log with the other's periodic chatter.
#[tauri::command]
fn set_detailed_logging(enabled: bool) {
    set_detailed_logging_enabled(enabled);
}

#[cfg(target_os = "windows")]
mod display_change_events {
    use super::*;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_HOTKEY, WM_NCDESTROY, WM_SETTINGCHANGE, WM_TIMER,
    };

    const SUBCLASS_ID: usize = 0x4F4C_4449; // "OLDI"
    static APP: OnceLock<AppHandle> = OnceLock::new();

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        subclass_id: usize,
        _ref_data: usize,
    ) -> LRESULT {
        match message {
            // Global hotkeys registered by hotkeys.rs. wparam carries the
            // hotkey id; this dispatch runs on the main thread (window
            // subclass), where WM_HOTKEY messages are delivered.
            WM_HOTKEY => {
                crate::hotkeys::handle_hotkey(wparam.0 as i32);
            }
            WM_TIMER => {
                crate::hotkeys::handle_timer(wparam.0);
            }
            WM_DISPLAYCHANGE => {
                if let Some(app) = APP.get() {
                    notify_display_changed(app, "WM_DISPLAYCHANGE");
                }
            }
            WM_DEVICECHANGE => {
                if let Some(app) = APP.get() {
                    notify_display_changed(app, "WM_DEVICECHANGE");
                }
            }
            // Display topology changes can also arrive as a settings
            // broadcast, especially when a virtual display driver updates
            // the active display configuration without changing the primary
            // mode first. Treat it as a display invalidation rather than
            // polling for changes.
            WM_SETTINGCHANGE => {
                if let Some(app) = APP.get() {
                    notify_display_changed(app, "WM_SETTINGCHANGE");
                }
            }
            WM_NCDESTROY => {
                let _ = RemoveWindowSubclass(hwnd, Some(window_proc), subclass_id);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }

    fn notify_display_changed(app: &AppHandle, source: &str) {
        let _ = app.emit("display-changed", source);
        // The vendor caches (primary-GPU adapter info, NVAPI context and its
        // display ID, and the DRS session) are all derived from the display
        // topology. A change can reorder or replace the primary adapter or
        // display, so invalidate them to force the next vendor query to
        // re-enumerate promptly.
        vendor_info::invalidate_display_caches();
        // The cached primary-HDR target used by get/set_sdr_white_level is
        // also derived from the display topology ,  a change can move it to a
        // different adapter/target, so invalidate it too.
        hdr_toggle::invalidate_hdr_target_cache();
        // Keep the tray menu's resolution/refresh-rate submenus in sync with
        // the active display configuration. Coalesced because a single display
        // change often arrives as a burst of several window messages.
        crate::rebuild_tray_menu_throttled(app);
    }

    pub fn install(window_handle: isize, app: AppHandle) -> Result<(), String> {
        let _ = APP.set(app);
        let hwnd = HWND(window_handle as *mut std::ffi::c_void);
        let installed = unsafe { SetWindowSubclass(hwnd, Some(window_proc), SUBCLASS_ID, 0) };
        if installed.as_bool() {
            Ok(())
        } else {
            Err("SetWindowSubclass failed".into())
        }
    }
}

// Gamma state ,  synchronous apply + periodic safety restore.
// The frontend calls apply_gamma_ramp on every slider change; the ramp is
// applied immediately (no polling thread, no IPC round-trip delay). A
// lightweight safety thread re-applies every 5 seconds so another app
// resetting the system gamma (fullscreen games, driver updates) is
// corrected without user interaction.
struct GammaShutdown {
    requested: AtomicBool,
    wake: Condvar,
    lock: Mutex<()>,
}

struct GammaState {
    desired: Arc<AtomicI32>,
    shutdown: Arc<GammaShutdown>,
    safety_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for GammaState {
    fn drop(&mut self) {
        self.shutdown.requested.store(true, Ordering::Release);
        self.shutdown.wake.notify_one();
        if let Ok(mut guard) = self.safety_thread.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
        unsafe {
            gamma_ramp::reset_display_ramp();
        }
    }
}

fn gamma_safety_loop(desired: Arc<AtomicI32>, shutdown: Arc<GammaShutdown>) {
    loop {
        let guard = shutdown
            .lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_guard, _timeout) = shutdown
            .wake
            .wait_timeout_while(guard, std::time::Duration::from_secs(5), |_| {
                !shutdown.requested.load(Ordering::Acquire)
            })
            .unwrap_or_else(|error| error.into_inner());
        if shutdown.requested.load(Ordering::Acquire) {
            break;
        }
        // A timeout is the normal five-second safety tick. The predicate also
        // closes the lost-notification race during shutdown.
        // Hold-to-compare applies neutral gamma directly on the hardware
        // without updating `desired`. Don't overwrite its comparison view.
        if crate::hotkeys::is_hold_compare_active() {
            if crate::vendor_info::nvidia_reference_mode_enabled() {
                continue;
            }
            crate::hotkeys::recover_hold_restore();
            continue;
        }
        let value = desired.load(Ordering::Relaxed);
        // When black lift is zero the system ramp is linear (OS default) , 
        // nothing to restore. Skip both the reference-mode check and the GDI
        // call to keep the thread truly idle while black crush is disabled.
        if value == 0 {
            continue;
        }
        if crate::vendor_info::nvidia_reference_mode_enabled() {
            continue;
        }
        let ramp = gamma_ramp::build_ramp(value);
        unsafe {
            gamma_ramp::set_display_ramp(&ramp);
        }
    }
}

// Applies the gamma ramp synchronously ,  the display update happens on the
// caller's thread (the Tauri command thread) and returns immediately. No
// background polling, no IPC delay. A separate safety thread re-applies at
// 5-second intervals to recover from external gamma resets.
// ── Pre-WebView log history ────────────────────────────────────────────────
// tauri-plugin-log's Webview target only forwards records while the frontend
// listener is attached; records logged before then (setup, config load, tray
// and dimmer initialization) are emitted as events with no listener and
// dropped. This extra Dispatch target captures those early records into a
// bounded ring buffer, and the frontend drains them via take_log_history
// right after it attaches the listener ,  so the terminal, file, Debug tab,
// and DevTools stay 1:1 even for startup records.
mod log_history {
    use serde::Serialize;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    const MAX_HISTORY: usize = 512;

    #[derive(Serialize, Clone)]
    pub struct LogHistoryEntry {
        pub level: String,
        pub message: String,
        // Wall-clock ms at capture time, so the frontend can replay startup
        // records with their true timestamps and in their true order instead
        // of stamping them with the (later) replay time.
        pub ts_ms: u64,
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    static HISTORY: Mutex<VecDeque<LogHistoryEntry>> = Mutex::new(VecDeque::new());
    static REPLAYED: AtomicBool = AtomicBool::new(false);

    fn level_str(level: log::Level) -> &'static str {
        match level {
            log::Level::Trace => "trace",
            log::Level::Debug => "debug",
            log::Level::Info => "info",
            log::Level::Warn => "warn",
            log::Level::Error => "error",
        }
    }

    fn capture(level: log::Level, message: String) {
        if REPLAYED.load(Ordering::Relaxed) {
            return; // live event stream is the single source now
        }
        if let Ok(mut history) = HISTORY.lock() {
            if history.len() >= MAX_HISTORY {
                history.pop_front();
            }
            history.push_back(LogHistoryEntry {
                level: level_str(level).to_string(),
                message,
                ts_ms: now_ms(),
            });
        }
    }

    // Runs inside tauri-plugin-log's fern pipeline as an extra target, so the
    // exact same record that reaches stdout/file/webview is captured here.
    pub fn fern_dispatch() -> fern::Dispatch {
        fern::Dispatch::new().chain(fern::Output::call(|record: &log::Record| {
            capture(record.level(), record.args().to_string());
        }))
    }

    // Drains the buffer once and stops buffering. Called by main.js after the
    // log://log listener resolves; from then on records flow live only.
    #[tauri::command]
    pub fn take_log_history() -> Vec<LogHistoryEntry> {
        REPLAYED.store(true, Ordering::Relaxed);
        let mut drained = Vec::new();
        if let Ok(mut history) = HISTORY.lock() {
            drained = history.drain(..).collect();
        }
        drained
    }
}

#[tauri::command]
fn apply_gamma_ramp(state: State<GammaState>, black_lift: i32) -> Result<(), String> {
    // Frontend clamps to ±150 already; clamp here too so a stray caller can
    // never drive the ramp math with an extreme value.
    let black_lift = black_lift.clamp(-150, 150);
    if crate::vendor_info::nvidia_reference_mode_enabled() {
        return Err("NVIDIA Override to reference mode is enabled; disable it in your NVIDIA driver settings under System → Displays to use Black Crush Tweak".into());
    }
    // Hold the same gamma lock across the state check, desired-value update,
    // and possible write. If release completes just before this command gets
    // the lock, this path applies the newest value instead of leaving the
    // linear compare ramp or an older restore value behind.
    let _gamma_guard = crate::hotkeys::gamma_write_guard();
    if !crate::hotkeys::is_hold_compare_active() {
        let ramp = gamma_ramp::build_ramp(black_lift);
        let applied = unsafe { gamma_ramp::set_display_ramp(&ramp) };
        if !applied {
            return Err("SetDeviceGammaRamp failed".into());
        }
    }
    state.desired.store(black_lift, Ordering::Relaxed);
    crate::hotkeys::update_hold_restore_locked(black_lift);
    Ok(())
}

// Bridge for frontend warnings/errors and uncaught JS errors, so the
// on-disk log captures actionable failures even if the app hangs or crashes
// before the user can open DevTools or the in-app Diagnostics tab.
#[tauri::command]
fn log_frontend(level: String, message: String) {
    // Keep the message untouched. The terminal/file formatter, Debug tab, and
    // DevTools all receive this same message through the canonical log record.
    match level.as_str() {
        "trace" => log::trace!("{message}"),
        "debug" => log::debug!("{message}"),
        "error" => log::error!("{message}"),
        "warn" => log::warn!("{message}"),
        _ => log::info!("{message}"),
    }
}

#[tauri::command]
fn get_display_info() -> Option<display_info::DisplayInfo> {
    display_info::query_current_display()
}

#[tauri::command]
fn refresh_vendor_display_info() -> vendor_info::VendorDisplayInfo {
    vendor_info::query_vendor_display_info_fresh()
}

// Async (not sync) commands: the apply now persists the mode to the registry
// in the same ChangeDisplaySettingsExW call (CDS_UPDATEREGISTRY, ~245ms on
// some driver stacks ,  the same cost the confirm used to carry). A sync
// command would block the Tauri main thread for that duration while the mode
// switch is happening. The async runtime keeps the main thread responsive.
// The tray path already runs set_resolution/set_refresh_rate on its own
// spawned thread; this makes the in-app path behave the same way.
#[tauri::command]
async fn set_refresh_rate(hz: u32) -> Result<display_info::AppliedMode, String> {
    display_info::set_refresh_rate(hz)
}

#[tauri::command]
async fn set_resolution(
    width: u32,
    height: u32,
    preferred_hz: u32,
) -> Result<display_info::AppliedMode, String> {
    display_info::set_resolution(width, height, preferred_hz)
}

// Async (not sync) commands: confirm/rollback persist or restore the display
// mode, which writes the user profile registry and can take hundreds of ms on
// some driver stacks. A sync command runs on the Tauri main thread and would
// freeze the whole app (tray, dimmer z-order repairs, WebView) for that
// duration ,  the "hitch" seen after clicking Keep. Moving them onto the async
// runtime keeps the main thread responsive.
#[tauri::command]
async fn confirm_display_mode_change() -> Result<(), String> {
    display_info::confirm_mode_change()
}

#[tauri::command]
async fn rollback_display_mode_change() -> Result<(), String> {
    display_info::rollback_mode_change()
}

#[tauri::command]
fn get_display_target() -> Option<display_info::DisplayTargetInfo> {
    display_info::display_target_info()
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.show().is_ok() {
            let _ = window.set_focus();
            let _ = app.emit("window-visibility", true);
        }
    }
}

// ── Tray menu with resolution / refresh-rate submenus ─────────────────────────
// The tray context menu exposes the display's supported resolutions and
// refresh rates in their own submenus, so the app's core display controls are
// reachable without opening the window. Check menu items mark the currently
// active mode. The menu is rebuilt whenever the display configuration changes
// and after every tray-driven mode change.

fn build_tray_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let show_item = MenuItem::with_id(app, "show", "Show OLED Helper", true, None::<&str>)?;
    let restart_item = MenuItem::with_id(app, "restart", "Restart", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

    let res_submenu = Submenu::new(app, "Resolution", true)?;
    let hz_submenu = Submenu::new(app, "Refresh Rate", true)?;
    let mut has_resolutions = false;
    let mut has_refresh_rates = false;
    if let Some(info) = display_info::query_current_display() {
        for resolution in &info.resolutions {
            has_resolutions = true;
            let item = CheckMenuItem::with_id(
                app,
                format!("res-{}x{}", resolution.width, resolution.height),
                format!("{}×{}", resolution.width, resolution.height),
                true,
                info.width == resolution.width && info.height == resolution.height,
                None::<&str>,
            )?;
            res_submenu.append(&item)?;
        }
        for hz in &info.refresh_rates {
            has_refresh_rates = true;
            let item = CheckMenuItem::with_id(
                app,
                format!("hz-{hz}"),
                format!("{hz} Hz"),
                true,
                *hz == info.refresh_hz,
                None::<&str>,
            )?;
            hz_submenu.append(&item)?;
        }
    }
    if !has_resolutions {
        res_submenu.append(&MenuItem::with_id(
            app,
            "res-none",
            "No resolutions available",
            false,
            None::<&str>,
        )?)?;
    }
    if !has_refresh_rates {
        hz_submenu.append(&MenuItem::with_id(
            app,
            "hz-none",
            "No refresh rates available",
            false,
            None::<&str>,
        )?)?;
    }

    Menu::with_items(
        app,
        &[
            &show_item,
            &res_submenu,
            &hz_submenu,
            &restart_item,
            &quit_item,
        ],
    )
}

fn rebuild_tray_menu(app: &AppHandle) {
    let Some(tray) = app.tray_by_id("main-tray") else {
        return;
    };
    match build_tray_menu(app) {
        Ok(menu) => {
            if let Err(error) = tray.set_menu(Some(menu)) {
                log::warn!("[tray] could not update tray menu: {error}");
            }
        }
        Err(error) => log::warn!("[tray] could not rebuild tray menu: {error}"),
    }
}

// A single display topology change often arrives as a burst of window messages
// (WM_DISPLAYCHANGE + WM_DEVICECHANGE + WM_SETTINGCHANGE, several of each).
// Every rebuild runs a full display query on the main thread, so coalesce
// notification-driven rebuilds. Tray-click paths call rebuild_tray_menu
// directly and are never throttled.
fn rebuild_tray_menu_throttled(app: &AppHandle) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_REBUILD: AtomicU64 = AtomicU64::new(0);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    if now_ms.saturating_sub(LAST_REBUILD.load(Ordering::Relaxed)) < 300 {
        return;
    }
    LAST_REBUILD.store(now_ms, Ordering::Relaxed);
    rebuild_tray_menu(app);
}

fn parse_mode_id(value: &str) -> Option<(u32, u32)> {
    let (width, height) = value.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

// Applies a mode picked from the tray menu. Always shows the small
// always-on-top "flyout" WebviewWindow for this confirmation, never the
// in-app modal ,  regardless of whether the main window happens to be open,
// so a tray-driven change always surfaces its confirmation at the tray
// rather than possibly popping the in-app dialog behind/inside a window the
// user wasn't necessarily looking at. The in-app dropdowns (App.svelte's
// changeResolution/changeRefreshRate) remain the only path that shows the
// in-app modal, since those calls only happen while the window is visible
// and focused.
fn tray_display_mode_change(app: AppHandle, result: Result<display_info::AppliedMode, String>) {
    match result {
        Ok(mode) => {
            // ChangeDisplaySettingsExW and wait_for_mode verify the new
            // DEVMODE, but Windows updates DWM, the shell taskbar, and the
            // monitor work area asynchronously afterward. Wait on this tray
            // worker until the requested monitor size is observed and the
            // monitor/work-area rectangles are stable across samples; never
            // block the Tauri/UI thread while the shell catches up.
            flyout_window::wait_for_display_geometry(mode.width, mode.height);

            // Give Windows/DWM/WebView2 a moment to finish settling display
            // and DPI changes before the flyout renders and positions itself.
            // wait_for_display_geometry already confirms the monitor rect is
            // stable, but downstream consumers (taskbar redraw, WebView2's
            // own DPI-changed handling) can still be catching up for a bit
            // longer; a fixed pause here is simpler and more predictable than
            // chasing every one of those signals individually.
            std::thread::sleep(std::time::Duration::from_secs(1));

            // Keep the handshake on this background worker. It waits for
            // frontend readiness and schedules only the short native prepare
            // and final-show operations on the Tauri thread; calling it from
            // run_on_main_thread would deadlock that thread while JavaScript
            // tries to deliver the readiness events.
            let window_app = app.clone();
            let shown_mode = mode.clone();
            if let Err(error) = flyout_window::show_after_ready(&window_app, shown_mode.clone()) {
                // The existing watchdog will safely restore the previous mode
                // even if the flyout window fails to show.
                log::error!(
                    "[tray] mode flyout window failed; mode will auto-revert safely: {error}"
                );
            } else {
                log::info!(
                    "[tray] mode flyout window shown for {}x{}@{}Hz",
                    shown_mode.width,
                    shown_mode.height,
                    shown_mode.refresh_hz
                );
            }
        }
        Err(error) => log::warn!("[tray] display mode change failed: {error}"),
    }
    rebuild_tray_menu(&app);
}

pub(crate) fn shutdown_gamma_worker(app: &AppHandle) {
    let state = app.state::<GammaState>();
    state.shutdown.requested.store(true, Ordering::Release);
    state.shutdown.wake.notify_one();
    if let Ok(mut guard) = state.safety_thread.lock() {
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
    }
    unsafe {
        gamma_ramp::reset_display_ramp();
    }
    // app.exit(0) bypasses Rust Drop, so explicitly close the process-lifetime
    // NVAPI DRS session/context before Tauri terminates. Flush afterward so
    // the teardown confirmation is written to the file log before exit.
    vendor_info::shutdown();
    log::logger().flush();
    config::shutdown();
}

fn request_shutdown_and_exit(app: &AppHandle) {
    taskbar_dimmer::shutdown();
    hotkeys::shutdown();
    shutdown_gamma_worker(app);
    app.exit(0);
}

// Set once at startup from --open-tab=<tab> (elevated RTX HDR restart) and
// consumed by take_startup_tab so the frontend reopens on the user's tab.
static STARTUP_TAB: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();

// Returns the tab requested via --open-tab (if any) exactly once, then clears
// it so a later frontend reload does not re-apply a stale tab.
#[tauri::command]
fn take_startup_tab() -> Option<String> {
    let mut guard = {
        let lock = STARTUP_TAB.get()?;
        lock.lock().ok()?
    };
    guard.take()
}

#[tauri::command]
fn set_hdr_enhancement_mode(mode: String) -> Result<(), String> {
    match mode.as_str() {
        "rtx" => {
            if !vendor_info::is_nvidia_primary_gpu() {
                return Err("NVIDIA RTX HDR is unavailable on this GPU".into());
            }
            // The admin-gated RTX write happens before touching Auto HDR. If
            // UAC is required, the current mode is left intact. If the second
            // operation fails, roll RTX back so both modes cannot remain on.
            vendor_info::enable_rtx_hdr_for_mode()?;
            if let Err(error) = hdr_toggle::set_auto_hdr_enabled(false) {
                let _ = vendor_info::set_rtx_hdr_enabled(false);
                return Err(error);
            }
        }
        "auto" | "off" => {
            let want_auto = mode == "auto";
            let was_rtx_enabled = if vendor_info::is_nvidia_primary_gpu() {
                vendor_info::get_rtx_hdr_settings()
                    .map(|settings| settings.enabled)
                    .unwrap_or(false)
            } else {
                false
            };
            if was_rtx_enabled {
                vendor_info::set_rtx_hdr_enabled(false)?;
            }
            if let Err(error) = hdr_toggle::set_auto_hdr_enabled(want_auto) {
                if was_rtx_enabled {
                    let _ = vendor_info::set_rtx_hdr_enabled(true);
                }
                return Err(error);
            }
        }
        _ => return Err(format!("unknown HDR enhancement mode '{mode}'")),
    }
    Ok(())
}

fn apply_pending_hdr_mode(mode: &str) -> Result<(), String> {
    match mode {
        "rtx" => {
            if !vendor_info::is_nvidia_primary_gpu() {
                return Err("NVIDIA RTX HDR is unavailable on this GPU".into());
            }
            // Configure RTX first. If the driver rejects the elevated write,
            // Auto HDR remains untouched. Only disable Auto HDR after RTX is
            // known to be enabled, and roll RTX back if that second operation
            // fails so the transition never leaves both paths active.
            vendor_info::enable_rtx_hdr_for_mode()?;
            if let Err(error) = hdr_toggle::set_auto_hdr_enabled(false) {
                let rollback = vendor_info::set_rtx_hdr_enabled(false);
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(format!(
                        "{error}; RTX HDR rollback also failed: {rollback_error}"
                    )),
                };
            }
        }
        "auto" => {
            // Disable RTX HDR only when this machine has NVIDIA RTX HDR.
            // Non-NVIDIA systems expose only Off and Windows Auto HDR.
            if vendor_info::is_nvidia_primary_gpu() {
                vendor_info::set_rtx_hdr_enabled(false)?;
            }
            hdr_toggle::set_auto_hdr_enabled(true)?;
        }
        "off" => {
            if vendor_info::is_nvidia_primary_gpu() {
                vendor_info::set_rtx_hdr_enabled(false)?;
            }
            hdr_toggle::set_auto_hdr_enabled(false)?;
        }
        _ => return Err(format!("unknown HDR enhancement mode '{mode}'")),
    }
    Ok(())
}

fn request_restart(app: &AppHandle) {
    // Always perform a real process restart, including when launched through
    // `tauri dev`. Spawn first so a failure leaves the current instance fully
    // functional rather than closing the app.
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            log::error!("failed to locate replacement executable: {error}");
            return;
        }
    };
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    args.push("--restart-parent".into());
    args.push(std::process::id().to_string().into());
    if let Err(error) = std::process::Command::new(exe).args(args).spawn() {
        log::error!("failed to spawn replacement instance: {error}");
        return;
    }

    taskbar_dimmer::shutdown();
    shutdown_gamma_worker(app);
    // Spawn the replacement before leaving the event loop so the next process
    // has a guaranteed handoff path. The child waits for this PID to exit
    // (--restart-parent) before initializing WebView2, preventing overlapping
    // browser/profile lifetimes during a tray restart.
    app.exit(0);
}

// RTX HDR lives in the NVIDIA global driver profile, which the driver only
// lets elevated processes write (SetSetting returns NVAPI_INVALID_USER_
// PRIVILEGE otherwise). Reads work non-elevated, so the UI can show the
// current mode ,  applying a change needs a one-time elevated restart.
// Relaunch via ShellExecuteW with the "runas" verb (UAC prompt), reusing the
// --restart-parent handoff so the new instance waits for this process to
// release WebView2's user-data folder. An optional pending RTX HDR mode is
// forwarded as --rtx-hdr-flags=<n> so the elevated instance applies it right
// after startup, and the active tab as --open-tab=<tab> so the user lands
// where they were.
#[tauri::command]
fn relaunch_elevated(
    app: AppHandle,
    rtx_hdr_flags: Option<u32>,
    rtx_hdr_enabled: Option<bool>,
    hdr_mode: Option<String>,
    tab: Option<String>,
) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    args.push("--restart-parent".into());
    args.push(std::process::id().to_string().into());
    if let Some(flags) = rtx_hdr_flags {
        args.push(format!("--rtx-hdr-flags={flags}").into());
    }
    if let Some(enabled) = rtx_hdr_enabled {
        args.push(format!("--rtx-hdr-enabled={}", u8::from(enabled)).into());
    }
    if let Some(mode) = hdr_mode.as_deref().filter(|mode| matches!(*mode, "off" | "auto" | "rtx")) {
        args.push(format!("--hdr-mode={mode}").into());
    }
    // Only forward tab ids the app actually has (see the tab switch chain in
    // App.svelte); anything else is ignored rather than carried into the
    // command line.
    const KNOWN_TABS: [&str; 8] = [
        "crush", "dimmer", "windows-hdr", "hotkeys", "debug", "system", "settings", "about",
    ];
    if let Some(tab) = tab {
        if KNOWN_TABS.contains(&tab.as_str()) {
            args.push(format!("--open-tab={tab}").into());
        }
    }
    // Quote each argument so a path with spaces survives the command line.
    let params: Vec<u16> = args
        .iter()
        .map(|arg| {
            let text = arg.to_string_lossy().replace('"', "\\\"");
            format!("\"{text}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let exe_w: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::w!("runas"),
            windows::core::PCWSTR(exe_w.as_ptr()),
            windows::core::PCWSTR(params.as_ptr()),
            windows::core::PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value greater than 32 on success; values ≤ 32
    // are failures. The one exception is ERROR_CANCELLED (1223): dismissing
    // the UAC prompt also yields > 32, so it must be checked explicitly , 
    // otherwise cancelling the elevation would look like success and kill
    // this instance. Keep the app fully functional whenever the launch did
    // not actually happen.
    let code = result.0 as isize;
    if code <= 32 || code == 1223 {
        // Translate the handful of codes users can actually hit into plain
        // language instead of surfacing the raw Win32/ShellExecuteW value , 
        // "ShellExecuteW returned 5" means nothing to someone who just wanted
        // to change an HDR setting.
        let message = match code {
            1223 => "Elevation cancelled ,  no changes made.".to_string(),
            5 => "Administrator access denied. Choose \"Yes\" on the prompt to apply.".to_string(),
            2 | 3 => "App not found to restart. Try reinstalling.".to_string(),
            8 => "Out of memory during restart. Close some programs and retry.".to_string(),
            26 => "A required file is in use. Close other app instances and retry.".to_string(),
            31 => "No program is associated with the restart. Try reinstalling.".to_string(),
            _ => format!("Restart with administrator access failed (error {code}). Restart manually as Administrator."),
        };
        return Err(message);
    }

    taskbar_dimmer::shutdown();
    shutdown_gamma_worker(&app);
    app.exit(0);
    Ok(())
}

#[tauri::command]
fn set_tray_enabled(
    app: tauri::AppHandle,
    state: State<TrayState>,
    enabled: bool,
) -> Result<(), String> {
    let tray = app
        .tray_by_id("main-tray")
        .ok_or_else(|| "system tray icon is not available".to_string())?;
    tray.set_visible(enabled)
        .map_err(|error| error.to_string())?;
    state.enabled.store(enabled, Ordering::Relaxed);
    Ok(())
}

// When this instance was spawned by a tray Restart, --restart-parent carries
// the old process's PID. Wait for it to fully exit before creating the
// WebView2 environment. A vanished parent is success; a live parent remains
// polled with low-frequency warnings until it exits. Also used by the
// updater's --apply-update mode to wait for the old process to release the
// exe file before swapping it.
#[cfg(target_os = "windows")]
pub(crate) fn wait_for_restart_parent(parent_pid: u32) -> bool {
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, STILL_ACTIVE,
    };
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, parent_pid) {
            Ok(handle) => handle,
            Err(error) => {
                // ERROR_INVALID_PARAMETER means the PID no longer exists;
                // other failures (for example, an access restriction) do not
                // prove that the parent released the WebView2 profile.
                if GetLastError() == ERROR_INVALID_PARAMETER {
                    log::info!("restart parent {parent_pid} is already gone");
                    return true;
                }
                log::error!(
                    "could not inspect restart parent {parent_pid}: {error}; aborting replacement"
                );
                return false;
            }
        };
        let mut last_warning = std::time::Instant::now();
        loop {
            let mut exit_code = 0u32;
            if GetExitCodeProcess(handle, &mut exit_code).is_err() {
                let _ = CloseHandle(handle);
                log::error!(
                    "could not read restart parent {parent_pid} status; aborting replacement"
                );
                return false;
            }
            if exit_code != STILL_ACTIVE.0 as u32 {
                let _ = CloseHandle(handle);
                return true;
            }
            if last_warning.elapsed() >= std::time::Duration::from_secs(5) {
                log::warn!("restart parent {parent_pid} is still shutting down; replacement remains waiting");
                last_warning = std::time::Instant::now();
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Portable-update handoff: a staged copy of the new exe was spawned with
    // --apply-update and is responsible for swapping the files and relaunching
    // once this process exits. Must run before any Tauri/WebView2 init.
    if updater::handle_apply_update_mode() {
        return;
    }
    if std::env::args().any(|arg| arg == "--display-mode-watchdog") {
        display_info::run_recovery_watchdog();
        return;
    }
    display_info::launch_recovery_watchdog_if_needed();

    // Tray-restart child: let the old instance release the user-data folder
    // before we start WebView2. If the parent remains alive or its state cannot
    // be verified, abort this child rather than competing for the profile.
    let is_restart_child = std::env::args().any(|arg| arg == "--restart-parent");
    #[cfg(target_os = "windows")]
    {
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        if let Some(pos) = args.iter().position(|arg| arg == "--restart-parent") {
            if let Some(pid) = args
                .get(pos + 1)
                .and_then(|arg| arg.to_str())
                .and_then(|text| text.parse::<u32>().ok())
            {
                if !wait_for_restart_parent(pid) {
                    return;
                }
            }
        }
    }

    // An elevated relaunch (RTX HDR write) can carry the mode the user tried
    // to apply before the restart. Apply it once, shortly after startup, on a
    // background thread ,  this instance is elevated, so the DRS write that
    // failed before now succeeds.
    let pending_rtx_hdr_flags: Option<u32> = std::env::args()
        .find_map(|arg| {
            arg.strip_prefix("--rtx-hdr-flags=")
                .and_then(|value| value.parse().ok())
        });
    let pending_rtx_hdr_enabled: Option<bool> = std::env::args()
        .find_map(|arg| {
            arg.strip_prefix("--rtx-hdr-enabled=")
                .and_then(|value| match value {
                    "1" | "true" => Some(true),
                    "0" | "false" => Some(false),
                    _ => None,
                })
        });
    let pending_hdr_mode: Option<String> = std::env::args().find_map(|arg| {
        arg.strip_prefix("--hdr-mode=")
            .filter(|mode| matches!(*mode, "off" | "auto" | "rtx"))
            .map(str::to_string)
    });
    // An elevated relaunch can carry the tab the user was on
    // (--open-tab=<tab>); take_startup_tab hands it to the frontend once so
    // the UI opens on the same tab after the UAC restart.
    let startup_tab: Option<String> = std::env::args()
        .find_map(|arg| arg.strip_prefix("--open-tab=").map(str::to_string));
    if let Some(tab) = startup_tab {
        if let Ok(mut guard) = STARTUP_TAB.get_or_init(|| Mutex::new(None)).lock() {
            *guard = Some(tab);
        }
    }
    if pending_rtx_hdr_flags.is_some()
        || pending_rtx_hdr_enabled.is_some()
        || pending_hdr_mode.is_some()
    {
        if let Err(error) = std::thread::Builder::new()
            .name("rtx-hdr-pending-apply".into())
            .spawn(move || {
                // Let nvapi and the driver settle before touching the profile.
                std::thread::sleep(std::time::Duration::from_millis(1500));
                // Apply the exclusive mode first. This disables the currently
                // active path before enabling the requested one, so an elevated
                // restart cannot leave Auto HDR and RTX HDR enabled together.
                if let Some(mode) = pending_hdr_mode.as_deref() {
                    match apply_pending_hdr_mode(mode) {
                        Ok(()) => log::info!("applied pending HDR enhancement mode '{mode}' after elevated restart"),
                        Err(error) => log::warn!("pending HDR enhancement mode apply failed: {error}"),
                    }
                }
                // Legacy individual RTX arguments remain supported for older
                // relaunches. Apply them after the mode only when present.
                if pending_hdr_mode.as_deref() == Some("rtx") {
                    if let Some(flags) = pending_rtx_hdr_flags {
                        match vendor_info::set_rtx_hdr_flags(flags) {
                        Ok(()) => log::info!(
                            "applied pending RTX HDR mode 0x{flags:02X} after elevated restart"
                        ),
                            Err(error) => log::warn!("pending RTX HDR mode apply failed: {error}"),
                        }
                    }
                }
                if pending_hdr_mode.is_none() {
                    if let Some(enabled) = pending_rtx_hdr_enabled {
                    match vendor_info::set_rtx_hdr_enabled(enabled) {
                        Ok(()) => log::info!(
                            "applied pending RTX HDR global enable={} after elevated restart",
                            enabled
                        ),
                            Err(error) => log::warn!("pending RTX HDR global enable apply failed: {error}"),
                        }
                    }
                }
            })
        {
            log::error!("could not start RTX HDR pending-apply thread: {error}");
        }
    }

    let mut builder = tauri::Builder::default();
    // Must be the first plugin registered (Tauri docs recommendation). Skip
    // it for our own restart-child processes: the old instance is still
    // alive (mid-shutdown) when the replacement launches, so the single-
    // instance lock would still be held and this new process would just get
    // forwarded to the dying instance and exit instead of taking over.
    if !is_restart_child {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }));
    }
    let gamma_desired = Arc::new(AtomicI32::new(0));
    crate::hotkeys::set_current_lift(gamma_desired.clone());
    builder
        .manage(GammaState {
            desired: gamma_desired,
            shutdown: Arc::new(GammaShutdown {
                requested: AtomicBool::new(false),
                wake: Condvar::new(),
                lock: Mutex::new(()),
            }),
            safety_thread: Mutex::new(None),
        })
        .manage(taskbar_dimmer::TaskbarDimmerState::default())
        .manage(TrayState {
            enabled: AtomicBool::new(true),
        })
        .invoke_handler(tauri::generate_handler![
            apply_gamma_ramp,
            log_frontend,
            get_display_info,
            refresh_vendor_display_info,
            vendor_info::get_rtx_hdr_settings_state,
            set_hdr_enhancement_mode,
            vendor_info::set_rtx_hdr_state,
            relaunch_elevated,
            updater::check_for_update,
            updater::download_update,
            updater::apply_update,
            take_startup_tab,
            set_refresh_rate,
            set_resolution,
            confirm_display_mode_change,
            rollback_display_mode_change,
            get_display_target,
            set_tray_enabled,
            config::load_config,
            config::save_config,
            config::reset_config,
            config::open_config_folder,
            set_detailed_logging,
            log_history::take_log_history,
            hotkeys::set_hotkey,
            hotkeys::reset_hotkeys,
            taskbar_dimmer::set_taskbar_dimmer,
            taskbar_dimmer::set_taskbar_dimmer_verbose_logging,
            taskbar_dimmer::set_taskbar_dimmer_detection,
            taskbar_dimmer::set_taskbar_dimmer_suspended,
            taskbar_dimmer::set_taskbar_dimmer_hover_mode,
            taskbar_dimmer::set_taskbar_dimmer_poll_interval,
            taskbar_dimmer::get_taskbar_dimmer_status,
            hdr_toggle::get_windows_hdr_status,
            hdr_toggle::set_windows_hdr_enabled,
            hdr_toggle::get_hdr_luminance,
            hdr_toggle::get_sdr_white_level,
            hdr_toggle::set_sdr_brightness_nits,
            hdr_toggle::get_auto_hdr_enabled,
            hdr_toggle::set_auto_hdr_enabled,
            flyout_window::set_flyout_hdr,
            flyout_window::flyout_frontend_ready,
            flyout_window::flyout_layout_ready,
            flyout_window::flyout_hidden,
        ])
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // Gamma safety thread: re-applies the last known ramp every 5
            // seconds so another app resetting the system gamma (fullscreen
            // games, driver mode switches) is corrected automatically.
            {
                let state = app.state::<GammaState>();
                let desired = state.desired.clone();
                let shutdown = state.shutdown.clone();
                let handle = std::thread::Builder::new()
                    .name("gamma-safety".into())
                    .spawn(move || gamma_safety_loop(desired, shutdown))
                    .map_err(|error| format!("could not start gamma safety thread: {error}"))?;
                state
                    .safety_thread
                    .lock()
                    .map_err(|_| "gamma state was poisoned".to_string())?
                    .replace(handle);
            }

            // Always-on rotating file log (not just debug builds) ,  GPU-init
            // hangs/crashes can happen in release builds too, and without a
            // persistent log the only evidence is whatever DevTools happened
            // to be open and scrolled to at the time. See log_frontend above
            // and the console bridge in main.js for how JS-side output gets in.
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Trace)
                    // tao's event-loop runner emits DEBUG lifecycle
                    // diagnostics ("NewEvents emitted without explicit
                    // RedrawEventsCleared" on every window focus/redraw when
                    // clicking the app) that are noise, not problems. Cap the
                    // whole tao module at Info so they are suppressed in every
                    // destination (stdout, file, Debug tab, DevTools) while
                    // real tao warnings/errors still pass through.
                    .level_for("tao", log::LevelFilter::Info)
                    .max_file_size(5_000_000)
                    .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepAll)
                    // The same logger record is fanned out to terminal, file,
                    // and the frontend event consumed by main.js. Keeping one
                    // logger target set prevents the three views from drifting.
                    .target(tauri_plugin_log::Target::new(
                        tauri_plugin_log::TargetKind::Webview,
                    ))
                    // Captures records emitted before the WebView attaches its
                    // listener, for the startup replay in main.js.
                    .target(tauri_plugin_log::Target::new(
                        tauri_plugin_log::TargetKind::Dispatch(log_history::fern_dispatch()),
                    ))
                    .build(),
            )?;

            // The hidden-tray confirmation is a second small Tauri WebviewWindow
            // ("flyout", see tauri.conf.json + flyout_window.rs) rendering the
            // same Svelte modal markup as the in-app confirmation. Pre-warmed
            // here (rather than lazily on first show()) so its WebView2
            // controller exists well before any display-mode change can race
            // its creation ,  see flyout_window::prewarm for why.
            flyout_window::prewarm(app.handle());
            log::info!("[tray] mode flyout window ready");

            let saved_config = config::load_config(app.handle().clone());
            app.state::<TrayState>()
                .enabled
                .store(saved_config.tray_enabled, Ordering::Relaxed);

            let tray_menu = build_tray_menu(app.handle())?;
            let tray_image = window_icon::crisp_tray_image()
                .or_else(|| app.default_window_icon().cloned())
                .ok_or("missing default app icon")?;

            TrayIconBuilder::with_id("main-tray")
                .icon(tray_image)
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    let id = event.id.as_ref();
                    match id {
                        "show" => show_main_window(app),
                        "restart" => request_restart(app),
                        "quit" => request_shutdown_and_exit(app),
                        // "res-3840x2160" ,  apply keeping the current refresh
                        // rate as the preferred one, mirroring the in-app
                        // dropdown's behavior. The mode switch can blank the
                        // screen for hundreds of ms, so it runs on a background
                        // thread and the result is handed back via an event.
                        _ if id.starts_with("res-") => {
                            if let Some((width, height)) =
                                id.strip_prefix("res-").and_then(parse_mode_id)
                            {
                                let already_active =
                                    display_info::query_current_display().as_ref().is_some_and(
                                        |info| info.width == width && info.height == height,
                                    );
                                if already_active {
                                    // Clicking the active mode toggled its check
                                    // mark ,  restore it without a mode change.
                                    rebuild_tray_menu(app);
                                    return;
                                }
                                // Do NOT show the main window here: the mode
                                // change itself may open the mini confirmation
                                // dialog, and forcing the full interface open
                                // defeats that. tray_display_mode_change routes
                                // the confirmation appropriately.
                                let app_handle = app.clone();
                                std::thread::spawn(move || {
                                    let preferred_hz = display_info::query_current_display()
                                        .map(|info| info.refresh_hz)
                                        .unwrap_or(0);
                                    tray_display_mode_change(
                                        app_handle,
                                        display_info::set_resolution(width, height, preferred_hz),
                                    );
                                });
                            }
                        }
                        // "hz-144"
                        _ if id.starts_with("hz-") => {
                            if let Some(hz) = id
                                .strip_prefix("hz-")
                                .and_then(|text| text.parse::<u32>().ok())
                            {
                                let already_active = display_info::query_current_display()
                                    .as_ref()
                                    .is_some_and(|info| info.refresh_hz == hz);
                                if already_active {
                                    rebuild_tray_menu(app);
                                    return;
                                }
                                // See the res- branch ,  never open the full
                                // window from a tray mode change.
                                let app_handle = app.clone();
                                std::thread::spawn(move || {
                                    tray_display_mode_change(
                                        app_handle,
                                        display_info::set_refresh_rate(hz),
                                    );
                                });
                            }
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            if let Some(tray) = app.tray_by_id("main-tray") {
                tray.set_visible(saved_config.tray_enabled)?;
            }

            let dimmer_state = app.state::<taskbar_dimmer::TaskbarDimmerState>();
            if let Err(error) = taskbar_dimmer::create_overlay(app.handle(), &dimmer_state) {
                log::warn!("taskbar dimmer overlay unavailable: {error}; will retry in background");
                // Startup-time overlay creation can transiently fail if the
                // taskbar/shell hasn't settled yet (e.g. right after a tray
                // restart, or during a resolution/refresh-rate change that
                // briefly recreates Shell_TrayWnd). create_overlay only runs
                // once here, so without a retry a transient failure disables
                // the dimmer for the rest of the session. Retry a few times
                // with a short delay instead of giving up immediately.
                let retry_app = app.handle().clone();
                std::thread::spawn(move || {
                    const MAX_ATTEMPTS: u32 = 5;
                    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(1000);
                    for attempt in 1..=MAX_ATTEMPTS {
                        std::thread::sleep(RETRY_DELAY);
                        let dimmer_state = retry_app.state::<taskbar_dimmer::TaskbarDimmerState>();
                        match taskbar_dimmer::create_overlay(&retry_app, &dimmer_state) {
                            Ok(()) => {
                                log::info!(
                                    "taskbar dimmer overlay created on retry attempt {attempt}"
                                );
                                // The frontend snapshots availability once at
                                // startup (get_taskbar_dimmer_status inside
                                // loadConfig) before this retry can possibly
                                // finish, so without this event the Taskbar
                                // Dimmer tab would keep showing "unavailable"
                                // for the rest of the session even though the
                                // overlay is now live.
                                let _ = retry_app.emit("taskbar-dimmer-available", true);
                                return;
                            }
                            Err(error) => {
                                log::warn!(
                                    "taskbar dimmer overlay retry {attempt}/{MAX_ATTEMPTS} failed: {error}"
                                );
                            }
                        }
                    }
                    log::error!(
                        "taskbar dimmer overlay could not be created after {MAX_ATTEMPTS} attempts; dimmer disabled for this session"
                    );
                });
            }

            // Tauri's runtime window icon (titlebar/taskbar while running) is blurry
            // on Windows ,  it sets HICON from raw RGBA instead of reading per-size
            // frames from icon.ico like Explorer does. Fix it manually.
            #[cfg(target_os = "windows")]
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(hwnd) = window.hwnd() {
                    let hwnd_raw = hwnd.0 as isize;
                    if let Err(error) =
                        display_change_events::install(hwnd_raw, app.handle().clone())
                    {
                        log::warn!(
                            "[display-info] could not install display-change event hook: {error}"
                        );
                    }
                    // Global hotkeys share the main window: WM_HOTKEY is
                    // delivered through the subclass above, and RegisterHotKey
                    // targets the same HWND. Registered here (setup runs on the
                    // main thread) from the persisted bindings.
                    hotkeys::init(app.handle().clone(), hwnd_raw, &saved_config);
                    window_icon::apply_crisp_window_icon(hwnd_raw);
                    // WebView2 initialization can reset the icon shortly after; reapply once more.
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        window_icon::apply_crisp_window_icon(hwnd_raw);
                    });
                }
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                    let tray_enabled = window
                        .app_handle()
                        .state::<TrayState>()
                        .enabled
                        .load(Ordering::Relaxed);
                    if tray_enabled {
                        api.prevent_close();
                        if window.hide().is_ok() {
                            // The process remains alive in the tray, but
                            // release the optional vendor-driver resources
                            // while idle. This is separate from final
                            // shutdown so NVAPI can initialize lazily again
                            // when the window is shown.
                            vendor_info::release_for_tray();
                            let _ = window.app_handle().emit("window-visibility", false);
                        }
                    }
                }
                tauri::WindowEvent::Destroyed => {
                    // Closing with the tray enabled only hides the window. When
                    // the window is actually destroyed, request a synchronous
                    // worker shutdown so the display ramp is reset before the
                    // process exits.
                    taskbar_dimmer::shutdown();
                    hotkeys::shutdown();
                    shutdown_gamma_worker(window.app_handle());
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
