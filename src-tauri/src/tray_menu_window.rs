// tray_menu_window.rs — custom CSS-styled tray popup (replaces the native
// muda menu from tray_menu.rs for the right-click path, as a test).
//
// Built the same way as flyout_window.rs: a small undecorated/transparent
// Tauri WebviewWindow rendering real Svelte/CSS instead of a Win32
// TrackPopupMenu, anchored flush against the taskbar edge near the tray
// icon using the same Win32 SetWindowPos approach as flyout_window.rs's
// taskbar_position, rather than tauri-plugin-positioner's TrayCenter (which
// only centers on the tray icon at a fixed offset, not flush to the edge).

use serde::Deserialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalSize, WebviewUrl, WebviewWindowBuilder,
};

#[derive(Debug, Deserialize, Clone, Copy)]
pub struct TrayMenuRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
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
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
};

const LABEL: &str = "tray-menu";
static SUBMENU_EXPANDED: AtomicBool = AtomicBool::new(false);
static MENU_GENERATION: AtomicU64 = AtomicU64::new(0);
// The resolution submenu includes the same friendly/UW/native labels as the
// main app, so reserve enough left-side space to keep those tags readable.
// These are the rendered outer widths: the CSS declarations add 12px of
// horizontal padding and 1px borders on both sides of each pane.
// 284px base + up to 334px auto-sized resolution submenu. The submenu's
// CSS max-width:320px caps the content box; its own 12px horizontal padding
// and 2px borders make the widest rendered pane 334px (same reasoning as
// the 230px→244px note below). The base menu stays flush against the
// window's right edge, so the extra canvas is reserved on the left for
// whichever submenu is open.
const FULL_WIDTH: f64 = 618.0;
const FULL_HEIGHT: f64 = 460.0;
// CSS width 270px + padding/borders.
const BASE_WIDTH: f64 = 284.0;
// Estimate for the collapsed base menu: Resolution / Refresh Rate /
// Favorites / separator / Restart / Exit (6 rows + 1 separator). Only used
// as the initial native hit-region before the first real Svelte layout
// measurement replaces it via tray_menu_set_expanded (see below), so an
// approximate value is fine — it only has to be roughly right for the very
// first frame after the window is created.
// Includes the title/monitor/current-mode header, the three compact quick
// control rows, separators, plus the largest inline favorites layout (up to
// three favorites) before exact DOM measurement.
const BASE_HEIGHT_ESTIMATE: f64 = 425.0;
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
    let scale = window.scale_factor().unwrap_or(1.0).max(0.5);
    let mut regions: Vec<HRGN> = rects
        .iter()
        .filter_map(|rect| {
            let left = (rect.x * scale).round() as i32;
            let top = (rect.y * scale).round() as i32;
            let right = ((rect.x + rect.width) * scale).round() as i32;
            let bottom = ((rect.y + rect.height) * scale).round() as i32;
            if right <= left || bottom <= top {
                return None;
            }
            let radius = (10.0 * scale).round().max(1.0) as i32;
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
    apply_hit_region(
        window,
        &[TrayMenuRect {
            x: FULL_WIDTH - BASE_WIDTH,
            y: FULL_HEIGHT - BASE_HEIGHT_ESTIMATE,
            width: BASE_WIDTH,
            height: BASE_HEIGHT_ESTIMATE,
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

// Reads the primary monitor's effective DPI as a scale factor. The tray
// window's own scale factor (GetDpiForWindow / window.scale_factor()) can
// retain the pre-change DPI context briefly after a display-mode switch,
// which made the first menu open after a resolution change land at the wrong
// y — the physical size derived from the stale scale no longer matches the
// new work area. Querying the monitor directly (mirroring
// flyout_window.rs's primary_monitor_scale) is independent of the hidden
// window's previous scale, with a short retry budget for the DWM/monitor
// init race on cold start.
#[cfg(target_os = "windows")]
fn primary_monitor_scale(fallback: f64) -> f64 {
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
    log::warn!(
        "[tray-menu] GetDpiForMonitor failed all 6 attempts; falling back to window scale {fallback}"
    );
    fallback.max(0.5)
}

// Positions the tray menu flush against the taskbar edge, near the tray icon
// corner, the same way flyout_window.rs anchors the mode-confirmation popup.
// The taskbar edge is derived from the monitor's work area (rcWork =
// rcMonitor minus the taskbar strip): the edge where the two differ is where
// the taskbar sits. Called on each anchor pass when the menu opens (see
// toggle below); the window is never resized while visible — the final
// post-show pass only re-applies the position, which is what corrects the
// DPI-transition shift after a mode switch.
//
// anchor_x_px, when given, is the tray icon's click x-position in physical
// pixels (from the TrayIconEvent) — the visible base-menu card (which sits
// flush against the window's right edge, see tray-menu.css's row-reverse
// layout and apply_default_hit_region's BASE_WIDTH offset) is centered on
// that x instead of the window always hugging the work area's right edge.
// card_width_px is the base menu card's width, so the card itself ends up
// centered on the icon rather than the invisible full window (which is
// wider, to also fit the resolution submenu) being centered on it — that
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
    // icon's y position — not x — is what varies there. The card sits at the
    // window's right edge, so window_x = desired_card_x - (width_px -
    // card_width_px).
    let horizontal_taskbar = monitor.bottom - work.bottom > 0 || work.top - monitor.top > 0;
    let anchored_x = anchor_x_px.filter(|_| horizontal_taskbar).map(|anchor_x| {
        let card_x = anchor_x - card_width_px / 2;
        let window_x = card_x - (width_px - card_width_px);
        window_x.clamp(work.left, max_x)
    });
    let (x, y) = if monitor.bottom - work.bottom > 0 {
        // Taskbar along the bottom edge — pin just above it, centered on the
        // tray icon's x position when known, otherwise right-aligned near
        // the tray/notification area.
        (
            anchored_x.unwrap_or((work.right - width_px - gap).max(work.left)),
            (work.bottom - height_px - gap).max(work.top),
        )
    } else if work.top - monitor.top > 0 {
        // Taskbar along the top edge — just below it, centered on the tray
        // icon's x position when known, otherwise near the tray on the right.
        (
            anchored_x.unwrap_or((work.right - width_px - gap).max(work.left)),
            (work.top + gap).min(max_y),
        )
    } else if work.left - monitor.left > 0 {
        // Taskbar along the left edge — just right of it, near the tray.
        (
            (work.left + gap).min(max_x),
            (work.bottom - height_px - gap).max(work.top),
        )
    } else if monitor.right - work.right > 0 {
        // Taskbar along the right edge — just left of it, near the tray.
        (
            (work.right - width_px - gap).max(work.left),
            (work.bottom - height_px - gap).max(work.top),
        )
    } else {
        // No taskbar on the primary monitor — fall back to the lower-right
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
// when the taskbar runs horizontally (see taskbar_position).
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
// (SWP_NOSIZE) that re-samples the taskbar position — Explorer can update
// rcWork (the taskbar-excluded work area) after rcMonitor has already
// settled, so the first open after a mode switch can otherwise land a few
// dozen px off.
//
// IMPORTANT (learned from the diagnostic logs): the hidden window's DPI
// context is NOT updated by these passes. Windows defers a hidden window's
// DPI transition until the window is shown, and applies it by rescaling the
// window's pre-change geometry *during* show() — which is why the first
// open after a resolution change used to land at the old y scaled by the
// DPI ratio (e.g. y=711 instead of 959 in the logs). toggle() therefore
// calls this with resize=false once more AFTER show(), when the transition
// is complete and the position sticks.
#[cfg(target_os = "windows")]
fn anchor_to_taskbar(window: &tauri::WebviewWindow, anchor_x_px: Option<i32>, resize: bool) {
    let Ok(hwnd) = window.hwnd() else { return };
    let (width_px, height_px) = if resize {
        let scale = primary_monitor_scale(window.scale_factor().unwrap_or(1.0));
        let width = (FULL_WIDTH * scale).round().max(1.0) as u32;
        let height = (FULL_HEIGHT * scale).round().max(1.0) as u32;
        // Keep Tauri's size bookkeeping in sync so the post-show pass's
        // outer_size() read reflects the new physical size.
        if let Err(e) = window.set_size(PhysicalSize::new(width, height)) {
            log::warn!("[tray-menu] reposition resize failed: {e}");
        }
        (width, height)
    } else {
        match window.outer_size() {
            Ok(size) => (size.width as u32, size.height as u32),
            Err(_) => return,
        }
    };
    let scale = primary_monitor_scale(window.scale_factor().unwrap_or(1.0));
    let card_width_px = (BASE_WIDTH * scale).round() as i32;
    let Some((x, y)) = taskbar_position(
        width_px as i32,
        height_px as i32,
        anchor_x_px,
        card_width_px,
    ) else {
        return;
    };
    let flags = if resize {
        SWP_NOACTIVATE | SWP_NOZORDER
    } else {
        SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSIZE
    };
    let size_w = if resize { width_px as i32 } else { 0 };
    let size_h = if resize { height_px as i32 } else { 0 };
    unsafe {
        let _ = SetWindowPos(HWND(hwnd.0), None, x, y, size_w, size_h, flags);
    }
}

#[cfg(not(target_os = "windows"))]
fn anchor_to_taskbar(_window: &tauri::WebviewWindow, _anchor_x_px: Option<i32>, _resize: bool) {}

fn get_or_create_window(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    if let Some(window) = app.get_webview_window(LABEL) {
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
    let window = builder
        .build()
        .map_err(|e| format!("could not create tray menu window: {e}"))?;
    disable_dwm_corner_rounding(app);
    // Transparency-nudge resize — WebView2's transparent background is only
    // honored after a real post-creation resize (see flyout_window.rs).
    if let Err(e) = window.set_size(LogicalSize::new(FULL_WIDTH, FULL_HEIGHT)) {
        log::warn!("[tray-menu] initial resize failed: {e}");
    }
    // Start with a small native region before the frontend has measured its
    // cards. The first Svelte layout replaces this estimate with exact DOM
    // rectangles through tray_menu_set_expanded.
    apply_default_hit_region(&window);
    // Hide on blur, like a normal context menu dismissing on outside click.
    let blur_window = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Focused(false) = event {
            // The native region is updated asynchronously when a submenu
            // opens. Delay the blur dismissal long enough for the preinstalled
            // bridge/exact region to arrive; if the submenu is still open,
            // the pointer crossed internally and the popup must remain visible.
            // Capture the menu generation so an old blur callback can never
            // hide a newly reopened tray menu. Kept short (60ms) since there
            // is no exit animation to wait for — the window hides instantly
            // once this guard clears, so a long delay only reads as sluggish.
            let generation = MENU_GENERATION.load(Ordering::Acquire);
            let delayed_window = blur_window.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(60));
                if MENU_GENERATION.load(Ordering::Acquire) == generation
                    && !SUBMENU_EXPANDED.load(Ordering::Acquire)
                {
                    let _ = delayed_window.hide();
                }
            });
        }
    });
    Ok(window)
}

/// Called once from setup() so the webview exists before the first click.
pub fn prewarm(app: &AppHandle) {
    if let Err(e) = get_or_create_window(app) {
        log::warn!("[tray-menu] prewarm failed: {e}");
    }
    // Warm up the DPI query (mirrors flyout_window.rs's prewarm): on a cold
    // start it can still be racing DWM/monitor init, and losing that race on
    // the very first open would anchor the menu at a stale y.
    #[cfg(target_os = "windows")]
    {
        let _ = primary_monitor_scale(1.0);
    }
}

/// Show (or hide, if already visible) the custom tray menu, anchored flush
/// against the taskbar edge near the tray icon. anchor_x is the click's
/// physical-pixel x-coordinate from the TrayIconEvent, used to center the
/// popup on wherever the tray icon actually sits.
pub fn toggle(app: &AppHandle, anchor_x: f64) {
    let window = match get_or_create_window(app) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("[tray-menu] could not open tray menu: {e}");
            return;
        }
    };
    MENU_GENERATION.fetch_add(1, Ordering::AcqRel);
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }
    SUBMENU_EXPANDED.store(false, Ordering::Release);
    // A click outside the region can hide the window before Svelte gets a
    // chance to close its submenu state. Reset the native region before the
    // next show so a stale expanded shape can never intercept the first frame.
    apply_default_hit_region(&window);
    let _ = window.emit_to(LABEL, "tray-menu-show", ());
    // Two-step anchoring (mirrors flyout_window.rs): step 1 moves and sizes
    // the hidden window at the monitor's fresh DPI (the window's own DPI
    // context can be stale after a mode switch). Windows defers a hidden
    // window's DPI transition until it is shown and applies it by rescaling
    // the window's pre-change geometry *during* show(), which used to put
    // the first open after a resolution change at the wrong y (e.g. 711
    // instead of 959). Step 2 runs right after show(), when the transition
    // is complete and the window's scale matches the monitor, and re-applies
    // the anchor — it sticks this time. Both steps run in the same
    // synchronous main-thread block, so no intermediate frame is ever
    // painted at the stale position.
    let anchor = Some(anchor_x.round() as i32);
    anchor_to_taskbar(&window, anchor, true);
    let _ = window.show();
    anchor_to_taskbar(&window, anchor, false);
    let _ = window.set_focus();
}

