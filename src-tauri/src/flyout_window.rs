// flyout_window.rs, shows the "Keep display mode?" popup.
//
// This used to be a fully hand-painted native Win32 window (~700 lines of raw
// GDI: manual word-wrap, hand-rolled hit testing, a hardcoded copy of the
// modal's CSS palette in Rust constants). It is now a second small Tauri
// WebviewWindow ("flyout") that renders the exact same Svelte modal
// component/markup as the in-app confirmation (src/lib/Flyout.svelte,
// sharing app.css), centered on the primary monitor's work area and
// shown/hidden from here. There is no possibility of the flyout's style
// drifting from the in-app modal, because it IS the in-app modal's markup.
//
// The window is built lazily (on first use) with WebviewWindowBuilder rather
// than declared statically in tauri.conf.json, so builder errors are visible
// here and logged instead of a static config failing silently at startup
// with no diagnostic.
//
// `shadow(false)` is set explicitly: without it Windows draws a hairline
// border/shadow around the full window rect even with decorations off,
// visible outside .modal's own rounded edge as a thin rectangular outline.
// An earlier version of this file avoided touching `shadow` at all because
// `transparent + decorations:false + shadow:false` together was observed as
// a flaky combination that left the window "dead" (every call failing with
// FailedToReceiveMessage, nothing ever rendering), see tauri-apps/tauri#8308.
// That flakiness tracked back to two separate root causes that are now fixed
// independently (the WebView2 environment conflict from mismatched
// additionalBrowserArgs, and the initial-size transparency bug below), so
// shadow(false) no longer reproduces the dead-window failure here.
use crate::display_info::AppliedMode;
use std::sync::{mpsc, Condvar, Mutex, OnceLock};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalSize, WebviewUrl, WebviewWindowBuilder,
};

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HWND;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
};
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
#[cfg(target_os = "windows")]
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW,
};

const FLYOUT_LABEL: &str = "flyout";
// Matches Flyout.svelte's fixed .modal width (380px). Height is sized to the
// modal's actual rendered content (title + up to 3-line description + error
// row + buttons, 18px padding) rather than padded for slack: the window no
// longer carries a soft drop-shadow (see flyout.css) that needed blank
// margin to blend into, so any extra height here is now visibly empty space
// between the tray icon and the popup instead of harmless transparent
// overflow.
const FLYOUT_WIDTH: f64 = 380.0;
// The description can wrap to four or five lines depending on monitor name,
// resolution, and DPI. 190px was just enough for the shortest copy and caused
// the lower half of the modal to be clipped after some mode transitions.
const FLYOUT_HEIGHT: f64 = 230.0;

// Returns the primary monitor's full geometry. Display mode changes in this
// app target the primary display, so the flyout must use this same monitor for
// DPI and work-area positioning; using the cursor monitor caused
// lower-to-higher transitions to retain the wrong scale.
#[cfg(target_os = "windows")]
fn primary_monitor_info() -> Option<(HMONITOR, MONITORINFO)> {
    unsafe {
        let monitor = MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        Some((monitor, info))
    }
}

