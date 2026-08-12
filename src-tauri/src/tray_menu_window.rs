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
use tauri::{AppHandle, Emitter, LogicalSize, Manager, WebviewUrl, WebviewWindowBuilder};

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
    HRGN, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, RGN_ERROR, RGN_OR,
};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
};

const LABEL: &str = "tray-menu";
const FULL_WIDTH: f64 = 420.0;
const FULL_HEIGHT: f64 = 340.0;
const BASE_WIDTH: f64 = 259.0;
const BASE_HEIGHT_ESTIMATE: f64 = 122.0;
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
fn primary_monitor_info() -> Option<MONITORINFO> {
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
        Some(info)
    }
}

// Positions the tray menu flush against the taskbar edge, near the tray icon
// corner, the same way flyout_window.rs anchors the mode-confirmation popup.
// The taskbar edge is derived from the monitor's work area (rcWork =
// rcMonitor minus the taskbar strip): the edge where the two differ is where
// the taskbar sits. Called once when the menu opens (see toggle below); the
// window is never resized or moved again while visible.
#[cfg(target_os = "windows")]
fn taskbar_position(width_px: i32, height_px: i32) -> Option<(i32, i32)> {
    let info = primary_monitor_info()?;
    let work = info.rcWork;
    let monitor = info.rcMonitor;
    let gap = 6;
    let max_x = (work.right - width_px).max(work.left);
    let max_y = (work.bottom - height_px).max(work.top);
    let (x, y) = if monitor.bottom - work.bottom > 0 {
        // Taskbar along the bottom edge — pin just above it, right-aligned
        // near the tray/notification area.
        (
            (work.right - width_px - gap).max(work.left),
            (work.bottom - height_px - gap).max(work.top),
        )
    } else if work.top - monitor.top > 0 {
        // Taskbar along the top edge — just below it, near the tray.
        (
            (work.right - width_px - gap).max(work.left),
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
fn taskbar_position(_width_px: i32, _height_px: i32) -> Option<(i32, i32)> {
    None
}

// Re-anchors the window to the taskbar edge. Called once when the menu
// opens (see toggle below).
#[cfg(target_os = "windows")]
fn reposition_to_taskbar(window: &tauri::WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else { return };
    let Ok(size) = window.outer_size() else {
        return;
    };
    let Some((x, y)) = taskbar_position(size.width as i32, size.height as i32) else {
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
fn reposition_to_taskbar(_window: &tauri::WebviewWindow) {}

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
            let _ = blur_window.hide();
        }
    });
    Ok(window)
}

/// Called once from setup() so the webview exists before the first click.
pub fn prewarm(app: &AppHandle) {
    if let Err(e) = get_or_create_window(app) {
        log::warn!("[tray-menu] prewarm failed: {e}");
    }
}

/// Show (or hide, if already visible) the custom tray menu, anchored flush
/// against the taskbar edge near the tray icon.
pub fn toggle(app: &AppHandle) {
    let window = match get_or_create_window(app) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("[tray-menu] could not open tray menu: {e}");
            return;
        }
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }
    // A click outside the region can hide the window before Svelte gets a
    // chance to close its submenu state. Reset the native region before the
    // next show so a stale expanded shape can never intercept the first frame.
    apply_default_hit_region(&window);
    let _ = window.emit_to(LABEL, "tray-menu-show", ());
    reposition_to_taskbar(&window);
    let _ = window.show();
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
    #[cfg(target_os = "windows")]
    if let Some(window) = app.get_webview_window(LABEL) {
        let rects = if expanded {
            submenu.into_iter().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut rects = rects;
        rects.insert(0, base);
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
/// behavior as the native menu's tray:res: handler (tray_menu.rs).
#[tauri::command]
pub fn tray_menu_set_resolution(app: AppHandle, width: u32, height: u32) {
    std::thread::spawn(move || {
        let current_hz = crate::display_info::query_current_display()
            .map(|d| d.refresh_hz)
            .unwrap_or(0);
        match crate::display_info::set_resolution(width, height, current_hz) {
            Ok(mode) => {
                crate::flyout_window::show_after_ready(&app, mode).ok();
            }
            Err(e) => log::warn!("[tray-menu] resolution change failed: {e}"),
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
    crate::request_restart(&app);
}

#[tauri::command]
pub fn tray_menu_quit(app: AppHandle) {
    hide(&app);
    crate::request_shutdown_and_exit(&app);
}