pub fn hide(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.hide();
    }
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
    crate::display_info::query_current_display()
        .ok_or_else(|| "could not query display info".to_string())
}

/// Frontend command: apply a resolution pick from the custom menu, same
/// behavior as the native menu's tray:res: handler (tray_menu.rs). hz is
/// optional: favorites need an exact width×height@hz triple, while the
/// plain Resolution submenu keeps the display's current refresh rate (same
/// behavior as tray_menu.rs's tray:res: handler).
#[tauri::command]
pub fn tray_menu_set_resolution(app: AppHandle, width: u32, height: u32, hz: Option<u32>) {
    std::thread::spawn(move || {
        let current = crate::display_info::query_current_display();
        let target_hz = hz.unwrap_or_else(|| current.as_ref().map(|d| d.refresh_hz).unwrap_or(0));
        // The tray resolution list keeps the current refresh rate. Applying
        // the active width/height again is therefore a guaranteed no-op and
        // must not open the Keep display mode? confirmation.
        if current
            .as_ref()
            .is_some_and(|d| d.width == width && d.height == height && d.refresh_hz == target_hz)
        {
            return;
        }
        match crate::display_info::set_resolution(width, height, target_hz) {
            Ok(mode) => {
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
        match crate::display_info::set_refresh_rate(hz) {
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
    crate::request_restart(&app, None);
}

#[tauri::command]
pub fn tray_menu_quit(app: AppHandle) {
    hide(&app);
    crate::request_shutdown_and_exit(&app);
}
