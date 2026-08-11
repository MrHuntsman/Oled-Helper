// Custom Steam-style tray menu rendered by a dedicated WebView window.
// The popup uses the same display mode data and safe mode-change flow as the
// native tray menu, while allowing the frontend to provide the dark styling,
// header, hover states, and flyout submenus.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, OnceLock,
};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HWND;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, GetMonitorInfoW, MonitorFromWindow, SetWindowRgn,
    HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, RGN_OR,
};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE};

const MENU_LABEL: &str = "tray-menu";
const MENU_WIDTH: f64 = 700.0;
const MENU_HEIGHT: f64 = 700.0;

#[derive(Clone, Serialize, Deserialize)]
pub struct TrayFavorite {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TrayResolution {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TrayMenuData {
    pub title: String,
    pub monitor_name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub favorites: Vec<TrayFavorite>,
    pub resolutions: Vec<TrayResolution>,
    pub refresh_rates: Vec<u32>,
    pub taskbar_edge: String,
}

#[derive(Clone, Serialize)]
pub struct TrayMenuSnapshot {
    pub open: bool,
    pub data: Option<TrayMenuData>,
}

struct TrayMenuState {
    open: bool,
    data: Option<TrayMenuData>,
}

static STATE: OnceLock<Mutex<TrayMenuState>> = OnceLock::new();

fn state() -> &'static Mutex<TrayMenuState> {
    STATE.get_or_init(|| {
        Mutex::new(TrayMenuState {
            open: false,
            data: None,
        })
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Deserialize)]
pub struct HitRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[tauri::command]
pub fn tray_menu_set_hit_rects(app: AppHandle, rects: Vec<HitRect>) {
    #[cfg(target_os = "windows")]
    apply_hit_region(&app, &rects);
}

// The tray window is a full-work-area transparent WebView. Without this, the
// OS hit-tests the whole window and swallows mouse messages (clicks, hover,
// cursor changes) directed at the desktop or other apps beneath it. CSS
// pointer-events cannot help: it only controls event delivery inside the page.
// SetWindowRgn makes the OS treat everything outside the card/submenu rects
// as not part of the window at all, so mouse messages go straight to whatever
// is underneath, across processes (a WM_NCHITTEST HTTRANSPARENT reply only
// chains to same-thread windows). Rect coords are client-space physical px.
#[cfg(target_os = "windows")]
fn apply_hit_region(app: &AppHandle, rects: &[HitRect]) {
    let Some(window) = app.get_webview_window(MENU_LABEL) else {
        return;
    };
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    let hwnd = HWND(hwnd.0);
    unsafe {
        if rects.is_empty() {
            // No clickable content: reset to the full-window region.
            let _ = SetWindowRgn(hwnd, None, true);
            return;
        }
        let region = CreateRectRgn(0, 0, 0, 0);
        for r in rects {
            let rect = CreateRectRgn(r.x, r.y, r.x + r.width, r.y + r.height);
            let _ = CombineRgn(Some(region), Some(region), Some(rect), RGN_OR);
            let _ = DeleteObject(HGDIOBJ(rect.0));
        }
        // SetWindowRgn takes ownership of the region on success; the system
        // frees it when a new region is set or the window is destroyed.
        if SetWindowRgn(hwnd, Some(region), true) == 0 {
            let _ = DeleteObject(HGDIOBJ(region.0));
        }
    }
}

// Timestamp (ms) of the last time the menu was closed, by any path. The tray
// right-click toggle uses it so a click landing right after a close is treated
// as a toggle-off instead of reopening. Reopening while the exit fade is still
// running swaps the fade to the entrance transform mid-flight, which renders as
// a glitchy sideways exit on some GPU/display setups.
static LAST_CLOSE_MS: AtomicU64 = AtomicU64::new(0);

fn snapshot() -> TrayMenuSnapshot {
    let guard = state().lock().ok();
    TrayMenuSnapshot {
        open: guard.as_ref().map(|value| value.open).unwrap_or(false),
        data: guard.as_ref().and_then(|value| value.data.clone()),
    }
}

pub fn close_tray_menu(app: &AppHandle) {
    if let Ok(mut guard) = state().lock() {
        guard.open = false;
    }
    LAST_CLOSE_MS.store(now_ms(), Ordering::Relaxed);
    if let Some(window) = app.get_webview_window(MENU_LABEL) {
        let _ = window.emit("tray-menu-close", ());
    }

    // The frontend normally hides the window after its exit transition. Keep
    // a native fallback so a close event cannot leave the transparent popup
    // alive if the WebView is busy or its listener is not ready yet. Recheck
    // state before hiding so a rapid reopen is never hidden by this fallback.
    let fallback_app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let still_closed = state().lock().map(|guard| !guard.open).unwrap_or(true);
        if still_closed {
            hide_tray_menu_native(&fallback_app);
        }
    });
}

