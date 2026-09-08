// tray_menu_window.rs, custom CSS-styled tray popup (replaces the native
// muda menu from tray_menu.rs for the right-click path, as a test).
//
// Built the same way as flyout_window.rs: a small undecorated/transparent
// Tauri WebviewWindow rendering real Svelte/CSS instead of a Win32
// TrackPopupMenu, anchored flush against the taskbar edge near the tray
// icon using the same Win32 SetWindowPos approach as flyout_window.rs's
// taskbar_position, rather than tauri-plugin-positioner's TrayCenter (which
// only centers on the tray icon at a fixed offset, not flush to the edge).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, State, WebviewUrl, WebviewWindowBuilder};

#[derive(Debug, Deserialize, Clone, Copy)]
pub struct TrayMenuRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    // CSS pixels are converted against the live DOM viewport and the native
    // client size. This remains correct through a Sunshine or DPI transition,
    // even if devicePixelRatio itself is briefly stale.
    #[serde(default = "default_tray_viewport")]
    pub viewport_width: f64,
    #[serde(default = "default_tray_viewport")]
    pub viewport_height: f64,
}

fn default_tray_viewport() -> f64 {
    0.0
}

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HWND;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
};
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRoundRectRgn, DeleteObject, GetMonitorInfoW, MonitorFromWindow, SetWindowRgn,
    HMONITOR, HRGN, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, RGN_ERROR, RGN_OR,
};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
};

const LABEL: &str = "tray-menu";

#[derive(Serialize)]
pub struct TrayMenuState {
    pub display: Option<crate::display_info::DisplayInfo>,
    pub config: crate::config::AppConfig,
    pub taskbar_dimmer: crate::taskbar_dimmer::TaskbarDimmerStatus,
    pub screen_dimmer: crate::power::ScreenDimmerStatus,
    pub helper_ready: bool,
    pub hdr_status: Option<crate::hdr_toggle::HdrCapability>,
    pub vendor_info: crate::vendor_info::VendorDisplayInfo,
    pub auto_hdr_enabled: bool,
    pub rtx_hdr_settings: Option<crate::vendor_info::RtxHdrSettings>,
    pub sdr_brightness: Option<crate::hdr_toggle::SdrWhiteLevel>,
    pub hotkey_target_label: String,
}

static SUBMENU_EXPANDED: AtomicBool = AtomicBool::new(false);
static MENU_GENERATION: AtomicU64 = AtomicU64::new(0);
static LAST_SHOW_AT_MS: AtomicU64 = AtomicU64::new(0);
static STATE_REFRESH_GENERATION: AtomicU64 = AtomicU64::new(0);
static STATE_REFRESH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static STATE_REFRESH_PENDING: AtomicBool = AtomicBool::new(false);
static CACHED_TRAY_STATE: OnceLock<Mutex<Option<Value>>> = OnceLock::new();
// The resolution submenu includes the same friendly/UW/native labels as the
// main app, so reserve enough left-side space to keep those tags readable.
// The fixed canvas is deliberately larger than the visible cards. The cards
// stay bottom-aligned, while the frontend narrows the native hit region after
// layout has settled.
//
// Keeping the canvas fixed avoids moving the HWND while the pointer is over
// the menu. A generous height also leaves room for the dynamic SDR row and
// inline favorites after a DPI or Sunshine transition.
const FULL_WIDTH: f64 = 618.0;
const FULL_HEIGHT: f64 = 620.0;
// The CSS tray card uses the global border-box rule, so its declared width is
// also its rendered outer width.
const BASE_WIDTH: f64 = 270.0;
// The window is built once at this full footprint (base menu + resolution
// submenu both visible) and never resized while shown; CSS/Svelte alone
// show or hide the submenu pane inside that fixed canvas. An earlier
// version resized a *live* window on hover to hug the collapsed size,
// but that relocates the OS window rect out from under the cursor mid-
// interaction: set_size() + reposition happen asynchronously (a Tauri IPC
// round-trip from a mouseenter handler), so there's a window of time where
// the browser's hover state still reflects the pre-move geometry, leaving
// a stale dead zone that doesn't track pixels until the next real
// mousemove. Keeping the window fixed avoids that desync entirely.