#[cfg(target_os = "windows")]
fn primary_monitor_scale(fallback: f64) -> f64 {
    // Read effective DPI from the monitor itself. GetDpiForWindow can retain
    // the flyout's old DPI context briefly after a mode switch, while this
    // query is independent of the hidden window's previous scale.
    for _ in 0..6 {
        if let Some((monitor, _)) = primary_monitor_info() {
            let mut dpi_x = 96u32;
            let mut dpi_y = 96u32;
            let result =
                unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
            if result.is_ok() {
                let dpi = dpi_x.max(dpi_y);
                if dpi > 0 {
                    return dpi as f64 / 96.0;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // A Tauri-reported window scale is supplied by show() as the fallback.
    fallback.max(0.5)
}

#[cfg(target_os = "windows")]
fn place_hidden_at_final_position(
    app: &AppHandle,
    width_px: i32,
    height_px: i32,
) -> Result<(i32, i32), String> {
    let window = app
        .get_webview_window(FLYOUT_LABEL)
        .ok_or_else(|| "flyout window is unavailable".to_string())?;
    let hwnd = window
        .hwnd()
        .map_err(|error| format!("could not get flyout HWND: {error}"))?;
    let (x, y) = centered_position(width_px, height_px)
        .ok_or_else(|| "could not determine primary monitor geometry".to_string())?;
    // Move and size the hidden HWND before the payload is rendered. This
    // causes Windows/WebView2 to process the target monitor's DPI while the
    // flyout is still invisible, so the first visible frame already uses the
    // final CSS scale.
    unsafe {
        SetWindowPos(
            HWND(hwnd.0),
            None,
            x,
            y,
            width_px,
            height_px,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
        .map_err(|error| format!("could not place hidden flyout: {error}"))?;
    }
    Ok((x, y))
}

#[cfg(target_os = "windows")]
fn show_at_final_position(app: &AppHandle) -> Result<(i32, i32), String> {
    let window = app
        .get_webview_window(FLYOUT_LABEL)
        .ok_or_else(|| "flyout window is unavailable".to_string())?;
    let hwnd = window
        .hwnd()
        .map_err(|error| format!("could not get flyout HWND: {error}"))?;
    // Explorer can update rcWork (the taskbar-excluded work area) after
    // rcMonitor has already settled. Re-sample the centered position here
    // and move the hidden HWND one last time before showing it. SWP_NOSIZE
    // is essential: moving within the same monitor does not change DPI, so
    // this corrects a stale first-use y coordinate without causing another
    // WebView2 text reflow.
    let current_size = window
        .outer_size()
        .map_err(|error| format!("could not read flyout size: {error}"))?;
    let (x, y) = centered_position(current_size.width as i32, current_size.height as i32)
        .ok_or_else(|| "could not determine final flyout geometry".to_string())?;
    // No SWP_NOACTIVATE on purpose: the confirmation dialog must take
    // keyboard focus so Enter (revert) and Escape (revert) work immediately,
    // like Windows' own display-mode confirm dialog. Showing with activation
    // is safe here, the flyout is only ever shown while the main window is
    // hidden in the tray (the visible-window path uses the in-app modal), so
    // no other app window loses focus it needs.
    unsafe {
        SetWindowPos(
            HWND(hwnd.0),
            None,
            x,
            y,
            0,
            0,
            SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOZORDER,
        )
        .map_err(|error| format!("could not show flyout: {error}"))?;
    }
    // Belt-and-braces: SetWindowPos activates the top-level window, but the
    // WebView child needs the actual keyboard focus for the DOM buttons to
    // receive key events. Runs after show so the window is already visible.
    let _ = window.set_focus();
    match window.outer_position() {
        Ok(pos) => Ok((pos.x, pos.y)),
        Err(error) => Err(format!("could not read shown flyout position: {error}")),
    }
}

// Centers the flyout on the primary monitor's work area. Used by both the
// in-app and tray-triggered confirmation paths, so every mode-confirmation
// popup appears in the middle of the screen instead of near the tray.
#[cfg(target_os = "windows")]
fn centered_position(width_px: i32, height_px: i32) -> Option<(i32, i32)> {
    let (_monitor_handle, info) = primary_monitor_info()?;
    let work = info.rcWork;
    let x = work.left + (work.right - work.left - width_px) / 2;
    let y = work.top + (work.bottom - work.top - height_px) / 2;
    Some((x, y))
}

#[cfg(not(target_os = "windows"))]
fn centered_position(_width_px: i32, _height_px: i32) -> Option<(i32, i32)> {
    None
}

// Waits for the post-mode-change monitor geometry to settle before the flyout
// is positioned. DEVMODE can report the requested mode as active slightly
// before Windows updates the monitor/work-area rectangles used by SetWindowPos.
// This runs on the tray worker, never the Tauri/UI thread. The expected mode
// dimensions prevent two identical samples of the *old* geometry from being
// accepted; the second identical sample confirms that the new shell geometry
// has stopped moving. A bounded fallback keeps a broken/virtual display driver
// from delaying the confirmation forever.
#[cfg(target_os = "windows")]
#[allow(dead_code)]
pub fn wait_for_display_geometry(width: u32, height: u32) -> bool {
    const MIN_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);
    const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
    const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);

    let started = std::time::Instant::now();
    std::thread::sleep(MIN_SETTLE);
    let mut previous: Option<MONITORINFO> = None;
    while started.elapsed() < TIMEOUT {
        let current = unsafe {
            let monitor = MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY);
            if monitor.is_invalid() {
                None
            } else {
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                GetMonitorInfoW(monitor, &mut info)
                    .as_bool()
                    .then_some(info)
            }
        };
        if let Some(info) = current {
            let monitor_width = (info.rcMonitor.right - info.rcMonitor.left) as u32;
            let monitor_height = (info.rcMonitor.bottom - info.rcMonitor.top) as u32;
            let matches_requested = monitor_width == width && monitor_height == height;
            let unchanged = previous
                .as_ref()
                .is_some_and(|old| old.rcMonitor == info.rcMonitor && old.rcWork == info.rcWork);
            if matches_requested && unchanged {
                log::debug!(
                    "[flyout] display geometry settled at {}x{} after {}ms",
                    monitor_width,
                    monitor_height,
                    started.elapsed().as_millis()
                );
                return true;
            }
            previous = Some(info);
        }
        std::thread::sleep(SAMPLE_INTERVAL);
    }
    log::warn!(
        "[flyout] display geometry did not settle at {}x{} within {}ms; showing flyout with the latest geometry",
        width,
        height,
        TIMEOUT.as_millis()
    );
    false
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn wait_for_display_geometry(_width: u32, _height: u32) -> bool {
    true
}

// Windows 11 applies its own DWM corner rounding to every top-level window
// (even undecorated/transparent ones), independent of anything the webview
// content draws. .modal in app.css draws its own 10px-radius rounded card,
// so without this the window frame's rounding and the modal's rounding are
// two different radii compositing on top of each other, producing a visible
// seam/corner mismatch (a thin square-cornered sliver poking out from behind
// the rounded modal). Telling DWM not to round this HWND at all leaves the
// CSS-drawn rounding as the only rounding visible, matching the in-app modal
// exactly.
#[cfg(target_os = "windows")]
fn disable_dwm_corner_rounding(app: &AppHandle) {
    let Some(window) = app.get_webview_window(FLYOUT_LABEL) else {
        return;
    };
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    unsafe {
        let preference = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            HWND(hwnd.0),
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&preference) as u32,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn disable_dwm_corner_rounding(_app: &AppHandle) {}

// Pushes the main window's HDR/SDR status to the flyout so its accent border
// matches --accent-hdr / --accent-sdr, mirroring what set_flyout_hdr used to
// do for the native window. The frontend still calls this via the
// `set_flyout_hdr` command on every HDR status change; it now just emits an
// event instead of storing an atomic for GDI painting to read. A no-op if
// the flyout window hasn't been created yet (nothing to sync).
#[tauri::command]
pub fn set_flyout_hdr(app: AppHandle, hdr: bool) {
    if let Some(window) = app.get_webview_window(FLYOUT_LABEL) {
        let _ = window.emit_to(FLYOUT_LABEL, "flyout-hdr", hdr);
    }
}

// Builds the flyout window if it doesn't already exist. Called eagerly from
// setup() (see prewarm below) so the webview exists well before it's first
// shown. Uses its own `data_directory` (a "flyout" subfolder next to the
// main window's default WebView2 profile) so it gets its own WebView2
// *environment* instead of sharing the main window's: the main window sets
// `additionalBrowserArgs` (tauri.conf.json) and the flyout does not, and
// creating a second webview with different browser args against the same
// shared environment is a known Tauri/WebView2 issue that fails with
// HRESULT 0x8007139F ("the group or resource is not in the correct state") ,
// see tauri-apps/tauri#11144. A separate data_directory gives each webview
// its own environment, avoiding the conflict entirely.
fn get_or_create_window(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    if let Some(window) = app.get_webview_window(FLYOUT_LABEL) {
        return Ok(window);
    }
    log::info!("[flyout] creating flyout window");
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map(|dir| dir.join("webview-flyout"))
        .ok();
    // Built 1px smaller than the real target size on purpose (see below).
    let mut builder =
        WebviewWindowBuilder::new(app, FLYOUT_LABEL, WebviewUrl::App("flyout.html".into()))
            .title("OLED Helper")
            .inner_size(FLYOUT_WIDTH - 1.0, FLYOUT_HEIGHT - 1.0)
            .resizable(false)
            .decorations(false)
            .transparent(true)
            .shadow(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .visible(false)
            .focused(false)
            .center();
    if let Some(dir) = data_dir {
        builder = builder.data_directory(dir);
    } else {
        log::warn!("[flyout] could not resolve app_local_data_dir; flyout will share the main window's WebView2 environment");
    }
    // This window has its own WebView2 environment (own data_directory), so
    // it can take its own browser args. Cap the HTTP cache like the main
    // window so the per-window profile folders stay bounded (~64 MB instead
    // of growing to a few hundred MB of cache each).
    builder = builder.additional_browser_args("--disk-cache-size=67108864");
    let window = builder
        .build()
        .map_err(|error| format!("could not create flyout window: {error}"))?;
    disable_dwm_corner_rounding(app);
    // WebView2's transparent background on Windows is only actually honored
    // for the region touched by a real resize after creation, the area at
    // the window's initial defined size renders opaque regardless of the
    // `transparent` flag (see tauri-apps/tauri#8133). Creating the window
    // 1px smaller than the real target size and immediately resizing up here
    // forces that one-time resize/redraw before the window is ever shown, so
    // by the time show() runs the whole surface has already been redrawn
    // transparent.
    if let Err(error) = window.set_size(LogicalSize::new(FLYOUT_WIDTH, FLYOUT_HEIGHT)) {
        log::warn!("[flyout] initial transparency-nudge resize failed: {error}");
    }
    log::info!("[flyout] flyout window created");
    Ok(window)
}

// Called once from setup(), before any display-mode change can occur, so the
// flyout's WebView2 controller is already alive by the time show() is ever
// called. show() still calls get_or_create_window() itself as a fallback in
// case prewarm failed or hasn't run yet (e.g. setup ordering changes).
pub fn prewarm(app: &AppHandle) {
    match get_or_create_window(app) {
        Ok(_) => log::info!("[flyout] flyout window pre-warmed"),
        Err(error) => log::warn!("[flyout] flyout window pre-warm failed: {error}"),
    }
    // Also warm up the DPI query itself. On a cold start, primary_monitor_scale
    // can still be racing DWM/monitor initialization for its first ~300ms
    // (retry budget), if that race is lost on the very first display-mode
    // change, size_and_position() computes width_px/height_px from a stale
    // scale factor, which shifts the centered position (and the DPI-scaled
    // size) away from the (correct) geometry used by every subsequent
    // change. Calling it once here, off any change's critical path, lets it
    // settle before it's ever load-bearing.
    #[cfg(target_os = "windows")]
    {
        let _ = primary_monitor_scale(1.0);
    }
}

// The flyout WebView is pre-warmed while hidden, but its event listeners and
// first Svelte layout are asynchronous. Showing immediately after emit can
// expose the window before the payload has been rendered (the first-use bug).
// These signals provide a one-way handshake without blocking the Tauri main
// thread: JavaScript marks the WebView ready, Rust emits the payload while the
// window is hidden, JavaScript waits for its next layout frame, and Rust then
// performs one final native size/position pass before showing.
struct FlyoutSignals {
    frontend_ready: Mutex<bool>,
    frontend_ready_changed: Condvar,
    layout_ready: Mutex<bool>,
    layout_ready_changed: Condvar,
}

static FLYOUT_SIGNALS: OnceLock<FlyoutSignals> = OnceLock::new();

fn flyout_signals() -> &'static FlyoutSignals {
    FLYOUT_SIGNALS.get_or_init(|| FlyoutSignals {
        frontend_ready: Mutex::new(false),
        frontend_ready_changed: Condvar::new(),
        layout_ready: Mutex::new(false),
        layout_ready_changed: Condvar::new(),
    })
}

#[tauri::command]
pub fn flyout_frontend_ready() {
    let signals = flyout_signals();
    if let Ok(mut ready) = signals.frontend_ready.lock() {
        *ready = true;
        signals.frontend_ready_changed.notify_all();
    }
    log::info!("[flyout] frontend handshake ready");
}

#[tauri::command]
pub fn flyout_layout_ready() {
    let signals = flyout_signals();
    if let Ok(mut ready) = signals.layout_ready.lock() {
        *ready = true;
        signals.layout_ready_changed.notify_all();
    }
    log::debug!("[flyout] rendered layout handshake ready");
}

fn reset_layout_ready() {
    let signals = flyout_signals();
    if let Ok(mut ready) = signals.layout_ready.lock() {
        *ready = false;
    }
}

fn wait_for_signal(lock: &Mutex<bool>, changed: &Condvar, timeout: std::time::Duration) -> bool {
    let guard = match lock.lock() {
        Ok(guard) => guard,
        Err(error) => error.into_inner(),
    };
    let result = changed.wait_timeout_while(guard, timeout, |ready| !*ready);
    match result {
        Ok((guard, _)) => *guard,
        Err(error) => *error.into_inner().0,
    }
}

fn wait_for_frontend_ready() -> bool {
    wait_for_signal(
        &flyout_signals().frontend_ready,
        &flyout_signals().frontend_ready_changed,
        std::time::Duration::from_secs(3),
    )
}

fn wait_for_layout_ready() -> bool {
    wait_for_signal(
        &flyout_signals().layout_ready,
        &flyout_signals().layout_ready_changed,
        std::time::Duration::from_secs(2),
    )
}

fn size_and_position(_app: &AppHandle, window: &tauri::WebviewWindow) -> (u32, u32, f64) {
    #[cfg(target_os = "windows")]
    let scale = primary_monitor_scale(window.scale_factor().unwrap_or(1.0));
    #[cfg(not(target_os = "windows"))]
    let scale = window.scale_factor().unwrap_or(1.0);
    let width_px = (FLYOUT_WIDTH * scale).round().max(1.0) as u32;
    let height_px = (FLYOUT_HEIGHT * scale).round().max(1.0) as u32;
    if let Err(error) = window.set_size(PhysicalSize::new(width_px, height_px)) {
        log::warn!("[flyout] set_size failed: {error}");
    }
    (width_px, height_px, scale)
}

// Runs on the Tauri main thread. The window remains hidden while this emits
// the mode, so the asynchronous Svelte update cannot be seen by the user.
fn prepare(app: &AppHandle, mode: &AppliedMode) -> Result<(), String> {
    let mut window = get_or_create_window(app)?;
    if window.outer_position().is_err() {
        log::warn!("[flyout] existing flyout window is unresponsive; rebuilding");
        let _ = window.destroy();
        window = get_or_create_window(app)?;
    }
    reset_layout_ready();
    let (width_px, height_px, _) = size_and_position(app, &window);
    #[cfg(target_os = "windows")]
    place_hidden_at_final_position(app, width_px as i32, height_px as i32)?;
    window
        .emit_to(FLYOUT_LABEL, "flyout-show", mode)
        .map_err(|error| format!("could not notify flyout window: {error}"))
}

// The hidden native placement happens in prepare() before Svelte renders.
// finish_show only changes visibility, so no DPI/size reflow is introduced
// after the flyout becomes visible. Emits "flyout-shown" right after the
// native window actually becomes visible, so the frontend can start its
// entrance transition exactly when the user can see it, starting it any
// earlier (e.g. on mount/mode-set) would let it finish playing while the
// window is still hidden behind SetWindowPos.
fn finish_show(app: &AppHandle) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let (x, y) = show_at_final_position(app)?;
    #[cfg(not(target_os = "windows"))]
    let (x, y) = {
        let window = get_or_create_window(app)?;
        window
            .show()
            .map_err(|error| format!("could not show flyout window: {error}"))?;
        match window.outer_position() {
            Ok(pos) => (pos.x, pos.y),
            Err(_) => (0, 0),
        }
    };
    if let Some(window) = app.get_webview_window(FLYOUT_LABEL) {
        let _ = window.emit_to(FLYOUT_LABEL, "flyout-shown", ());
        // Tell the taskbar dimmer overlay this HWND is now visible and
        // topmost, so its periodic z-order repair inserts itself just
        // behind the flyout instead of in front of it (see
        // taskbar_dimmer.rs's active_flyout_window for why, HWND_TOPMOST
        // otherwise lets the dimmer's continuous repair cover the flyout
        // entirely, even though the flyout itself is also topmost).
        #[cfg(target_os = "windows")]
        if let Ok(hwnd) = window.hwnd() {
            crate::taskbar_dimmer::set_flyout_hwnd(hwnd.0 as isize);
        }
    }
    log::info!("[flyout] shown at ({x}, {y}) without a visible DPI resize");
    Ok(())
}

// Full hidden-WebView handshake. This is called from the tray worker, not the
// Tauri/UI thread, so waiting for JavaScript never freezes event processing.
pub fn show_after_ready(app: &AppHandle, mode: AppliedMode) -> Result<(), String> {
    log::debug!(
        "[flyout] show_after_ready called for {}x{}@{} from thread {:?}",
        mode.width,
        mode.height,
        mode.refresh_hz,
        std::thread::current().id()
    );
    if !wait_for_frontend_ready() {
        log::warn!("[flyout] frontend handshake timed out; attempting legacy hidden show");
    }
    let prepare_app = app.clone();
    let prepare_mode = mode.clone();
    let (prepare_done, prepare_result) = mpsc::channel();
    app.run_on_main_thread(move || {
        let result = prepare(&prepare_app, &prepare_mode);
        let _ = prepare_done.send(result);
    })
    .map_err(|error| format!("could not schedule hidden flyout prepare: {error}"))?;
    match prepare_result.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(error) => {
            return Err(format!("hidden flyout prepare timed out: {error}"));
        }
    }

    if !wait_for_layout_ready() {
        log::warn!("[flyout] layout handshake timed out; finalizing with current hidden layout");
    }
    let finish_app = app.clone();
    let (finish_done, finish_result) = mpsc::channel();
    app.run_on_main_thread(move || {
        let result = finish_show(&finish_app);
        let _ = finish_done.send(result);
    })
    .map_err(|error| format!("could not schedule flyout show: {error}"))?;
    match finish_result.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(result) => result,
        Err(error) => Err(format!("flyout show timed out: {error}")),
    }
}

// Forced-hide path. The frontend normally hides the window itself after
// confirm/rollback, but this is the belt-and-braces fallback so a failed or
// blank frontend hide can never leave a transparent always-on-top window
// blocking clicks on the main window. Called by the display-mode IPC
// confirm/rollback commands and the 15-second auto-rollback thread.
//
// The native SetWindowPos(SWP_HIDEWINDOW) call comes FIRST, mirroring how
// the show path uses native SetWindowPos(SWP_SHOWWINDOW): the Tauri
// window.hide() API routes through the WebView2 controller, which can be
// wedged ("dead window", FailedToReceiveMessage) exactly when a stuck flyout
// needs hiding, and a silent failure would leave the transparent window
// visible and click-blocking. Hiding the raw HWND works regardless of the
// webview's state. window.hide() still runs after it as belt and braces so
// the webview's own visibility bookkeeping matches.
pub fn hide(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(FLYOUT_LABEL) {
        #[cfg(target_os = "windows")]
        if let Ok(hwnd) = window.hwnd() {
            unsafe {
                let _ = SetWindowPos(
                    HWND(hwnd.0),
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_HIDEWINDOW | SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        let _ = window.hide();
    }
    crate::taskbar_dimmer::set_flyout_hwnd(0);
    log::debug!("[flyout] hidden");
}

// Called by the frontend (confirm()/rollback() in Flyout.svelte) right after
// it hides the window itself. The frontend owns the hide() call for the
// normal confirm/rollback flow, so this only clears the dimmer's
// flyout-priority pointer, without it, the dimmer overlay would keep
// yielding z-order to a flyout HWND that is no longer showing, which is
// harmless (active_flyout_window checks IsWindowVisible) but leaves a stale
// pointer around until the next show. Clearing it here keeps the state tidy.
// Also emits a flyout-completed event to the main window so the in-app UI
// can clear its busy state and re-poll display info.
#[tauri::command]
pub fn flyout_hidden(app: AppHandle) {
    crate::taskbar_dimmer::set_flyout_hwnd(0);
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.emit("flyout-completed", ());
    }
}

// Shows the flyout confirmation dialog centered on screen. Called from the
// in-app resolution/refresh-rate dropdowns. The mode data is forwarded to
// the flyout's Svelte frontend via the standard flyout-show event.
//
// This command must NOT run the show_after_ready handshake on its own thread:
// it's a sync command, so it executes on the Tauri main thread, and
// show_after_ready blocks waiting for work it schedules back onto that same
// main thread (run_on_main_thread + channel recv), a guaranteed deadlock
// until the channel timeout. Spawn a worker thread so the handshake behaves
// exactly like the tray path always did.
#[tauri::command]
pub fn show_mode_confirmation(
    app: AppHandle,
    width: u32,
    height: u32,
    refresh_hz: u32,
    monitor_name: Option<String>,
    confirmation_seconds: Option<u32>,
) -> Result<(), String> {
    let mode = AppliedMode {
        width,
        height,
        refresh_hz,
        monitor_name: monitor_name.unwrap_or_default(),
        confirmation_seconds: confirmation_seconds.unwrap_or(15),
    };
    std::thread::spawn(move || {
        if let Err(error) = show_after_ready(&app, mode) {
            log::error!("[flyout] mode confirmation failed to show: {error}");
        }
    });
    log::debug!(
        "[flyout] show_mode_confirmation accepted {}x{}@{} and returned",
        width,
        height,
        refresh_hz
    );
    Ok(())
}