pub fn hide_tray_menu_native(app: &AppHandle) {
    if let Ok(mut guard) = state().lock() {
        guard.open = false;
    }
    if let Some(window) = app.get_webview_window(MENU_LABEL) {
        let size = window
            .outer_size()
            .map(|s| format!("{}x{}", s.width, s.height))
            .and_then(|size| {
                window
                    .outer_position()
                    .map(|p| format!("{size} at ({},{})", p.x, p.y))
            })
            .unwrap_or_else(|_| "size unknown".to_string());
        let visible = window.is_visible().unwrap_or(false);
        log::info!("[tray] native hide (visible={visible}, {size})");
        // Move the window far off-screen before hiding it. The card content is
        // already faded out by the time this runs, so the move is invisible;
        // any DWM/WebView2 composition artifact that plays when the native
        // window is hidden (most visible while the shell is still processing a
        // tray right-click) then happens off-screen instead of over the
        // taskbar. place_window repositions the window on-screen before the
        // next show.
        let _ = window.set_position(tauri::LogicalPosition::new(-32000.0, -32000.0));
        let _ = window.hide();
        crate::taskbar_dimmer::set_flyout_hwnd(0);
    } else {
        log::warn!("[tray] native hide requested but tray menu window is missing");
    }
}

fn set_current_data(data: TrayMenuData) {
    if let Ok(mut guard) = state().lock() {
        guard.data = Some(data);
        guard.open = false;
    }
}

#[cfg(target_os = "windows")]
fn taskbar_edge() -> String {
    let Some((monitor, work)) = primary_monitor_work_area() else {
        return "bottom".to_string();
    };
    if monitor.bottom - work.bottom > 0 {
        "bottom".to_string()
    } else if work.top - monitor.top > 0 {
        "top".to_string()
    } else if work.left - monitor.left > 0 {
        "left".to_string()
    } else if monitor.right - work.right > 0 {
        "right".to_string()
    } else {
        "bottom".to_string()
    }
}

#[cfg(not(target_os = "windows"))]
fn taskbar_edge() -> String {
    "bottom".to_string()
}

fn build_data(app: &AppHandle) -> TrayMenuData {
    let display = crate::display_info::query_current_display();
    let config = crate::config::load_config(app.clone());
    let (monitor_name, width, height, refresh_hz, mut resolutions, mut refresh_rates) =
        match display {
            Some(info) => (
                info.monitor_name,
                info.width,
                info.height,
                info.refresh_hz,
                info.resolutions
                    .into_iter()
                    .map(|mode| TrayResolution {
                        width: mode.width,
                        height: mode.height,
                    })
                    .collect(),
                info.refresh_rates,
            ),
            None => (
                "No display detected".to_string(),
                0,
                0,
                0,
                Vec::new(),
                Vec::new(),
            ),
        };
    let mut favorites: Vec<_> = config
        .favorite_display_modes
        .into_iter()
        .map(|favorite| TrayFavorite {
            width: favorite.width,
            height: favorite.height,
            hz: favorite.hz,
        })
        .collect();

    // Keep every display list in a predictable high-to-low order. This is
    // done in the payload so the menu stays consistent even before the
    // WebView has rendered it.
    favorites.sort_by(|a, b| {
        b.width
            .cmp(&a.width)
            .then_with(|| b.height.cmp(&a.height))
            .then_with(|| b.hz.cmp(&a.hz))
    });
    resolutions.sort_by(|a, b| b.width.cmp(&a.width).then_with(|| b.height.cmp(&a.height)));
    refresh_rates.sort_unstable_by(|a, b| b.cmp(a));

    TrayMenuData {
        title: "OLED Helper 2".to_string(),
        monitor_name,
        width,
        height,
        refresh_hz,
        favorites,
        resolutions,
        refresh_rates,
        taskbar_edge: taskbar_edge(),
    }
}