// CSS transparency only affects WebView painting; Windows would still route
// mouse input to every pixel of the top-level HWND. Keep the HWND fixed for
// stable positioning, but apply a native region around the measured cards so
// the transparent canvas outside them is actually click-through.
#[cfg(target_os = "windows")]
fn apply_hit_region(window: &tauri::WebviewWindow, rects: &[TrayMenuRect]) {
    let Ok(hwnd) = window.hwnd() else { return };
    let native_size = window.inner_size().ok();
    let first = rects.first();
    let fallback_scale = window.scale_factor().unwrap_or(1.0).max(0.5);
    let scale_x = first
        .and_then(|rect| {
            (rect.viewport_width.is_finite()
                && rect.viewport_width > 0.0
                && native_size.is_some())
                .then(|| native_size.unwrap().width as f64 / rect.viewport_width)
        })
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(fallback_scale);
    let scale_y = first
        .and_then(|rect| {
            (rect.viewport_height.is_finite()
                && rect.viewport_height > 0.0
                && native_size.is_some())
                .then(|| native_size.unwrap().height as f64 / rect.viewport_height)
        })
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(fallback_scale);
    // Add a small CSS-pixel safety margin. The WebView and native window can
    // round a fractional DPI boundary differently, and clipping one row is
    // worse than allowing a few transparent pixels into the native region.
    let padding_x = (2.0 * scale_x).ceil() as i32;
    let padding_y = (2.0 * scale_y).ceil() as i32;
    let mut regions: Vec<HRGN> = rects
        .iter()
        .filter_map(|rect| {
            let left = ((rect.x * scale_x).round() as i32 - padding_x).max(0);
            let top = ((rect.y * scale_y).round() as i32 - padding_y).max(0);
            let right = ((rect.x + rect.width) * scale_x).round() as i32 + padding_x;
            let bottom = ((rect.y + rect.height) * scale_y).round() as i32 + padding_y;
            if right <= left || bottom <= top {
                return None;
            }
            let radius = (10.0 * scale_x.min(scale_y)).round().max(1.0) as i32;
            let region = unsafe { CreateRoundRectRgn(left, top, right, bottom, radius, radius) };
            (!region.0.is_null()).then_some(region)
        })
        .collect();
    let Some(combined) = regions.pop() else {
        return;
    };

    unsafe {
        for source in regions {
            let result = CombineRgn(Some(combined), Some(combined), Some(source), RGN_OR);
            let _ = DeleteObject(source.into());
            if result == RGN_ERROR {
                let _ = DeleteObject(combined.into());
                return;
            }
        }
        // SetWindowRgn transfers ownership to Windows on success. Only delete
        // the handle on failure, where the caller still owns it.
        if SetWindowRgn(HWND(hwnd.0), Some(combined), true) == 0 {
            let _ = DeleteObject(combined.into());
        }
    }
}

#[cfg(target_os = "windows")]
fn apply_default_hit_region(window: &tauri::WebviewWindow) {
    let (width, height) = window
        .outer_size()
        .map(|size| (size.width.max(1) as f64, size.height.max(1) as f64))
        .unwrap_or((FULL_WIDTH, FULL_HEIGHT));
    apply_hit_region(
        window,
        &[TrayMenuRect {
            x: 0.0,
            y: 0.0,
            width,
            height,
            viewport_width: 0.0,
            viewport_height: 0.0,
        }],
    );
}

#[cfg(not(target_os = "windows"))]
fn apply_default_hit_region(_window: &tauri::WebviewWindow) {}

