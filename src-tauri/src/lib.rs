mod config;
mod display_info;
mod explorer_hook;
mod flyout_window;
mod font_replacement;
mod gamepad;
mod gamma;
mod gamma_ramp;
mod hdr_toggle;
mod hotkeys;
mod input_monitor;
mod monitors;
mod power;
mod process;
mod taskbar_dimmer;
mod text_rendering;
mod tray_balloon;
mod tray_menu_window;
mod updater;
mod vendor_info;
mod window_icon;

use std::collections::HashMap;
#[cfg(target_os = "windows")]
use std::sync::OnceLock;
use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, Condvar, Mutex,
};
use tauri::{
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager,
};

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
        SIZE_MINIMIZED, SIZE_RESTORED, WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_HOTKEY, WM_NCDESTROY,
        WM_SETTINGCHANGE, WM_SIZE, WM_TIMER,
    };

    const SUBCLASS_ID: usize = 0x4F4C_4449; // "OLDI"
    static APP: OnceLock<AppHandle> = OnceLock::new();
    // True while the main window is minimized (between SIZE_MINIMIZED and the
    // next SIZE_RESTORED). SIZE_RESTORED also fires for ordinary resizes, so
    // this flag is what lets us tell a genuine restore-from-minimized apart
    // from a plain resize, only the former replays the restore animation.
    static WAS_MINIMIZED: AtomicBool = AtomicBool::new(false);

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
            // Minimize/restore tracking for the Text tab's native preview. On
            // minimize, destroy the preview window: it is an owned popup and
            // Windows would otherwise re-show it mid restore-animation, making
            // the text sample appear before the animation finishes. With the
            // HWND gone there is nothing to auto-show; the frontend recreates
            // it once the window is back and settled. On restore from
            // minimized, emit the same window-visibility event the tray paths
            // use (animated = true) so the frontend's animated-flag is correct
            // even when the restore came from the taskbar (which fires no
            // event of its own) instead of the tray icon.
            WM_SIZE => {
                if wparam.0 == SIZE_MINIMIZED as usize {
                    WAS_MINIMIZED.store(true, Ordering::SeqCst);
                    if let Some(app) = APP.get() {
                        let _ = crate::text_rendering::destroy_text_preview(app);
                    }
                } else if wparam.0 == SIZE_RESTORED as usize
                    && WAS_MINIMIZED.swap(false, Ordering::SeqCst)
                {
                    if let Some(app) = APP.get() {
                        let _ = app.emit(
                            "window-visibility",
                            serde_json::json!({ "visible": true, "animated": true }),
                        );
                        // Watch the window's pixels until the restore
                        // animation visually settles, then tell the frontend
                        // exactly when it ends (window-restore-settled) so the
                        // Text tab's preview doesn't need a fixed delay.
                        crate::text_rendering::start_restore_settle_watch(app, hwnd.0 as isize);
                    }
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
        // Fires for EVERY system-wide WM_SETTINGCHANGE broadcast (any app on
        // the system changing any setting), so it is noise at Debug: log it
        // only when detailed logging is enabled.
        if crate::detailed_logging_enabled() {
            log::debug!("[display-info] change notification ({source})");
        }
        // The CCD SDR white level write broadcasts a settings-change message
        // on some systems. Reacting to the app's own write as if it were a
        // display topology change would re-enumerate the display config
        // (modes, EDID, vendor info) on every slider step and make dragging
        // extremely laggy, so ignore notifications right after a write.
        if hdr_toggle::recent_sdr_white_write() {
            return;
        }
        let _ = app.emit("display-changed", source);
        // The vendor caches (primary-GPU adapter info, NVAPI context and its
        // display ID, and the DRS session) are all derived from the display
        // topology. A change can reorder or replace the primary adapter or
        // display, so invalidate them to force the next vendor query to
        // re-enumerate promptly.
        vendor_info::invalidate_display_caches();
        // The cached primary-HDR target used by get/set_sdr_white_level is
        // also derived from the display topology, a change can move it to a
        // different adapter/target, so invalidate it too.
        hdr_toggle::invalidate_hdr_target_cache();
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


// ── Pre-WebView log history ────────────────────────────────────────────────
// tauri-plugin-log's Webview target only forwards records while the frontend
// listener is attached; records logged before then (setup, config load, tray
// and dimmer initialization) are emitted as events with no listener and
// dropped. This extra Dispatch target captures those early records into a
// bounded ring buffer, and the frontend drains them via take_log_history
// right after it attaches the listener, so the terminal, file, Debug tab,
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
fn get_display_info_for(device_name: String) -> Option<display_info::DisplayInfo> {
    display_info::query_display_for(&device_name)
}

#[tauri::command]
fn refresh_vendor_display_info() -> vendor_info::VendorDisplayInfo {
    vendor_info::query_vendor_display_info_fresh()
}

// Cheap NVIDIA reference-mode probe for the Black Crush tab. Unlike
// refresh_vendor_display_info this does not run the full NVAPI color/VRR
// query chain; it reads the cached driver-registry flag (5 s TTL), so the tab
// can poll it without a per-tick NVAPI session.
#[tauri::command]
fn get_nvidia_reference_mode() -> bool {
    vendor_info::nvidia_reference_mode_enabled()
}

// Async (not sync) commands: the apply now persists the mode to the registry
// in the same ChangeDisplaySettingsExW call (CDS_UPDATEREGISTRY, ~245ms on
// some driver stacks, the same cost the confirm used to carry). A sync
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
// duration, the "hitch" seen after clicking Keep. Moving them onto the async
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

// Frontend self-heal for window visibility. The `window-visibility` event
// mirrors show/hide driven from the tray, but a restore that happens outside
// that path (e.g. clicking the taskbar button after minimize, or a missed
// event) never emits it, so the frontend's mirror flag can go stale and leave
// the active tab blank. Expose the real state so the frontend can reconcile
// on window focus.
#[tauri::command]
fn is_main_window_visible(app: AppHandle) -> bool {
    app.get_webview_window("main")
        .map(|window| {
            window.is_visible().unwrap_or(false) && !window.is_minimized().unwrap_or(false)
        })
        .unwrap_or(false)
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let is_minimized = window.is_minimized().unwrap_or(false);
        let is_visible = window.is_visible().unwrap_or(false);

        // Windows transfers focus to the taskbar while the tray icon is being
        // clicked, so is_focused() is already false by the time this callback
        // runs. Visibility and minimized state are the reliable toggle inputs.
        if is_visible && !is_minimized {
            if window.minimize().is_ok() {
                let _ = app.emit(
                    "window-visibility",
                    serde_json::json!({ "visible": false, "animated": false }),
                );
            }
            return;
        }

        if is_minimized {
            let _ = window.unminimize();
        }
        if window.show().is_ok() {
            let _ = window.set_focus();
            // animated = the window was minimized, so restoring replays the
            // Windows restore animation; a merely-hidden window (close-to-tray)
            // appears instantly. The frontend uses this to decide whether the
            // Text tab's native preview must wait out the animation before
            // showing.
            let _ = app.emit(
                "window-visibility",
                serde_json::json!({ "visible": true, "animated": is_minimized }),
            );
        }
    }
}

pub(crate) fn shutdown_gamma_worker(app: &AppHandle) {
    let state = app.state::<gamma::GammaState>();
    state.shutdown.requested.store(true, Ordering::Release);
    state.shutdown.wake.notify_one();
    if let Ok(mut guard) = state.safety_thread.lock() {
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
    }
    if let Ok(mut brightness) = state.screen_dim_brightness.lock() {
        brightness.clear();
    }
    unsafe {
        gamma_ramp::reset_display_ramp();
        gamma::reset_per_monitor_ramps(&state.per_monitor);
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
    power::shutdown();
    shutdown_gamma_worker(app);
    explorer_hook::shutdown();
    app.exit(0);
}

// Full app exit from the main window's Exit button. Shares the same graceful
// shutdown path as the tray's Exit (tray_menu_quit), so dimmer/hotkey/gamma
// resources are released and the config is flushed before the process ends.
#[tauri::command]
fn quit_app(app: AppHandle) {
    request_shutdown_and_exit(&app);
}

// One-time tray balloon on the first close-to-tray, so the user learns the
// app is still running. Persisted in the config; the static guard also covers
// rapid double-closes before the async config write has flushed.
static TRAY_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);
fn show_tray_notice(app: &AppHandle) {
    if TRAY_NOTICE_SHOWN.swap(true, Ordering::Relaxed) {
        return;
    }
    let mut config = config::load_config(app.clone());
    if config.tray_notice_shown {
        return;
    }
    config.tray_notice_shown = true;
    let _ = config::save_config(app.clone(), config);
    tray_balloon::show(
        "OLED Helper",
        "OLED Helper is still running in the system tray. Click the tray icon to reopen it.",
    );
}

// Debug-tab helper: re-enable the one-time tray balloon after it has already
// shown, by clearing both the in-process guard and the persisted flag.
#[tauri::command]
fn reset_tray_notice(app: AppHandle) -> Result<(), String> {
    TRAY_NOTICE_SHOWN.store(false, Ordering::Relaxed);
    let mut config = config::load_config(app.clone());
    config.tray_notice_shown = false;
    config::save_config(app, config)
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

// Applies a single HDR enhancement mode so exactly one of {RTX HDR, Auto HDR,
// Off} is active. Shared by the interactive command (set_hdr_enhancement_mode)
// and the elevated-restart pending apply in run(): the transition and its
// rollback must stay identical in both paths, or a failed transition could
// leave RTX and Auto HDR enabled together.
fn apply_hdr_mode(mode: &str) -> Result<(), String> {
    match mode {
        "rtx" => {
            if !vendor_info::is_nvidia_primary_gpu() {
                return Err("NVIDIA RTX HDR is unavailable on this GPU".into());
            }
            // Configure RTX first. If the driver rejects the elevated write
            // (UAC), Auto HDR is left untouched. Only disable Auto HDR after
            // RTX is known to be enabled, and roll RTX back if that second
            // operation fails so the transition never leaves both paths active.
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
        "auto" | "off" => {
            let want_auto = mode == "auto";
            // Disable RTX HDR only on NVIDIA machines (non-NVIDIA systems
            // expose only Off and Windows Auto HDR). Remember whether RTX was
            // on so a failed Auto HDR write can restore it.
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

#[tauri::command]
fn set_hdr_enhancement_mode(mode: String) -> Result<(), String> {
    apply_hdr_mode(&mode)
}

// Tab ids the app actually renders (see the tab switch chain in App.svelte);
// used to validate --open-tab forwarding on restart.

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
    let is_restart_child = std::env::args().any(|arg| arg == process::ARG_RESTART_PARENT);
    #[cfg(target_os = "windows")]
    {
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        if let Some(pos) = args.iter().position(|arg| arg == process::ARG_RESTART_PARENT) {
            if let Some(pid) = args
                .get(pos + 1)
                .and_then(|arg| arg.to_str())
                .and_then(|text| text.parse::<u32>().ok())
            {
                if !process::wait_for_restart_parent(pid) {
                    return;
                }
            }
        }
    }

    // An elevated relaunch (RTX HDR write) can carry the mode the user tried
    // to apply before the restart. Apply it once, shortly after startup, on a
    // background thread, this instance is elevated, so the DRS write that
    // failed before now succeeds.
    let pending_rtx_hdr_flags: Option<u32> = std::env::args().find_map(|arg| {
        arg.strip_prefix(process::ARG_RTX_HDR_FLAGS_PREFIX)
            .and_then(|value| value.parse().ok())
    });
    let pending_rtx_hdr_enabled: Option<bool> = std::env::args().find_map(|arg| {
        arg.strip_prefix(process::ARG_RTX_HDR_ENABLED_PREFIX)
            .and_then(|value| match value {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            })
    });
    let pending_hdr_mode: Option<String> = std::env::args().find_map(|arg| {
        arg.strip_prefix(process::ARG_HDR_MODE_PREFIX)
            .filter(|mode| matches!(*mode, "off" | "auto" | "rtx"))
            .map(str::to_string)
    });
    let pending_screen_dimmer_enabled: Option<bool> = std::env::args().find_map(|arg| {
        arg.strip_prefix(process::ARG_SCREEN_DIMMER_ENABLED_PREFIX)
            .and_then(|value| match value {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            })
    });
    // An elevated relaunch can carry the tab the user was on
    // (--open-tab=<tab>); take_startup_tab hands it to the frontend once so
    // the UI opens on the same tab after the UAC restart.
    let startup_tab: Option<String> =
        std::env::args().find_map(|arg| arg.strip_prefix(process::ARG_OPEN_TAB_PREFIX).map(str::to_string));
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
                    match apply_hdr_mode(mode) {
                        Ok(()) => log::info!(
                            "applied pending HDR enhancement mode '{mode}' after elevated restart"
                        ),
                        Err(error) => {
                            log::warn!("pending HDR enhancement mode apply failed: {error}")
                        }
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
                            Err(error) => {
                                log::warn!("pending RTX HDR global enable apply failed: {error}")
                            }
                        }
                    }
                }
            })
        {
            log::error!("could not start RTX HDR pending-apply thread: {error}");
        }
    }

    // "Always open as administrator" (Settings tab): relaunch elevated
    // before any window when the flag is set and this process isn't
    // elevated. Runs first so a single UAC prompt also covers the Screen
    // Dimmer check below. If the user dismisses the prompt the app continues
    // non-elevated and asks again on the next launch, the flag is their
    // explicit choice, so it stays set. The elevated replacement never
    // re-enters this branch (process_is_elevated). A dismissed prompt also
    // skips the Screen Dimmer elevation below: one prompt per launch. The
    // dimmer is disabled on disk in that case (same as its own decline path)
    // so a non-elevated session never runs a half-degraded version of it.
    #[cfg(target_os = "windows")]
    let mut elevation_declined = false;
    #[cfg(target_os = "windows")]
    if !process::process_is_elevated() && config::always_elevated_on_disk() {
        let extra = [format!("{}settings", process::ARG_OPEN_TAB_PREFIX)];
        if process::spawn_elevated(&extra).is_ok() {
            std::process::exit(0);
        }
        // The always-elevated prompt covers the dimmer too: it needs admin as
        // well, so a declined prompt skips the dimmer's own elevation ask
        // (one UAC prompt per launch). The dimmer's on-disk setting is left
        // untouched: the tab shows a restart-as-admin prompt instead, so the
        // user's intent survives a declined prompt.
        elevation_declined = true;
        log::warn!("[power] elevation declined; screen dimmer stays enabled but inactive");
    }

    // Screen Dimmer needs admin for its media-playback detection. Ask for
    // elevation BEFORE any window is created: the persisted config is probed
    // directly (no AppHandle exists yet) and a successful UAC relaunch hands
    // off to the elevated replacement, so a normal close/reopen never flashes
    // the window open before the prompt. A declined prompt (or any other
    // failed launch) falls through to a plain non-elevated startup with the
    // on-disk setting left intact; the dimmer never runs in the half-degraded
    // state (dimming on, media detection off), and the tab shows a
    // restart-as-admin prompt instead. The elevated replacement never
    // re-enters this branch (process_is_elevated). Skipped entirely when the
    // always-elevated prompt above was declined (it already covers the
    // dimmer's elevation).
    #[cfg(target_os = "windows")]
    if !elevation_declined && !process::process_is_elevated() && config::screen_protector_enabled_on_disk() {
        let extra = [
            format!("{}1", process::ARG_SCREEN_DIMMER_ENABLED_PREFIX),
            format!("{}screen-dimmer", process::ARG_OPEN_TAB_PREFIX),
        ];
        match process::spawn_elevated(&extra) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                log::warn!(
                    "[power] screen dimmer elevation declined ({error}); dimmer stays enabled but inactive"
                );
            }
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
        .manage(gamma::GammaState {
            desired: gamma_desired,
            per_monitor: Arc::new(Mutex::new(HashMap::new())),
            screen_dim_brightness: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Arc::new(gamma::GammaShutdown {
                requested: AtomicBool::new(false),
                wake: Condvar::new(),
                lock: Mutex::new(()),
            }),
            safety_thread: Mutex::new(None),
        })
        .manage(taskbar_dimmer::TaskbarDimmerState::default())
        .invoke_handler(tauri::generate_handler![
            gamma::apply_gamma_ramp,
            gamma::apply_gamma_ramp_for,
            monitors::list_monitors,
            log_frontend,
            get_display_info,
            get_display_info_for,
            refresh_vendor_display_info,
            get_nvidia_reference_mode,
            vendor_info::get_rtx_hdr_settings_state,
            set_hdr_enhancement_mode,
            vendor_info::set_rtx_hdr_state,
            process::relaunch_elevated,
            process::is_process_elevated,
            process::open_url,
            updater::check_for_update,
            updater::download_update,
            updater::apply_update,
            take_startup_tab,
            set_refresh_rate,
            set_resolution,
            confirm_display_mode_change,
            rollback_display_mode_change,
            get_display_target,
            is_main_window_visible,
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
            text_rendering::text_rendering_get_state,
            text_rendering::text_rendering_set_mode,
            text_rendering::text_rendering_set_gamma,
            text_rendering::text_rendering_set_cleartype_level,
            text_rendering::text_rendering_reset,
            text_rendering::text_rendering_restart_app,
            power::power_get_state,
            power::power_set_display_timeout,
            power::power_set_sleep_timeout,
            power::power_set_screensaver,
            power::power_turn_off_display,
            power::system_restart,
            power::power_set_screen_protector,
            power::power_dim_preview,
            gamepad::gamepad_input_active,
            font_replacement::font_list_installed,
            font_replacement::font_apply,
            font_replacement::font_restore,
            font_replacement::font_get_status,
            font_replacement::font_download_catalog,
            font_replacement::font_download_apply,
            font_replacement::font_preview_sources,
            text_rendering::text_preview_show,
            text_rendering::text_preview_update,
            text_rendering::text_preview_set_bounds,
            text_rendering::text_preview_hide,
            text_rendering::text_preview_destroy,
            text_rendering::text_preview_render,
            explorer_hook::explorer_hook_set_mode,
            explorer_hook::explorer_hook_set_enabled,
            explorer_hook::explorer_hook_set_scope,
            explorer_hook::explorer_hook_set_per_monitor,
            explorer_hook::explorer_hook_set_monitor_mode,
            explorer_hook::explorer_hook_set_monitor_knobs,
            explorer_hook::explorer_hook_set_monitor_custom,
            explorer_hook::explorer_hook_monitors,
            explorer_hook::explorer_hook_status,
            flyout_window::set_flyout_hdr,
            flyout_window::flyout_frontend_ready,
            flyout_window::flyout_layout_ready,
            flyout_window::flyout_hidden,
            flyout_window::show_mode_confirmation,
            tray_menu_window::tray_menu_get_display_info,
            tray_menu_window::tray_menu_set_resolution,
            tray_menu_window::tray_menu_set_refresh_rate,
            tray_menu_window::tray_menu_hide,
            tray_menu_window::tray_menu_open_main,
            tray_menu_window::tray_menu_restart,
            tray_menu_window::tray_menu_quit,
            tray_menu_window::tray_menu_set_expanded,
            quit_app,
            reset_tray_notice,
        ])
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_positioner::init())
        // Persist and restore the main window's geometry (size, position,
        // maximized) across launches, the official window-state plugin.
        // Registered at the BUILDER level, not in .setup(): Tauri creates the
        // config-defined windows (and fires the plugin's window_created hook
        // synchronously) before the user setup callback runs, so a plugin
        // registered there never sees the main window and its cache stays
        // empty (state file written as {}, neither saves nor restores).
        // Scoped to the "main" window only: the flyout and tray-menu popups
        // are positioned programmatically every time and must not be
        // restored. Flags exclude VISIBLE/DECORATIONS/FULLSCREEN so a
        // close-to-tray hide can never be restored as a hidden window (this
        // app always starts visible); size/position/maximized are saved on
        // resize/move/close and flushed on exit.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_filter(|label| label == "main")
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .build(),
        )
        .setup(move |app| {
            // Gamma safety thread: re-applies the last known ramp every 5
            // seconds so another app resetting the system gamma (fullscreen
            // games, driver mode switches) is corrected automatically.
            {
                let state = app.state::<gamma::GammaState>();
                let desired = state.desired.clone();
                let per_monitor = state.per_monitor.clone();
                let screen_dim_brightness = state.screen_dim_brightness.clone();
                let shutdown = state.shutdown.clone();
                let handle = std::thread::Builder::new()
                    .name("gamma-safety".into())
                    .spawn(move || gamma::gamma_safety_loop(
                        desired,
                        per_monitor,
                        screen_dim_brightness,
                        shutdown,
                    ))
                    .map_err(|error| format!("could not start gamma safety thread: {error}"))?;
                state
                    .safety_thread
                    .lock()
                    .map_err(|_| "gamma state was poisoned".to_string())?
                    .replace(handle);
            }

            // Always-on rotating file log (not just debug builds), GPU-init
            // hangs/crashes can happen in release builds too, and without a
            // persistent log the only evidence is whatever DevTools happened
            // to be open and scrolled to at the time. See log_frontend above
            // and the console bridge in main.js for how JS-side output gets in.
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    // The app's own modules keep full Trace (the Debug tab
                    // and file log stay detailed), but everything else -
                    // reqwest, hyper, tao's event-loop diagnostics, and the
                    // rest of the dependency tree, is capped at Info so
                    // library internals can't flood the log with per-request
                    // noise. That flood was filling files to the rotation cap
                    // in minutes during dev ("just a few lines" repeated
                    // thousands of times).
                    .level(log::LevelFilter::Info)
                    .level_for("app_lib", log::LevelFilter::Trace)
                    .max_file_size(5_000_000)
                    // Keep the newest 5 rotated archives; the plugin prunes
                    // older ones on startup and rotation, so the logs folder
                    // stays bounded instead of accumulating every 5 MB file
                    // forever (KeepAll left ~160 MB of backlog).
                    .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(5))
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
            // its creation, see flyout_window::prewarm for why.
            // Load the persisted config before the flyout/tray webviews are
            // pre-warmed: their first status reads (taskbar dimmer, etc.)
            // must see the saved values, not the state defaults.
            let mut saved_config = config::load_config(app.handle().clone());
            if let Some(enabled) = pending_screen_dimmer_enabled {
                saved_config.screen_protector_enabled = enabled;
                // Persist the pending state before the frontend loads its
                // config, so the elevated replacement and its UI agree even
                // if the original frontend was closed before its async save
                // completed.
                let _ = config::save_config(app.handle().clone(), saved_config.clone());
            }

            // Seed the backend's global gamma fallback before the screen-
            // protector worker starts. Per-monitor Black Crush profiles are
            // keyed by display identity + mode and are resolved by the
            // frontend's existing monitor-profile path immediately afterward;
            // the backend map is populated by apply_gamma_ramp_for.
            {
                let state = app.state::<gamma::GammaState>();
                state.desired.store(
                    (saved_config.black_lift * 10.0).round().clamp(-150.0, 150.0) as i32,
                    Ordering::Release,
                );
            }

            // Seed the taskbar dimmer state from the persisted config. The
            // main window's frontend applies these to the overlay shortly
            // after launch, but the pre-warmed tray menu reads status before
            // that, without the seed it paints the default opacity (95%) on
            // the first open and then jumps to the real value.
            {
                let dimmer_state = app.state::<crate::taskbar_dimmer::TaskbarDimmerState>();
                *dimmer_state
                    .enabled
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = saved_config.taskbar_dimmer_enabled;
                *dimmer_state
                    .opacity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = saved_config.taskbar_dimmer_opacity.clamp(0.0, 1.0);
                *dimmer_state
                    .fade_in_duration_ms
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = saved_config.taskbar_dimmer_fade_in_duration_ms;
                *dimmer_state
                    .fade_out_duration_ms
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = saved_config.taskbar_dimmer_fade_out_duration_ms;
            }

            // Apply the persisted Explorer-hook state (Text tab) at startup
            // so the hook installs without the Text tab having to be opened
            // first. Failures are logged, never fatal: the watchdog retries
            // the install, and the Text tab re-syncs on mount.
            if let Err(error) = explorer_hook::apply_startup_state(app.handle(), &saved_config) {
                log::warn!("[explorer-hook] startup apply failed: {error}");
            }

            // Start the screen protector watcher if a timeout was persisted,
            // so the dim works even before the frontend loads its config.
            {
                let state = app.state::<gamma::GammaState>();
                crate::power::init(
                    saved_config.screen_protector_enabled,
                    saved_config.screen_protector_seconds,
                    saved_config.screen_protector_opacity,
                    saved_config.screen_protector_fade_in_duration_ms,
                    saved_config.screen_protector_fade_out_duration_ms,
                    saved_config.screen_protector_fullscreen_detection,
                    state.desired.clone(),
                    state.per_monitor.clone(),
                    state.screen_dim_brightness.clone(),
                );
            }

            flyout_window::prewarm(app.handle());
            log::info!("[tray] mode flyout window ready");

            // Custom CSS tray popup (test replacement for the native menu on
            // right-click). Pre-warmed for the same reason as the flyout: its
            // WebView2 controller should exist before the first click.
            tray_menu_window::prewarm(app.handle());
            log::info!("[tray] custom tray menu window ready");

            let tray_image = window_icon::crisp_tray_image()
                .or_else(|| app.default_window_icon().cloned())
                .ok_or("missing default app icon")?;

            // TEST: right-click now opens the custom CSS popup
            // (tray_menu_window) instead of the native muda menu. No native
            // menu is attached to the tray icon, so Tauri/muda won't show its
            // own popup on right-click, we handle Click ourselves for both
            // buttons.
            TrayIconBuilder::with_id("main-tray")
                .icon(tray_image)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                    match event {
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } => show_main_window(tray.app_handle()),
                        TrayIconEvent::Click {
                            button: MouseButton::Right,
                            button_state: MouseButtonState::Up,
                            position,
                            ..
                        } => tray_menu_window::toggle(tray.app_handle(), position.x),
                        _ => {}
                    }
                })
                .build(app)?;

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
            // on Windows, it sets HICON from raw RGBA instead of reading per-size
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
                    // The window always closes to the tray; the top-bar Exit
                    // button and the tray's Exit are the real exits. Hiding
                    // keeps the process alive so the tray icon can reopen it.
                    api.prevent_close();
                    // Flush the window-state plugin's in-memory geometry to
                    // disk now: the plugin only writes on process exit, and
                    // the window may stay hidden in the tray for a long time
                    // (or the process be killed) before that happens. Save
                    // while still visible so the pre-hide geometry is kept.
                    use tauri_plugin_window_state::{AppHandleExt, StateFlags};
                    let _ = window.app_handle().save_window_state(
                        StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED,
                    );
                    if window.hide().is_ok() {
                        // First close-to-tray ever: tell the user the app is
                        // still running in the tray.
                        show_tray_notice(window.app_handle());
                        // The process remains alive in the tray, but
                        // release the optional vendor-driver resources
                        // while idle. This is separate from final
                        // shutdown so NVAPI can initialize lazily again
                        // when the window is shown.
                        vendor_info::release_for_tray();
                        let _ = window.app_handle().emit(
                            "window-visibility",
                            serde_json::json!({ "visible": false, "animated": false }),
                        );
                    }
                }
                tauri::WindowEvent::Destroyed => {
                    // Closing the main window hides it to the tray rather than
                    // destroying it. When it is actually destroyed (real exit),
                    // request a synchronous worker shutdown so the display
                    // ramp is reset before the process exits.
                    taskbar_dimmer::shutdown();
                    hotkeys::shutdown();
                    power::shutdown();
                    shutdown_gamma_worker(window.app_handle());
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