#[cfg(target_os = "windows")]
fn primary_monitor_work_area() -> Option<(
    windows::Win32::Foundation::RECT,
    windows::Win32::Foundation::RECT,
)> {
    unsafe {
        let monitor = MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info)
            .as_bool()
            .then_some((info.rcMonitor, info.rcWork))
    }
}

#[cfg(target_os = "windows")]
fn place_window(window: &WebviewWindow) -> Result<(), String> {
    let hwnd = window
        .hwnd()
        .map_err(|error| format!("could not get tray menu HWND: {error}"))?;
    let Some((_monitor, work)) = primary_monitor_work_area() else {
        return Err("could not determine the primary monitor work area".to_string());
    };

    // Keep the transparent WebView inside the monitor work area, not just the
    // visible card. This gives side flyouts a safe canvas to grow into while
    // guaranteeing that no submenu can extend over the taskbar. The shell is
    // pointer-transparent, so the unused part of this canvas does not create
    // a visible blank area or steal clicks from the desktop.
    let width = work.right - work.left;
    let height = work.bottom - work.top;
    unsafe {
        SetWindowPos(
            HWND(hwnd.0),
            Some(HWND_TOPMOST),
            work.left,
            work.top,
            width,
            height,
            SWP_NOACTIVATE,
        )
        .map_err(|error| format!("could not position tray menu: {error}"))?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn place_window(_window: &WebviewWindow) -> Result<(), String> {
    Ok(())
}

fn get_or_create_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    if let Some(window) = app.get_webview_window(MENU_LABEL) {
        return Ok(window);
    }
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map(|dir| dir.join("webview-tray-menu"))
        .ok();
    let mut builder =
        WebviewWindowBuilder::new(app, MENU_LABEL, WebviewUrl::App("tray-menu.html".into()))
            .title("OLED Helper")
            .inner_size(MENU_WIDTH, MENU_HEIGHT)
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
        .map_err(|error| format!("could not create tray menu window: {error}"))?;
    let event_window = window.clone();
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Focused(false) => {
            // Let the WebView play its exit transition before the native
            // window is hidden. A delayed focus notification can arrive just
            // after a rapid close and reopen, so ignore it if focus returned.
            if event_window.is_focused().unwrap_or(false) {
                return;
            }
            let is_open = state().lock().map(|guard| guard.open).unwrap_or(false);
            if !is_open {
                return;
            }
            close_tray_menu(event_window.app_handle());
        }
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            hide_tray_menu_native(event_window.app_handle());
        }
        tauri::WindowEvent::Destroyed => {
            crate::taskbar_dimmer::set_flyout_hwnd(0);
            if let Ok(mut guard) = state().lock() {
                guard.open = false;
            }
        }
        _ => {}
    });
    Ok(window)
}

pub fn prewarm(app: &AppHandle) {
    match get_or_create_window(app) {
        Ok(window) => {
            // Consume the native window's first-show/compositor transition
            // while the WebView is still transparent and has no menu data.
            // Without this, Windows can animate the first real show using its
            // initial window geometry, which looks like a horizontal slide;
            // later opens only run the card's intentional vertical keyframes.
            if let Err(error) = place_window(&window) {
                log::debug!("[tray] custom tray pre-warm positioning skipped: {error}");
            }
            if window.show().is_ok() {
                let _ = window.hide();
            }
            log::info!("[tray] custom tray menu pre-warmed");
        }
        Err(error) => log::warn!("[tray] custom tray menu pre-warm failed: {error}"),
    }
}

pub fn show_tray_menu(app: AppHandle) {
    let data = build_data(&app);
    set_current_data(data.clone());
    let Ok(window) = get_or_create_window(&app) else {
        if let Ok(mut guard) = state().lock() {
            guard.open = false;
        }
        return;
    };
    if let Err(error) = place_window(&window) {
        log::warn!("[tray] custom menu positioning failed: {error}");
        if let Ok(mut guard) = state().lock() {
            guard.open = false;
        }
        return;
    }
    if let Err(error) = window.show() {
        log::warn!("[tray] custom menu show failed: {error}");
        if let Ok(mut guard) = state().lock() {
            guard.open = false;
        }
        return;
    }
    if let Ok(mut guard) = state().lock() {
        guard.open = true;
    }
    let _ = window.set_focus();
    if let Ok(hwnd) = window.hwnd() {
        crate::taskbar_dimmer::set_flyout_hwnd(hwnd.0 as isize);
    }
    if let Ok(size) = window.outer_size() {
        log::info!(
            "[tray] custom menu opened (size {}x{})",
            size.width,
            size.height
        );
    }
    let _ = window.emit("tray-menu-open", data);
}