#[cfg(target_os = "windows")]
fn disable_dwm_corner_rounding(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    let Ok(hwnd) = window.hwnd() else { return };
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

// Returns the primary monitor's full geometry (used to find the taskbar
// edge, mirroring flyout_window.rs's primary_monitor_info).
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

// Positions the tray menu flush against the taskbar edge, near the tray icon
// corner, the same way flyout_window.rs anchors the mode-confirmation popup.
// The taskbar edge is derived from the monitor's work area (rcWork =
// rcMonitor minus the taskbar strip): the edge where the two differ is where
// the taskbar sits. Called on each anchor pass when the menu opens (see
// toggle below); the window is never resized while visible; the final
// post-show pass only re-applies the position, which is what corrects the
// DPI-transition shift after a mode switch.
//
// anchor_x_px, when given, is the tray icon's click x-position in physical
// pixels (from the TrayIconEvent), the visible base-menu card (which sits
// flush against the window's right edge, see tray-menu.css's row-reverse
// layout and apply_default_hit_region's BASE_WIDTH offset) is centered on
// that x instead of the window always hugging the work area's right edge.
// card_width_px is the base menu card's width, so the card itself ends up
// centered on the icon rather than the invisible full window (which is
// wider, to also fit the resolution submenu) being centered on it, that
// would visually offset the card away from the icon.
#[cfg(target_os = "windows")]
fn taskbar_position(
    width_px: i32,
    height_px: i32,
    anchor_x_px: Option<i32>,
    card_width_px: i32,
) -> Option<(i32, i32)> {
    let (_monitor_handle, info) = primary_monitor_info()?;
    let work = info.rcWork;
    let monitor = info.rcMonitor;
    let gap = 6;
    let max_x = (work.right - width_px).max(work.left);
    let max_y = (work.bottom - height_px).max(work.top);
    // When an anchor x is available and the taskbar runs along the top or
    // bottom edge (the horizontal case where a left/right x position is
    // meaningful), center the visible base-menu card on it instead of
    // pinning the window to the work area's right edge. Vertical taskbars
    // (left/right edge) keep the existing bottom-aligned behavior, since the
    // icon's y position, not x, is what varies there. The card sits at the
    // window's right edge, so window_x = desired_card_x, (width_px -
    // card_width_px).
    let horizontal_taskbar = monitor.bottom - work.bottom > 0 || work.top - monitor.top > 0;
    let anchored_x = anchor_x_px.filter(|_| horizontal_taskbar).map(|anchor_x| {
        let card_x = anchor_x - card_width_px / 2;
        let window_x = card_x - (width_px - card_width_px);
        window_x.clamp(work.left, max_x)
    });
    let (x, y) = if monitor.bottom - work.bottom > 0 {
        // Taskbar along the bottom edge, pin just above it, centered on the
        // tray icon's x position when known, otherwise right-aligned near
        // the tray/notification area.
        (
            anchored_x.unwrap_or((work.right - width_px - gap).max(work.left)),
            (work.bottom - height_px - gap).max(work.top),
        )
    } else if work.top - monitor.top > 0 {
        // Taskbar along the top edge, just below it, centered on the tray
        // icon's x position when known, otherwise near the tray on the right.
        (
            anchored_x.unwrap_or((work.right - width_px - gap).max(work.left)),
            (work.top + gap).min(max_y),
        )
    } else if work.left - monitor.left > 0 {
        // Taskbar along the left edge, just right of it, near the tray.
        (
            (work.left + gap).min(max_x),
            (work.bottom - height_px - gap).max(work.top),
        )
    } else if monitor.right - work.right > 0 {
        // Taskbar along the right edge, just left of it, near the tray.
        (
            (work.right - width_px - gap).max(work.left),
            (work.bottom - height_px - gap).max(work.top),
        )
    } else {
        // No taskbar on the primary monitor; fall back to the lower-right
        // corner of the work area.
        (
            (work.right - width_px - gap).max(work.left),
            (work.bottom - height_px - gap).max(work.top),
        )
    };
    Some((x, y))
}

#[cfg(not(target_os = "windows"))]
fn taskbar_position(
    _width_px: i32,
    _height_px: i32,
    _anchor_x_px: Option<i32>,
    _card_width_px: i32,
) -> Option<(i32, i32)> {
    None
}

// Re-anchors the window to the taskbar edge. Called when the menu opens
// (see toggle below). anchor_x_px is the tray icon's click x-position in
// physical pixels, used to center the visible base-menu card on the icon
// when the taskbar runs horizontally (see taskbar_position). scale is the
// primary monitor's effective DPI scale factor, resolved ONCE per open by
// toggle() and shared by both passes: the monitor's DPI is stable within
// one open, so re-querying it per pass only multiplied the transient
// GetDpiForMonitor failures after a display change into visible blocking.
//
// The window's own scale factor can lag the monitor's after a display-mode
// switch (the first open after a resolution change landed at the wrong y for
// exactly this reason), so the physical size is derived from the monitor's
// effective DPI instead of the window's possibly-stale DPI context.
//
// resize=true is the sizing pass: it derives the physical footprint from
// the monitor's fresh DPI and applies it natively while hidden (set_size
// alone only updates Tauri's bookkeeping; the native SetWindowPos is what
// actually moves/resizes the HWND). resize=false is a reposition-only pass
// (SWP_NOSIZE) that re-samples the taskbar position; Explorer can update
// rcWork (the taskbar-excluded work area) after rcMonitor has already
// settled, so the first open after a mode switch can otherwise land a few
// dozen px off.
//
// IMPORTANT (learned from the diagnostic logs): the hidden window's DPI
// context is NOT updated by these passes. Windows defers a hidden window's
// DPI transition until the window is shown, and applies it by rescaling the
// window's pre-change geometry *during* show(), which is why the first
// open after a resolution change used to land at the old y scaled by the
// DPI ratio (e.g. y=711 instead of 959 in the logs). toggle() therefore
// calls this with resize=false once more AFTER show(), when the transition
// is complete and the position sticks.
#[cfg(target_os = "windows")]
fn anchor_to_taskbar(window: &tauri::WebviewWindow, anchor_x_px: Option<i32>) {
    let Ok(hwnd) = window.hwnd() else { return };
    let size = match window.outer_size() {
        Ok(size) => size,
        Err(_) => return,
    };
    let width_px = size.width.max(1);
    let height_px = size.height.max(1);
    // The CSS card is border-box, so its rendered outer width is BASE_WIDTH.
    // The wider fixed canvas reserves the submenu on the left.
    let card_width_px = ((width_px as f64 * BASE_WIDTH / FULL_WIDTH).round() as i32).max(1);
    let Some((x, y)) = taskbar_position(
        width_px as i32,
        height_px as i32,
        anchor_x_px,
        card_width_px,
    ) else {
        return;
    };
    unsafe {
        let _ = SetWindowPos(
            HWND(hwnd.0),
            None,
            x,
            y,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSIZE,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn anchor_to_taskbar(_window: &tauri::WebviewWindow, _anchor_x_px: Option<i32>) {}

fn get_or_create_window(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    log::info!("[tray-debug] get_or_create_window begin");
    if let Some(window) = app.get_webview_window(LABEL) {
        let visible = window.is_visible().unwrap_or(false);
        log::info!("[tray-debug] existing window found, visible={visible}");
        // Reassert the fixed logical canvas on every open. This repairs a
        // native size retained across a remote-display session without ever
        // resizing the window while it is visible.
        if !window.is_visible().unwrap_or(false) {
            let _ = window.set_size(LogicalSize::new(FULL_WIDTH, FULL_HEIGHT));
        }
        return Ok(window);
    }
    log::info!("[tray-menu] creating tray menu window");
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map(|dir| dir.join("webview-tray-menu"))
        .ok();
    let mut builder =
        WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("tray-menu.html".into()))
            .title("OLED Helper")
            .inner_size(FULL_WIDTH - 1.0, FULL_HEIGHT - 1.0)
            .resizable(false)
            .decorations(false)
            .transparent(true)
            .shadow(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .visible(false)
            .focused(false);
    if let Some(dir) = data_dir {
        builder = builder.data_directory(dir);
    }
    // This window has its own WebView2 environment (own data_directory), so
    // it can take its own browser args. Cap the HTTP cache like the main
    // window so the per-window profile folders stay bounded (~64 MB instead
    // of growing to a few hundred MB of cache each).
    builder = builder.additional_browser_args("--disk-cache-size=67108864");
    log::info!("[tray-debug] building tray window");
    let window = builder
        .build()
        .map_err(|e| format!("could not create tray menu window: {e}"))?;
    log::info!("[tray-debug] tray window built");
    disable_dwm_corner_rounding(app);
    // Transparency-nudge resize, WebView2's transparent background is only
    // honored after a real post-creation resize (see flyout_window.rs).
    if let Err(e) = window.set_size(LogicalSize::new(FULL_WIDTH, FULL_HEIGHT)) {
        log::warn!("[tray-menu] initial resize failed: {e}");
    }
    // Start with a safe initial region before the frontend has measured its
    // cards. The first Svelte layout replaces this estimate with exact DOM
    // rectangles through tray_menu_set_expanded.
    //
    // Use the full fixed canvas as the initial region only as a fallback shape
    // for the base menu, not as a claim that the submenu is already open. Once
    // Svelte reports the real base card, the exact smaller region replaces it,
    // and the submenu region is added only when it is actually expanded.
    apply_default_hit_region(&window);
    // Hide on blur, like a normal context menu dismissing on outside click.
    let blur_window = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Focused(false) = event {
            log::info!("[tray-debug] native focus lost, generation={}", MENU_GENERATION.load(Ordering::Acquire));
            // The native region is updated asynchronously when a submenu
            // opens. Delay the blur dismissal long enough for the preinstalled
            // bridge/exact region to arrive; if the submenu is still open,
            // the pointer crossed internally and the popup must remain visible.
            // Capture the menu generation so an old blur callback can never
            // hide a newly reopened tray menu. Kept short (60ms) since there
            // is no exit animation to wait for; the window hides instantly
            // once this guard clears, so a long delay only reads as sluggish.
            let generation = MENU_GENERATION.load(Ordering::Acquire);
            let delayed_window = blur_window.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(80));
                if MENU_GENERATION.load(Ordering::Acquire) == generation
                    && LAST_SHOW_AT_MS.load(Ordering::Acquire).saturating_add(250)
                        <= now_ms()
                    && !SUBMENU_EXPANDED.load(Ordering::Acquire)
                {
                    log::info!("[tray-debug] delayed blur hiding generation={generation}");
                    let _ = delayed_window.hide();
                } else {
                    log::info!("[tray-debug] delayed blur ignored generation={generation}, current_generation={}, submenu_expanded={}, age_ms={}", MENU_GENERATION.load(Ordering::Acquire), SUBMENU_EXPANDED.load(Ordering::Acquire), now_ms().saturating_sub(LAST_SHOW_AT_MS.load(Ordering::Acquire)));
                }
            });
        }
    });
    Ok(window)
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
pub fn prewarm(app: &AppHandle) {
    log::info!("[tray-debug] prewarm begin");
    if let Err(e) = get_or_create_window(app) {
        log::warn!("[tray-menu] prewarm failed: {e}");
        return;
    }
    // Build the first snapshot while the tray window is still hidden. Later
    // refreshes are event-driven, so the click path never performs these reads.
    let taskbar_state = app.state::<crate::taskbar_dimmer::TaskbarDimmerState>();
    let snapshot = collect_tray_menu_state(app, taskbar_state);
    log::info!("[tray-debug] prewarm snapshot collected");
    let payload = serde_json::to_value(&snapshot).unwrap_or(Value::Null);
    if let Ok(mut cached) = CACHED_TRAY_STATE.get_or_init(|| Mutex::new(None)).lock() {
        *cached = Some(payload);
        log::info!("[tray-debug] prewarm cache populated");
    } else {
        log::warn!("[tray-debug] prewarm cache lock failed");
    }
}

