mod config;
mod display_info;
mod early_log;
mod explorer_hook;
mod favorite_credit;
mod flyout_window;
mod font_replacement;
mod gamepad;
mod gamma;
mod gamma_ramp;
mod gamma_watch;
mod hdr_toggle;
mod helper;
mod hotkey_actions;
mod hotkeys;
mod input_monitor;
mod key_observer;
mod main_window;
mod monitors;
mod power;
mod process;
mod taskbar_dimmer;
mod text_rendering;
mod tray_balloon;
mod tray_menu_window;
mod tv;
mod updater;
mod vendor_info;
mod webview_env;
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
    static MAIN_HWND: Mutex<Option<isize>> = Mutex::new(None);
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
                    notify_display_changed(app, "WM_DISPLAYCHANGE", true);
                }
            }
            WM_DEVICECHANGE => {
                if let Some(app) = APP.get() {
                    notify_display_changed(app, "WM_DEVICECHANGE", true);
                }
            }
            // Display topology changes can also arrive as a settings
            // broadcast, especially when a virtual display driver updates
            // the active display configuration without changing the primary
            // mode first. Treat it as a display invalidation rather than
            // polling for changes. The hook table is NOT refreshed here:
            // WM_SETTINGCHANGE fires for every app on the system changing
            // any setting, and the monitor caps only change on real
            // topology/mode changes, which broadcast WM_DISPLAYCHANGE or
            // WM_DEVICECHANGE.
            WM_SETTINGCHANGE => {
                if let Some(app) = APP.get() {
                    notify_display_changed(app, "WM_SETTINGCHANGE", false);
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

    fn notify_display_changed(app: &AppHandle, source: &str, refresh_hook_table: bool) {
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
        // The Explorer-hook shared monitor table embeds each monitor's
        // current resolution/DPI caps; a real mode or topology change makes
        // them stale for memory-DC attribution and the DPI-virtualization
        // gate, so refresh the table while the hook feature is on.
        if refresh_hook_table {
            explorer_hook::refresh_monitor_table();
            // A topology change can shift the per-monitor profile identities
            // (one of two identical panels unplugged, or a second one
            // plugged in), so re-key the dimmer maps to the current keys
            // before the watchers look them up again.
            crate::taskbar_dimmer::rekey_profiles();
            // The taskbar moves to its new geometry during a resolution/DPI
            // change; poke the dimmer so the overlay repositions immediately
            // instead of waiting out the periodic taskbar-cache cadence (1s
            // in poll mode, the 5s catch-all in event mode), which left it
            // floating at the old position for a visible beat.
            crate::taskbar_dimmer::on_display_change();
            crate::power::rekey_profiles();
            // Black Crush profiles are keyed per display AND mode, but the
            // runtime gamma table is keyed per monitor only, so a mode change
            // left the outgoing mode's lift in place and the safety thread kept
            // re-applying it until a frontend resolved the incoming mode's
            // key, which only happens while the main webview exists. Re-resolve
            // here so the screen follows the mode straight from the tray.
            crate::gamma::reapply_after_display_change(app);
        }
        let _ = app.emit("display-changed", source);
        if refresh_hook_table {
            tray_menu_window::refresh_after_display_change(app);
            window_icon::refresh_tray_icon_after_display_change(app);
        }
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
        if let Ok(mut stored) = MAIN_HWND.lock() {
            *stored = Some(window_handle);
        }
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

// Cached variant for low-frequency, informational reads (the tray menu's
// HDR-mode row). Unlike refresh_vendor_display_info this serves the vendor
// cache when it is warm, so a tray open does not pay a full NVAPI/ADLX
// session query every time. The display-change path invalidates the driver
// context caches, so the first read after a topology/mode change still
// re-queries fresh.
#[tauri::command]
fn query_vendor_display_info() -> vendor_info::VendorDisplayInfo {
    vendor_info::query_vendor_display_info()
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
async fn set_refresh_rate(
    hz: u32,
    device_name: Option<String>,
) -> Result<display_info::AppliedMode, String> {
    display_info::set_refresh_rate(hz, device_name)
}

#[tauri::command]
async fn set_resolution(
    width: u32,
    height: u32,
    preferred_hz: u32,
    device_name: Option<String>,
    skip_confirmation: Option<bool>,
    favorite_id: Option<String>,
) -> Result<display_info::AppliedMode, String> {
    let result = display_info::set_resolution(
        width,
        height,
        preferred_hz,
        device_name,
        skip_confirmation.unwrap_or(false),
    )?;
    if result.skip_confirmation {
        display_info::confirm_mode_change()?;
    } else if let Some(favorite_id) = favorite_id.as_deref() {
        // This confirmation dialog belongs to a favorite: a Keep credits it as
        // trusted on its monitor (see favorite_credit). Armed here, before the
        // frontend asks the flyout to show, so the credit is in place the
        // moment the user can click Keep.
        favorite_credit::arm(favorite_id);
    }
    Ok(result)
}

// Async (not sync) commands: confirm/rollback persist or restore the display
// mode, which writes the user profile registry and can take hundreds of ms on
// some driver stacks. A sync command runs on the Tauri main thread and would
// freeze the whole app (tray, dimmer z-order repairs, WebView) for that
// duration, the "hitch" seen after clicking Keep. Moving them onto the async
// runtime keeps the main thread responsive.
#[tauri::command]
async fn confirm_display_mode_change(app: AppHandle) -> Result<(), String> {
    let result = display_info::confirm_mode_change();
    // The flyout frontend hides its window before invoking this, but a
    // failed or blank hide would leave a transparent always-on-top window
    // blocking clicks on the main window. Force-hide it here regardless of
    // the transaction result; hiding an already-hidden window is a no-op.
    let _ = crate::flyout_window::hide(&app);
    // The Keep is what earns a favorite its skip-the-dialog credit. Commit it
    // from the transaction's own result so every apply path (in-app menu,
    // in-app hotkey, tray menu, tray hotkey) is credited the same way. A
    // failed confirm leaves the mode pending, so the credit is dropped and the
    // watchdog's rollback disarms it too.
    if result.is_ok() {
        favorite_credit::commit(&app);
    } else {
        favorite_credit::disarm();
    }
    // Report the transaction's real outcome to the main window.
    crate::flyout_window::emit_completed(&app, result.is_ok());
    result
}

#[tauri::command]
async fn rollback_display_mode_change(app: AppHandle) -> Result<(), String> {
    let result = display_info::rollback_mode_change();
    let _ = crate::flyout_window::hide(&app);
    // A Revert never marks a favorite as trusted, the mode was rejected.
    favorite_credit::disarm();
    crate::flyout_window::emit_completed(&app, false);
    result
}

// The flyout reports when it actually became visible. If the show handshake
// ate part of the confirmation window (a cold WebView2 start after the app sat
// in the tray can take seconds), the rollback deadline is pushed out so the
// user still gets the full window from the moment the dialog is on screen.
// See display_info::extend_mode_confirmation.
#[tauri::command]
fn extend_mode_confirmation(token: u64, deadline_ms: u64) -> Option<u64> {
    display_info::extend_mode_confirmation(token, deadline_ms)
}

#[tauri::command]
fn get_display_target() -> Option<display_info::DisplayTargetInfo> {
    display_info::display_target_info()
}

// Frontend self-heal for main webview availability. The `window-visibility`
// event mirrors the real tray hide, but a taskbar minimize is not a tray hide:
// the native window stays visible and its webview must remain mounted. Expose
// that state so a focus reconciliation cannot mistake a taskbar minimize for
// the close-to-tray teardown path.
#[tauri::command]
fn is_main_window_visible(app: AppHandle) -> bool {
    let visible = main_window::window(&app)
        .map(|window| window.is_visible().unwrap_or(false))
        .unwrap_or(false);
    log::debug!("[window] native visibility queried visible={visible}");
    visible
}

// The rebuilt main frontend reports in here once its event listeners are
// attached, which is what hands the global hotkeys back to it. Until then
// hotkeys::handle_hotkey performs them in Rust (hotkey_actions): an event
// emitted into a webview that is still loading would just be lost, and the
// renderer suspension that hid the window may still be in effect.
#[tauri::command]
fn main_frontend_ready() {
    hotkeys::set_frontend_alive(true);
}

pub(crate) fn show_main_window(app: &AppHandle) {
    if let Some(window) = main_window::window(app) {
        let is_minimized = window.is_minimized().unwrap_or(false);
        let is_visible = window.is_visible().unwrap_or(false);

        // Windows transfers focus to the taskbar while the tray icon is being
        // clicked, so is_focused() is already false by the time this callback
        // runs. Visibility and minimized state are the reliable toggle inputs.
        if is_visible && !is_minimized {
            // This is a taskbar minimize, not a close to the tray. Keep the
            // main webview active and keep the frontend mounted so restoring
            // from the taskbar is immediate and preserves its state.
            let _ = window.minimize();
            return;
        }

        // Hidden (or minimized): show it again. main_window::reveal also
        // rebuilds the webview when the tray teardown took it away, and does
        // the rebuild off this thread because add_child waits on main-thread
        // work of its own.
        main_window::reveal(app);
    }
}

// The exit paths can observe the same shutdown twice: request_shutdown_and_exit
// runs the sequence explicitly, then app.exit(0) destroys the main window,
// whose Destroyed handler runs it again. The module shutdowns are individually
// idempotent, and this flag makes the whole gamma teardown (ramp reset, vendor
// session release, config worker join, watch-record removal) run exactly once.
static GAMMA_SHUTDOWN_STARTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn shutdown_gamma_worker(app: &AppHandle) {
    if GAMMA_SHUTDOWN_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
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
    // With the ramps back to default there is nothing left for the external
    // gamma watchdog to restore, so release its record; the child exits on its
    // next poll.
    gamma_watch::shutdown();
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
    tv::shutdown(app);
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

// Serializes the whole multi-write HDR transition. Each individual NVAPI/
// registry write is already locked inside vendor_info/hdr_toggle, but a
// transition spans several of them (enable RTX, then disable Auto HDR, with a
// rollback on failure). Without a lock around the entire transaction, the
// elevated-restart pending-apply thread (which runs ~1.5 s after startup) and
// a user clicking an HDR mode in the Windows HDR tab could interleave and end
// with both RTX HDR and Auto HDR active, or with the wrong final mode.
static HDR_MODE_LOCK: Mutex<()> = Mutex::new(());

// Applies a single HDR enhancement mode so exactly one of {RTX HDR, Auto HDR,
// Off} is active. Shared by the interactive command (set_hdr_enhancement_mode)
// and the elevated-restart pending apply in run(): the transition and its
// rollback must stay identical in both paths, or a failed transition could
// leave RTX and Auto HDR enabled together.
pub(crate) fn apply_hdr_mode(mode: &str) -> Result<(), String> {
    let _transaction_guard = HDR_MODE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
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
async fn set_hdr_enhancement_mode(app: AppHandle, mode: String) -> Result<(), String> {
    let result = match apply_hdr_mode(&mode) {
        // A DRS write from a standard-user process returns the admin marker.
        // The whole exclusive transition then runs in the elevated helper
        // child (same apply_hdr_mode, so the rollback behavior is identical)
        // instead of relaunching the app elevated. The app itself stays
        // non-elevated, which is what avoids the elevated-GUI hotkey issue.
        Err(error) if error.contains("ADMIN_REQUIRED:") => {
            crate::helper::ensure_registered().map_err(|setup_error| {
                format!("The HDR change needs a one-time elevated helper setup. {setup_error}")
            })?;
            let helper_mode = mode.clone();
            let run = tauri::async_runtime::spawn_blocking(move || {
                crate::helper::run_request(
                    "hdr-mode",
                    |request| request.mode = Some(helper_mode.clone()),
                    std::time::Duration::from_secs(20),
                )
            })
            .await
            .map_err(|error| format!("the privileged helper task failed: {error}"))?;
            crate::helper::result_into_std(run?)
        }
        other => other,
    };
    if result.is_ok() {
        // The HDR enhancement mode has no settings-change broadcast of its
        // own, so notify listeners (the tray's mode selector) when it
        // changes instead of leaving them to poll.
        let _ = app.emit("hdr-mode-changed", mode);
    }
    result
}

// Tab ids the app actually renders (see the tab switch chain in App.svelte);
// used to validate --open-tab forwarding on restart.

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Earliest possible file logging. The restart handoff (the replacement's
    // parent wait and the single-instance lock race), the updater's apply
    // child, and the watchdog children all run before the log plugin exists,
    // and their records used to be lost, which is exactly what made the
    // silent tray-restart failure of 2026-09-10 undiagnosable. The early
    // logger appends to the same log file in the same format; setup() hands
    // its global slot to the plugin via early_log::attach_plugin_logger.
    early_log::init();
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
    // External gamma watchdog child: owns no window and runs before any
    // Tauri/WebView2 init, so it can reset the display gamma ramp if this
    // process dies while a non-default ramp is applied.
    if std::env::args().any(|arg| arg == "--gamma-watchdog") {
        gamma_watch::run_watchdog();
        return;
    }
    // One-shot elevated child that registers the privileged-helper Task
    // Scheduler task (/RL HIGHEST) and exits. Spawned by the non-elevated
    // app the first time a feature needs the helper; must dispatch before
    // the --restart-parent wait below because the spawner keeps running.
    if std::env::args().any(|arg| arg == helper::ARG_REGISTER_HELPER_TASK) {
        helper::run_register_helper_task();
        return;
    }
    // The scheduled task's entry: performs one privileged operation read
    // from helper-request.json, writes helper-result.json, and exits. Runs
    // before any Tauri/WebView2 init, same as the gamma watchdog.
    if std::env::args().any(|arg| arg == helper::ARG_PRIVILEGED_KEY_OBSERVER) {
        helper::run_privileged_key_observer();
        return;
    }
    if std::env::args().any(|arg| arg == helper::ARG_PRIVILEGED_HELPER) {
        helper::run_privileged_helper();
        return;
    }
    display_info::launch_recovery_watchdog_if_needed();
    // A stale gamma watch record (a session that died with a ramp applied)
    // spawns a resolver child that clears the stuck ramp.
    gamma_watch::start();

    // Tray-restart child: let the old instance release the user-data folder
    // before we start WebView2. The wait is bounded (see
    // wait_for_restart_parent): a parent stuck in teardown is terminated so
    // the restart can complete, and only an unverifiable parent aborts this
    // child rather than competing for the profile.
    let is_restart_child = std::env::args().any(|arg| arg == process::ARG_RESTART_PARENT);
    // Start on Boot launches carry --start-tray (baked into the registry Run
    // entry by the autostart plugin args): open straight into the tray, same
    // idle state a close-to-tray reaches, without stealing focus at login.
    // Migration first: an entry created by an older build lacks the flag, and
    // rewriting it here means even a never-re-enabled Start on Boot starts in
    // the tray from now on.
    if std::env::args().any(|arg| arg == process::ARG_START_TRAY) {
        process::set_start_in_tray();
    }
    process::migrate_autostart_entry();
    #[cfg(target_os = "windows")]
    {
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        if let Some(pos) = args
            .iter()
            .position(|arg| arg == process::ARG_RESTART_PARENT)
        {
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

    // Acquire the single-instance lock before any elevation relaunch. A
    // normal launch may immediately hand off to an elevated child for the
    // Screen Dimmer or the always-elevated setting, so checking this later
    // would let every launch create another replacement process. Restart
    // children reach this point only after their old parent has exited, and
    // they retry the lock while that old instance finishes releasing its
    // handles, so a deliberate handoff is never mistaken for a duplicate
    // launch.
    if !process::acquire_single_instance(process::APP_IDENTIFIER, is_restart_child) {
        return;
    }

    // A tray restart can carry the tab the user was on (--open-tab=<tab>);
    // take_startup_tab hands it to the frontend once so the UI opens on the
    // same tab after the restart. The TV Controller tab is opt in: launching
    // with --tv (or -tv) reveals it in the nav AND opens on it, while an
    // explicit --open-tab still wins so a restart handoff lands on the tab
    // the user chose.
    let tv_launch = std::env::args().any(|arg| process::is_tv_launch_arg(&arg));
    if tv_launch {
        process::set_tv_launch();
        // Verifiable in the file/terminal log (early_log is already up), so a
        // launch without the tab can be told apart from a frontend that
        // failed to read the flag.
        log::info!("[tv] launch flag --tv / -tv present; TV Controller tab enabled");
    }
    let startup_tab: Option<String> = std::env::args().find_map(|arg| {
        arg.strip_prefix(process::ARG_OPEN_TAB_PREFIX)
            .map(str::to_string)
    });
    let startup_tab = startup_tab.or_else(|| tv_launch.then(|| process::TV_TAB_ID.to_string()));
    if let Some(tab) = startup_tab {
        if let Ok(mut guard) = STARTUP_TAB.get_or_init(|| Mutex::new(None)).lock() {
            *guard = Some(tab);
        }
    }

    // No feature forces an elevated app session anymore: the Screen Dimmer,
    // RTX HDR writes, and font replacement all run their privileged steps in
    // the transient scheduled-task helper child (helper.rs), so the GUI
    // stays non-elevated by design. An already-elevated session (legacy
    // install, manual "Run as administrator") still works; it just never
    // starts that way on its own.
    let builder = tauri::Builder::default();
    let gamma_desired = Arc::new(AtomicI32::new(0));
    let gamma_per_monitor = Arc::new(Mutex::new(HashMap::new()));
    let gamma_brightness = Arc::new(Mutex::new(HashMap::new()));
    crate::hotkeys::set_current_lift(gamma_desired.clone());
    crate::hotkeys::set_per_monitor_lifts(gamma_per_monitor.clone());
    crate::hotkeys::set_screen_dim_brightness(gamma_brightness.clone());
    builder
        .manage(gamma::GammaState {
            desired: gamma_desired,
            per_monitor: gamma_per_monitor,
            screen_dim_brightness: gamma_brightness,
            shutdown: Arc::new(gamma::GammaShutdown {
                requested: AtomicBool::new(false),
                wake: Condvar::new(),
                lock: Mutex::new(()),
            }),
            safety_thread: Mutex::new(None),
        })
        .manage(taskbar_dimmer::TaskbarDimmerState::default())
        .manage(tv::TvControllerState::default())
        .invoke_handler(tauri::generate_handler![
            gamma::apply_gamma_ramp,
            gamma::apply_gamma_ramp_for,
            monitors::list_monitors,
            log_frontend,
            get_display_info,
            get_display_info_for,
            refresh_vendor_display_info,
            query_vendor_display_info,
            get_nvidia_reference_mode,
            vendor_info::get_rtx_hdr_settings_state,
            set_hdr_enhancement_mode,
            vendor_info::set_rtx_hdr_state,
            helper::ensure_helper_task,
            helper::helper_task_registered,
            process::open_url,
            process::tv_tab_enabled,
            updater::check_for_update,
            updater::download_update,
            updater::apply_update,
            take_startup_tab,
            set_refresh_rate,
            set_resolution,
            confirm_display_mode_change,
            extend_mode_confirmation,
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
            taskbar_dimmer::set_taskbar_dimmer_enabled_for_all,
            taskbar_dimmer::set_taskbar_dimmer_verbose_logging,
            taskbar_dimmer::set_taskbar_dimmer_detection,
            taskbar_dimmer::set_taskbar_dimmer_suspended,
            taskbar_dimmer::set_taskbar_dimmer_hover_mode,
            taskbar_dimmer::set_taskbar_dimmer_poll_interval,
            taskbar_dimmer::get_taskbar_dimmer_status,
            tv::tv_get_state,
            tv::tv_get_discovery,
            tv::tv_discover,
            tv::tv_connect,
            tv::tv_pair,
            tv::tv_disconnect,
            tv::tv_forget,
            tv::tv_adjust_volume,
            tv::tv_toggle_mute,
            tv::tv_restart,
            tv::tv_refresh_picture_context,
            tv::tv_set_picture_mode,
            tv::tv_set_picture_setting,
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
            power::power_get_screen_dimmer_status,
            power::power_get_screen_dimmer_diagnostics,
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
            tray_menu_window::tray_menu_get_state,
            tray_menu_window::tray_menu_save_favorites,
            tray_menu_window::tray_menu_set_resolution,
            tray_menu_window::tray_menu_set_refresh_rate,
            tray_menu_window::tray_menu_hide,
            tray_menu_window::tray_menu_open_main,
            tray_menu_window::tray_menu_restart,
            tray_menu_window::tray_menu_quit,
            tray_menu_window::tray_menu_debug,
            tray_menu_window::tray_menu_set_keep_warm,
            tray_menu_window::tray_menu_hdr_report,
            tray_menu_window::tray_menu_set_expanded,
            quit_app,
            reset_tray_notice,
            main_frontend_ready,
        ])
        // The Start on Boot entry launches with --start-tray so a login
        // autostart opens straight into the tray (see process::start_in_tray);
        // manual launches never carry the flag and behave as before. The
        // one-time migration in run() rewrites entries created before this
        // arg existed.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![process::ARG_START_TRAY]),
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
        // close-to-tray hide can never be restored as a hidden window (an
        // autostart launch starts hidden by design, see main_window::create);
        // size/position/maximized are saved on resize/move/close and flushed
        // on exit.
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
            // The main window is built here rather than declared in
            // tauri.conf.json: it has to be a plain window with a webview we
            // can add and drop on demand (see main_window), and Tauri would
            // create a config-declared window as an inseparable
            // WebviewWindow. Its size/position still come back across launches
            // through the window-state plugin, which is registered at the
            // builder level above so it sees this window too.
            main_window::create(app.handle())?;

            // An autostart launch begins without a webview (the window was
            // created hidden), the same state a close-to-tray teardown
            // reaches, so hand the global hotkeys to the Rust dispatcher
            // right away: FRONTEND_ALIVE otherwise still says true from its
            // static default and every hotkey would be emitted into a
            // webview that does not exist yet. The rebuilt frontend flips it
            // back on its first reveal (on_page_load / main_frontend_ready).
            if process::start_in_tray() {
                hotkeys::set_frontend_alive(false);
            }

            // Watch for a second launch: when another instance starts (the
            // single-instance mutex already existed) it signals the named
            // event and exits; this thread brings the main window forward
            // in response, exactly what the old single-instance plugin's
            // callback did. It is also active after a restart handoff, and
            // is a no-op when the event could not be created.
            process::start_single_instance_watcher(app.handle().clone());

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
            // split() assembles the plugin's full logger (every target and
            // filter below) WITHOUT installing it into the global log slot,
            // which early_log has owned since the top of run().
            // attach_plugin_logger fills the early logger's delegate with the
            // assembled one, so pre-Tauri records and plugin records share
            // one slot with no gap and no duplicated lines. The plugin must
            // not be built with .build() here, that would try to install a
            // second global logger and fail.
            let (log_plugin, log_max_level, log_logger) = tauri_plugin_log::Builder::default()
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
                .split(app.handle())?;
            early_log::attach_plugin_logger(log_max_level, log_logger);
            app.handle().plugin(log_plugin)?;

            // The hidden-tray confirmation is a second small Tauri WebviewWindow
            // ("flyout", see tauri.conf.json + flyout_window.rs) rendering the
            // same Svelte modal markup as the in-app confirmation. Pre-warmed
            // here (rather than lazily on first show()) so its WebView2
            // controller exists well before any display-mode change can race
            // its creation, see flyout_window::prewarm for why.
            // Load the persisted config before the flyout/tray webviews are
            // pre-warmed: their first status reads (taskbar dimmer, etc.)
            // must see the saved values, not the state defaults.
            let saved_config = config::load_config(app.handle().clone());
            if let Err(error) = tv::restore_saved(app.handle(), saved_config.tv_device.clone()) {
                log::warn!("[tv] startup restore could not start: {error}");
            }
            tray_menu_window::set_keep_tray_menu_warm(saved_config.keep_tray_menu_warm);

            // Seed the backend's Black Crush state before the screen-protector
            // worker starts. This resolves each connected monitor's saved
            // per-display profile from config (display identity + mode), so a
            // session that starts with the main webview deferred (autostart,
            // close-to-tray) still applies every monitor's Black Crush, not
            // just the primary. The frontend re-applies the same values once
            // it runs; this seed is what covers the window before it does.
            {
                let state = app.state::<gamma::GammaState>();
                gamma::seed_black_crush_from_config(&state, &saved_config);
            }

            // Seed the taskbar dimmer state from the persisted config. The
            // main window's frontend applies these to the overlay shortly
            // after launch, but the pre-warmed tray menu reads status before
            // that, without the seed it paints the default opacity (95%) on
            // the first open and then jumps to the real value. Per-monitor
            // profiles go straight into the shared profiles map; the flat
            // legacy fields seed the primary monitor so the tray menu (which
            // has no monitor selector) reads real values on first open.
            {
                // The taskbar dimmer's on/off is per monitor (it lives in
                // each DimmerProfile), so there is no global master toggle to
                // seed. The flat taskbar_dimmer_enabled field remains the
                // legacy/primary fallback and seeds the primary profile below
                // when no per-monitor entries exist yet.
                let mut profiles = crate::taskbar_dimmer::profiles_map()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                // Re-key persisted entries (legacy sigs or bare EDID ids) to
                // the monitors' current profile identities, so the runtime
                // map matches the frontend's keys before it rewrites config.
                for (key, profile) in &saved_config.taskbar_dimmer_profiles {
                    profiles.insert(crate::monitors::canonical_profile_id(key), profile.clone());
                }
                // Legacy migration: configs written before on/off became per
                // monitor had one global flat flag (taskbar_dimmer_enabled)
                // controlling every taskbar, and their per-monitor profile
                // entries carry no `enabled` field (so they deserialize to
                // off). If that legacy flag was on and no profile is on yet,
                // propagate it to every profile so an upgrade doesn't
                // silently turn off the user's dimmers. Under the new schema
                // this can't over-migrate: the frontend keeps the flat field
                // equal to the primary's enabled, so a config with all
                // profiles off would have the flat flag off too.
                if saved_config.taskbar_dimmer_enabled
                    && !profiles.values().any(|p| p.enabled)
                {
                    for profile in profiles.values_mut() {
                        profile.enabled = true;
                    }
                }
                if profiles.is_empty() {
                    // Legacy config (or first run): seed the primary monitor's
                    // profile from the flat fields so the tray menu's
                    // primary-keyed status read shows the persisted values.
                    let primary_key = crate::monitors::all_monitors()
                        .into_iter()
                        .find(|m| m.primary)
                        .map(|m| m.profile_id.clone());
                    if let Some(key) = primary_key {
                        let entry = profiles.entry(key).or_default();
                        entry.enabled = saved_config.taskbar_dimmer_enabled;
                        entry.opacity = saved_config.taskbar_dimmer_opacity.clamp(0.0, 1.0);
                        entry.fade_in_duration_ms = saved_config.taskbar_dimmer_fade_in_duration_ms;
                        entry.fade_out_duration_ms =
                            saved_config.taskbar_dimmer_fade_out_duration_ms;
                    }
                }
                // Detection Method is global, not per monitor: seed the
                // runtime statics from the persisted config so the hook and
                // tick cadence match the user's setting before any command
                // re-applies them.
                crate::taskbar_dimmer::sync_detection(
                    &saved_config.taskbar_dimmer_hover_detection,
                    saved_config.taskbar_dimmer_poll_interval_ms.clamp(16, 200) as u32,
                );
            }
            // Put the seeded profiles on screen now instead of waiting for the
            // frontend to do it. A Start on Boot launch (--start-tray) creates
            // the window hidden with NO webview, so the frontend's
            // initTaskbarDimmerRuntime never runs until the user opens the
            // window, and the dimmer was missing for the whole session
            // otherwise. No-op while every profile is off; the frontend's own
            // apply later is idempotent.
            crate::taskbar_dimmer::apply_startup();

            // Apply the persisted Explorer-hook state (Text tab) at startup
            // so the hook installs without the Text tab having to be opened
            // first. Failures are logged, never fatal: the watchdog retries
            // the install, and the Text tab re-syncs on mount.
            if let Err(error) = explorer_hook::apply_startup_state(app.handle(), &saved_config) {
                log::warn!("[explorer-hook] startup apply failed: {error}");
            }

            // Start the screen protector watcher if a timeout was persisted,
            // so the dim works even before the frontend loads its config.
            // Per-monitor profiles go straight into the shared profiles map;
            // the flat legacy fields seed the primary monitor so the
            // no-selector paths (TabPower's poll, the dim preview) read real
            // values on first launch.
            {
                let mut screen_profiles = saved_config.screen_dimmer_profiles.clone();
                if screen_profiles.is_empty() {
                    // Legacy config (or first run): seed the primary monitor's
                    // profile from the flat fields so the primary keeps the
                    // user's old settings.
                    let primary_key = crate::monitors::all_monitors()
                        .into_iter()
                        .find(|m| m.primary)
                        .map(|m| m.profile_id.clone());
                    if let Some(key) = primary_key {
                        let entry = screen_profiles.entry(key).or_default();
                        entry.seconds = saved_config.screen_protector_seconds;
                        entry.opacity = saved_config.screen_protector_opacity.clamp(0.0, 1.0);
                        entry.fade_in_duration_ms = saved_config.screen_protector_fade_in_duration_ms;
                        entry.fade_out_duration_ms =
                            saved_config.screen_protector_fade_out_duration_ms;
                        entry.fullscreen_detection = saved_config.screen_protector_fullscreen_detection;
                    }
                }
                let state = app.state::<gamma::GammaState>();
                crate::power::init(
                    saved_config.screen_protector_enabled,
                    screen_profiles,
                    state.desired.clone(),
                    state.per_monitor.clone(),
                    state.screen_dim_brightness.clone(),
                );
                // Lets the dim watcher notify open readouts the instant the
                // dim state flips (mirrors display_info::set_app_handle below).
                crate::power::set_app_handle(app.handle().clone());
            }

            // The display-info module's auto-rollback thread force-hides the
            // flyout when the confirmation window expires; it needs the app
            // handle for that (see display_info.rs).
            display_info::set_app_handle(app.handle().clone());
            flyout_window::prewarm(app.handle());
            log::info!("[tray] mode flyout window ready");

            // Custom CSS tray popup (test replacement for the native menu on
            // right-click). It is pre-warmed after the taskbar overlay is
            // created, so its initial snapshot sees the real availability.
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
            } else {
                // Publish the successful initial creation too. The tray window
                // is pre-warmed immediately afterward, but an already-mounted
                // WebView may also need to clear a transient unavailable state.
                let _ = app.emit("taskbar-dimmer-available", true);
            }

            // Custom CSS tray popup (test replacement for the native menu on
            // right-click). Only pre-warm it when the user opted into the
            // fast-first-open tradeoff. Otherwise it is created lazily on the
            // first tray-menu click and then released after the idle timeout.
            if saved_config.keep_tray_menu_warm {
                tray_menu_window::prewarm(app.handle());
            } else {
                log::info!("[tray] custom tray menu will be created on first use");
            }
            tray_menu_window::start_idle_destroyer(app.handle());
            log::info!("[tray] custom tray menu ready");

            // Keeps every window's WebView2 memory policy in step with its
            // visibility (Low target level, and a suspended renderer, while
            // hidden), see webview_env.
            webview_env::start_policy(app.handle().clone());

            // Tauri's runtime window icon (titlebar/taskbar while running) is blurry
            // on Windows, it sets HICON from raw RGBA instead of reading per-size
            // frames from icon.ico like Explorer does. Fix it manually.
            #[cfg(target_os = "windows")]
            if let Some(window) = main_window::window(app.handle()) {
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

            // A restart child (tray restart, elevated UAC relaunch) can come
            // up without the foreground: an elevated child is spawned by
            // consent.exe, so Windows refuses its window activation. Grab the
            // foreground shortly after a restart, never for plain
            // login/autostart launches where stealing focus would be wrong.
            if is_restart_child {
                let restart_app = app.handle().clone();
                std::thread::spawn(move || {
                    process::grab_foreground_after_restart(restart_app);
                });
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
                    // window.hide() on Windows reports success through the
                    // runtime's message dispatcher even when the hide did
                    // not take effect (a re-entrant close dispatch can drop
                    // the message while still returning Ok), so verify the
                    // window is actually hidden before telling the frontend.
                    // Otherwise the frontend unmounts the tab content
                    // ({#if windowVisible}) while the window is still on
                    // screen, an empty tab that never goes away.
                    let mut hidden = false;
                    if window.hide().is_ok() {
                        hidden = !window.is_visible().unwrap_or(false);
                        if !hidden {
                            // One short retry covers transient states; the
                            // hide is synchronous on the main thread, so
                            // this is cheap and invisible to the user.
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            if window.hide().is_ok() {
                                hidden = !window.is_visible().unwrap_or(false);
                            }
                        }
                    }
                    if hidden {
                        // Hidden in the tray: hand the webview's memory back.
                        webview_env::set_inactive(window.app_handle(), "main");
                        // First close-to-tray ever: tell the user the app is
                        // still running in the tray.
                        show_tray_notice(window.app_handle());
                        // The process remains alive in the tray, but
                        // release the optional vendor-driver resources
                        // while idle. This is separate from final
                        // shutdown so NVAPI can initialize lazily again
                        // when the window is shown.
                        vendor_info::release_for_tray();
                        // Tell the frontend it is hidden while it is still
                        // listening (it unmounts the tab content and drops the
                        // Text preview); the teardown below closes its webview
                        // for good, so this is the last event it receives.
                        let _ = window.app_handle().emit(
                            "window-visibility",
                            serde_json::json!({ "visible": false, "animated": false }),
                        );
                        // Hidden in the tray is the app's idle state, so this is
                        // where the webviews are torn down (see main_window):
                        // dropping them is the only way WebView2's browser and
                        // GPU processes, and the hundreds of MB they hold, are
                        // released. The window itself stays, so hotkeys, the
                        // subclass, the icon and the geometry all survive; a
                        // click on the tray icon rebuilds the webview and the
                        // frontend boots from the persisted config.
                        //
                        // On its own thread: this runs inside the close
                        // handler, and destroying the other windows plus
                        // closing the webview is not something to do re-entrant
                        // with this one's message. Hotkeys were already handed
                        // over to hotkey_actions (main_window::tear_down sets
                        // that first), so nothing depends on the frontend until
                        // it is rebuilt and reports back in.
                        let teardown_app = window.app_handle().clone();
                        std::thread::spawn(move || main_window::tear_down(&teardown_app));
                    } else {
                        // Leave the window fully visible with its content
                        // intact: far better than a blank window. The user
                        // can simply click the X again.
                        log::warn!(
                            "[window] close-to-tray hide did not take effect; leaving the window visible"
                        );
                    }
                }
                tauri::WindowEvent::Destroyed if window.label() == main_window::LABEL => {
                    // Closing the main window hides it to the tray rather than
                    // destroying it. When it is actually destroyed (real exit),
                    // request a synchronous worker shutdown so the display
                    // ramp is reset before the process exits.
                    //
                    // The label guard matters: the tray and flyout popups ARE
                    // destroyed for real when the app goes idle in the tray
                    // (see main_window::tear_down), and tearing the app's
                    // workers down with them would kill hotkeys, the dimmer and
                    // the gamma safety thread on the way into the tray.
                    taskbar_dimmer::shutdown();
                    hotkeys::shutdown();
                    power::shutdown();
                    tv::shutdown(window.app_handle());
                    shutdown_gamma_worker(window.app_handle());
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, _event| {});
}