// How long after any close a right-click is treated as a toggle-off instead of
// a reopen. The window covers both the race between the Focused(false) event
// and the tray click handler (a few ms) and the frontend's ~140 ms exit fade,
// so a rapid right-click cannot reopen the menu while it is still fading out.
const CLOSE_TOGGLE_WINDOW_MS: u64 = 200;

/// Right-click toggle: opens the menu when it is closed, closes it when it is
/// open. A single right-click must never both close and reopen, so when the
/// menu is closed but was just closed by this click's focus loss, the click is
/// treated as a toggle-off instead of reopening.
pub fn toggle_tray_menu(app: &AppHandle) {
    let was_open = state().lock().map(|guard| guard.open).unwrap_or(false);
    if was_open {
        close_tray_menu(app);
        return;
    }
    let last_close = LAST_CLOSE_MS.load(Ordering::Relaxed);
    if last_close != 0 && now_ms().saturating_sub(last_close) < CLOSE_TOGGLE_WINDOW_MS {
        return;
    }
    show_tray_menu(app.clone());
}

#[tauri::command]
pub fn tray_menu_get_snapshot() -> TrayMenuSnapshot {
    snapshot()
}

#[tauri::command]
pub fn tray_menu_close(app: AppHandle) {
    close_tray_menu(&app);
}

#[tauri::command]
pub fn tray_menu_hide_native(app: AppHandle) {
    hide_tray_menu_native(&app);
}

// Tears the process down only after the frontend has finished its exit fade
// and hidden the popup (the close event starts a ~140 ms fade; the frontend
// then hides the native window). Exiting mid-animation makes the WebView2
// compositor render its teardown frame, which can visibly drift on some
// GPU/display setups before the process disappears.
const EXIT_ANIMATION_GRACE_MS: u64 = 250;

#[tauri::command]
pub fn tray_menu_restart(app: AppHandle) {
    close_tray_menu(&app);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(EXIT_ANIMATION_GRACE_MS));
        crate::request_restart(&app);
    });
}

#[tauri::command]
pub fn tray_menu_quit(app: AppHandle) {
    close_tray_menu(&app);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(EXIT_ANIMATION_GRACE_MS));
        crate::request_shutdown_and_exit(&app);
    });
}

fn active_mode() -> Option<(u32, u32, u32)> {
    crate::display_info::query_current_display()
        .map(|info| (info.width, info.height, info.refresh_hz))
}

#[tauri::command]
pub fn tray_menu_select_resolution(app: AppHandle, width: u32, height: u32) {
    let current = active_mode();
    let preferred_hz = current.map(|(_, _, hz)| hz).unwrap_or(0);
    if current.is_some_and(|(current_width, current_height, _)| {
        current_width == width && current_height == height
    }) {
        close_tray_menu(&app);
        return;
    }
    close_tray_menu(&app);
    std::thread::spawn(move || {
        crate::tray_display_mode_change(
            app,
            crate::display_info::set_resolution(width, height, preferred_hz),
        );
    });
}

#[tauri::command]
pub fn tray_menu_select_refresh_rate(app: AppHandle, hz: u32) {
    if active_mode().is_some_and(|(_, _, current_hz)| current_hz == hz) {
        close_tray_menu(&app);
        return;
    }
    close_tray_menu(&app);
    std::thread::spawn(move || {
        crate::tray_display_mode_change(app, crate::display_info::set_refresh_rate(hz));
    });
}

#[tauri::command]
pub fn tray_menu_select_favorite(app: AppHandle, width: u32, height: u32, hz: u32) {
    if active_mode().is_some_and(|mode| mode == (width, height, hz)) {
        close_tray_menu(&app);
        return;
    }
    close_tray_menu(&app);
    std::thread::spawn(move || {
        crate::tray_display_mode_change(
            app,
            crate::display_info::set_resolution(width, height, hz),
        );
    });
}

#[cfg(not(target_os = "windows"))]
pub fn show_tray_menu(_app: AppHandle) {}