/// Show (or hide, if already visible) the custom tray menu, anchored flush
/// against the taskbar edge near the tray icon. anchor_x is the click's
/// physical-pixel x-coordinate from the TrayIconEvent, used to center the
/// popup on wherever the tray icon actually sits.
fn schedule_post_show_event(
    window: &tauri::WebviewWindow,
    payload: Value,
    generation: u64,
) {
    let delayed_window = window.clone();
    std::thread::spawn(move || {
        for delay_ms in [30_u64, 120, 300] {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            if MENU_GENERATION.load(Ordering::Acquire) != generation
                || !delayed_window.is_visible().unwrap_or(false)
            {
                return;
            }
            log::info!("[tray-debug] show retry generation={generation}, delay_ms={delay_ms}, visible={}", delayed_window.is_visible().unwrap_or(false));
            match delayed_window.emit_to(LABEL, "tray-menu-show", payload.clone()) {
                Ok(()) => log::info!("[tray-debug] show retry emitted generation={generation}"),
                Err(error) => log::warn!("[tray-debug] show retry emit failed generation={generation}: {error}"),
            }
        }
    });
}/// Re-anchor the already-visible window while Windows finishes applying a
/// display or DPI transition.
fn schedule_post_show_anchor(
    window: &tauri::WebviewWindow,
    anchor_x_px: Option<i32>,
    generation: u64,
) {
    let delayed_window = window.clone();
    std::thread::spawn(move || {
        for delay_ms in [50_u64, 150, 350, 700] {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            if MENU_GENERATION.load(Ordering::Acquire) != generation
                || !delayed_window.is_visible().unwrap_or(false)
            {
                return;
            }
            anchor_to_taskbar(&delayed_window, anchor_x_px);
        }
    });
}

pub fn toggle(app: &AppHandle, anchor_x: f64) {
    log::info!("[tray-debug] toggle begin anchor_x={anchor_x:.1}");
    let window = match get_or_create_window(app) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("[tray-menu] could not open tray menu: {e}");
            return;
        }
    };
    MENU_GENERATION.fetch_add(1, Ordering::AcqRel);
    let visible = window.is_visible().unwrap_or(false);
    log::info!("[tray-debug] toggle generation={}, visible_before={visible}", MENU_GENERATION.load(Ordering::Acquire));
    if visible {
        log::info!("[tray-debug] hiding already-visible tray window");
        match window.emit_to(LABEL, "tray-menu-hide", ()) {
            Ok(()) => log::info!("[tray-debug] hide event emitted"),
            Err(error) => log::warn!("[tray-debug] hide event emit failed: {error}"),
        }
        match window.hide() {
            Ok(()) => log::info!("[tray-debug] native hide succeeded"),
            Err(error) => log::warn!("[tray-debug] native hide failed: {error}"),
        }
        return;
    }
    SUBMENU_EXPANDED.store(false, Ordering::Release);
    LAST_SHOW_AT_MS.store(now_ms(), Ordering::Release);
    // A click outside the region can hide the window before Svelte gets a
    // chance to close its submenu state. Reset the native region before the
    // next show so a stale expanded shape can never intercept the first frame.
    apply_default_hit_region(&window);
    SUBMENU_EXPANDED.store(false, Ordering::Release);
    let payload = cached_tray_state();
    log::info!("[tray-debug] cache lookup complete, has_payload={}", payload.is_some());
    let payload = payload.unwrap_or(Value::Null);
    match window.emit_to(LABEL, "tray-menu-show", payload.clone()) {
        Ok(()) => log::info!("[tray-debug] initial show event emitted"),
        Err(error) => log::warn!("[tray-debug] initial show event emit failed: {error}"),
    }
    let anchor = Some(anchor_x.round() as i32);
    anchor_to_taskbar(&window, anchor);
    match window.show() {
        Ok(()) => log::info!("[tray-debug] native show succeeded, visible_after={}", window.is_visible().unwrap_or(false)),
        Err(error) => log::warn!("[tray-debug] native show failed: {error}"),
    }
    anchor_to_taskbar(&window, anchor);
    let generation = MENU_GENERATION.load(Ordering::Acquire);
    schedule_post_show_event(&window, payload, generation);
    schedule_post_show_anchor(&window, anchor, generation);
    match window.set_focus() {
        Ok(()) => log::info!("[tray-debug] focus request succeeded"),
        Err(error) => log::warn!("[tray-debug] focus request failed: {error}"),
    }
    log::info!("[tray-debug] toggle complete generation={generation}, visible_after={}", window.is_visible().unwrap_or(false));
}

pub fn hide(app: &AppHandle) {
    log::info!("[tray-debug] hide requested");
    if let Some(window) = app.get_webview_window(LABEL) {
        match window.emit_to(LABEL, "tray-menu-hide", ()) {
            Ok(()) => log::info!("[tray-debug] hide event emitted"),
            Err(error) => log::warn!("[tray-debug] hide event emit failed: {error}"),
        }
        match window.hide() {
            Ok(()) => log::info!("[tray-debug] native hide succeeded"),
            Err(error) => log::warn!("[tray-debug] native hide failed: {error}"),
        }
    } else {
        log::warn!("[tray-debug] hide requested but window does not exist");
    }
}

fn tray_primary_key(display: Option<&crate::display_info::DisplayInfo>) -> Option<String> {
    display
        .and_then(|info| {
            let key = if info.profile_id.is_empty() {
                &info.monitor_id
            } else {
                &info.profile_id
            };
            (!key.is_empty()).then(|| key.to_string())
        })
}

fn collect_tray_menu_state(
    app: &AppHandle,
    taskbar_state: State<'_, crate::taskbar_dimmer::TaskbarDimmerState>,
) -> TrayMenuState {
    let display = tray_menu_get_display_info().ok();
    let monitor_key = tray_primary_key(display.as_ref());
    let config = crate::config::load_config(app.clone());
    let taskbar_dimmer = crate::taskbar_dimmer::get_taskbar_dimmer_status(
        taskbar_state,
        monitor_key.clone(),
    );
    let screen_dimmer = crate::power::power_get_screen_dimmer_status(monitor_key);
    let hdr_status = crate::hdr_toggle::get_windows_hdr_status().ok().flatten();
    let vendor_info = crate::vendor_info::query_vendor_display_info();
    let auto_hdr_enabled = crate::hdr_toggle::get_auto_hdr_enabled().unwrap_or(false);
    let rtx_hdr_settings = crate::vendor_info::get_rtx_hdr_settings_state().ok();
    let sdr_brightness = crate::hdr_toggle::get_sdr_white_level().ok().flatten();
    let hotkey_target_label = {
        let monitors = crate::monitors::all_monitors();
        let selected = monitors
            .iter()
            .find(|monitor| !config.active_monitor_sig.is_empty() && monitor.sig() == config.active_monitor_sig)
            .or_else(|| monitors.iter().find(|monitor| monitor.primary));
        selected
            .map(|monitor| {
                if monitor.friendly_name.is_empty() {
                    "Primary monitor".to_string()
                } else {
                    monitor.friendly_name.clone()
                }
            })
            .unwrap_or_default()
    };
    TrayMenuState {
        display,
        config,
        taskbar_dimmer,
        screen_dimmer,
        helper_ready: crate::helper::helper_task_registered(),
        hdr_status,
        vendor_info,
        auto_hdr_enabled,
        rtx_hdr_settings,
        sdr_brightness,
        hotkey_target_label,
    }
}

#[tauri::command]
pub fn tray_menu_debug(message: String) {
    log::info!("[tray-debug] frontend {message}");
}

#[tauri::command]
pub fn tray_menu_get_state(
    app: AppHandle,
    state: State<'_, crate::taskbar_dimmer::TaskbarDimmerState>,
) -> TrayMenuState {
    collect_tray_menu_state(&app, state)
}

fn cached_tray_state() -> Option<Value> {
    CACHED_TRAY_STATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|state| state.clone())
}

/// Refresh the complete tray snapshot in response to a native event. The
/// expensive display and driver reads stay off the tray click path.
pub fn refresh_cached_state(app: &AppHandle) {
    if STATE_REFRESH_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        STATE_REFRESH_PENDING.store(true, Ordering::Release);
        return;
    }
    let generation = STATE_REFRESH_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let refresh_app = app.clone();
    std::thread::spawn(move || {
        let taskbar_state = refresh_app.state::<crate::taskbar_dimmer::TaskbarDimmerState>();
        let snapshot = collect_tray_menu_state(&refresh_app, taskbar_state);
        let payload = serde_json::to_value(&snapshot).unwrap_or(Value::Null);
        if STATE_REFRESH_GENERATION.load(Ordering::Acquire) == generation {
            if let Ok(mut cached) = CACHED_TRAY_STATE
                .get_or_init(|| Mutex::new(None))
                .lock()
            {
                *cached = Some(payload.clone());
            }
            let _ = refresh_app.emit("tray-menu-state-updated", payload);
        }
        STATE_REFRESH_IN_FLIGHT.store(false, Ordering::Release);
        if STATE_REFRESH_PENDING.swap(false, Ordering::AcqRel) {
            refresh_cached_state(&refresh_app);
        }
    });
}

/// Refresh the event-driven snapshot after a display transition. A short
/// delay lets Windows finish replacing the monitor and DPI topology before
/// the native queries run, without introducing a periodic polling loop.
pub fn refresh_after_display_change(app: &AppHandle) {
    let delayed_app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(180));
        refresh_cached_state(&delayed_app);
    });
}


/// Frontend command: update the native hit-test region after Svelte changes
/// the visible panes. This intentionally never changes window geometry, so
/// opening the submenu cannot move the base menu or invalidate hover state.
#[tauri::command]
pub fn tray_menu_set_expanded(
    app: AppHandle,
    expanded: bool,
    base: TrayMenuRect,
    submenu: Option<TrayMenuRect>,
) {
    SUBMENU_EXPANDED.store(expanded, Ordering::Release);
    #[cfg(target_os = "windows")]
    if let Some(window) = app.get_webview_window(LABEL) {
        let mut rects = vec![base];
        if expanded {
            if let Some(submenu) = submenu {
                // The two cards touch visually, but their individual rounded
                // regions (and IPC/layout rounding) can leave a thin seam that
                // routes the cursor through the transparent window. Add one
                // shared bridge covering only their vertical overlap; the
                // surrounding canvas remains click-through.
                let left = base.x.min(submenu.x);
                let right = (base.x + base.width).max(submenu.x + submenu.width);
                let top = base.y.max(submenu.y);
                let bottom = (base.y + base.height).min(submenu.y + submenu.height);
                if right > left && bottom > top {
                    rects.push(TrayMenuRect {
                        x: left,
                        y: top,
                        width: right - left,
                        height: bottom - top,
                        viewport_width: base.viewport_width,
                        viewport_height: base.viewport_height,
                    });
                }
                rects.push(submenu);
            }
        }
        apply_hit_region(&window, &rects);
    }
}

/// Frontend command: fetch current display info (resolutions list, current
/// selection) to populate the Resolution submenu.
#[tauri::command]
pub fn tray_menu_get_display_info() -> Result<crate::display_info::DisplayInfo, String> {
    let mut info = crate::display_info::query_current_display()
        .ok_or_else(|| "could not query display info".to_string())?;
    // Black Crush profiles are keyed by the monitor EDID id (MONITOR\...),
    // which monitors.rs and display_info.rs both resolve today. Keep the
    // substitution anyway so the tray always reports exactly what the main
    // app's monitor list uses, even if the two resolutions ever drift.
    if let Some(primary) = crate::monitors::all_monitors().into_iter().find(|m| m.primary) {
        if !primary.monitor_id.is_empty() {
            info.monitor_id = primary.monitor_id.clone();
        }
    }
    Ok(info)
}

/// Frontend command: apply a resolution pick from the custom menu, same
/// behavior as the native menu's tray:res: handler (tray_menu.rs). hz is
/// optional: favorites need an exact width×height@hz triple, while the
/// plain Resolution submenu keeps the display's current refresh rate (same
/// behavior as tray_menu.rs's tray:res: handler). monitor_id is the EDID id
/// of the monitor a favorite was saved on: the change targets that monitor
/// (like the main app's favorites path) instead of always the primary. The
/// plain Resolution submenu sends none, so it stays primary-scoped.
#[tauri::command]
pub fn tray_menu_set_resolution(
    app: AppHandle,
    width: u32,
    height: u32,
    hz: Option<u32>,
    monitor_id: Option<String>,
    monitor_device: Option<String>,
    skip_confirmation: Option<bool>,
) {
    std::thread::spawn(move || {
        // Resolve the favorite's saved monitor to its GDI device name. A
        // favorite whose monitor is not currently connected must not be
        // applied to the primary instead, that would silently change the
        // wrong display; skip it with a log line, mirroring the main app's
        // "Monitor not connected" guard. The GDI device name is unique per
        // physical monitor, so it wins when the favorite carries one: two
        // identical panels share the EDID id, and matching on it alone would
        // target the first sibling instead of the saved panel.
        let target_device = monitor_device
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let device_name = match target_device {
            Some(device) => {
                let resolved = crate::monitors::all_monitors()
                    .into_iter()
                    .find(|m| m.device_name.eq_ignore_ascii_case(device))
                    .map(|m| m.device_name)
                    .filter(|name| !name.is_empty());
                if resolved.is_none() {
                    log::warn!(
                        "[tray-menu] favorite target monitor {device} is not connected; skipping mode change"
                    );
                    return;
                }
                resolved
            }
            None => match monitor_id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
                Some(id) => {
                    let resolved = crate::monitors::all_monitors()
                        .into_iter()
                        .find(|m| {
                            !m.monitor_id.is_empty() && m.monitor_id.eq_ignore_ascii_case(id)
                        })
                        .map(|m| m.device_name)
                        .filter(|name| !name.is_empty());
                    if resolved.is_none() {
                        log::warn!(
                            "[tray-menu] favorite target monitor {id} is not connected; skipping mode change"
                        );
                        return;
                    }
                    resolved
                }
                None => None,
            },
        };
        // The no-op check must compare against the TARGET monitor's live
        // mode, not the primary's, so an already-active favorite on a
        // secondary display does not open a pointless confirmation.
        let current = match device_name.as_deref() {
            Some(name) => crate::display_info::query_display_for(name),
            None => crate::display_info::query_current_display(),
        };
        let target_hz = hz.unwrap_or_else(|| current.as_ref().map(|d| d.refresh_hz).unwrap_or(0));
        if current
            .as_ref()
            .is_some_and(|d| d.width == width && d.height == height && d.refresh_hz == target_hz)
        {
            return;
        }
        match crate::display_info::set_resolution(width, height, target_hz, device_name, skip_confirmation.unwrap_or(false)) {
            Ok(mode) => {
                if mode.skip_confirmation {
                    if let Err(error) = crate::display_info::confirm_mode_change() {
                        log::warn!("[tray-menu] trusted favorite confirmation failed: {error}");
                    }
                    return;
                }
                crate::flyout_window::show_after_ready(&app, mode).ok();
            }
            Err(e) => log::warn!("[tray-menu] resolution change failed: {e}"),
        }
    });
}

/// Frontend command: apply a refresh-rate pick from the Refresh Rate
/// submenu, same behavior as the native menu's tray:hz: handler
/// (tray_menu.rs).
#[tauri::command]
pub fn tray_menu_set_refresh_rate(app: AppHandle, hz: u32) {
    std::thread::spawn(move || {
        if crate::display_info::query_current_display()
            .as_ref()
            .is_some_and(|d| d.refresh_hz == hz)
        {
            return;
        }
        match crate::display_info::set_refresh_rate(hz, None) {
            Ok(mode) => {
                crate::flyout_window::show_after_ready(&app, mode).ok();
            }
            Err(e) => log::warn!("[tray-menu] refresh rate change failed: {e}"),
        }
    });
}

#[tauri::command]
pub fn tray_menu_hide(app: AppHandle) {
    hide(&app);
}

#[tauri::command]
pub fn tray_menu_restart(app: AppHandle) {
    hide(&app);
    crate::process::request_restart(&app, None);
}

/// Open (not toggle) the main app window from the tray menu header. Mirrors
/// the single-instance handler: unminimize + show + focus, then emit the
/// same window-visibility event so the frontend reconciles its tab state.
#[tauri::command]
pub fn tray_menu_open_main(app: AppHandle) {
    hide(&app);
    if let Some(window) = app.get_webview_window("main") {
        let was_minimized = window.is_minimized().unwrap_or(false);
        if was_minimized {
            let _ = window.unminimize();
        }
        let _ = window.show();
        let _ = window.set_focus();
        // animated = the window was minimized, so the restore replays the
        // Windows restore animation (see show_main_window).
        let _ = app.emit(
            "window-visibility",
            serde_json::json!({ "visible": true, "animated": was_minimized }),
        );
    }
}

#[tauri::command]
pub fn tray_menu_quit(app: AppHandle) {
    hide(&app);
    crate::request_shutdown_and_exit(&app);
}
