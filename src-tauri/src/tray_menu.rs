// Custom Steam-style tray menu rendered by a dedicated WebView window.
// The popup uses the same display mode data and safe mode-change flow as the
// native tray menu, while allowing the frontend to provide the dark styling,
// header, hover states, and flyout submenus.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Mutex, OnceLock,
};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{HWND, POINT};
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY,
};
#[cfg(target_os = "windows")]
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongPtrW, GetWindowRect, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWL_EXSTYLE, HWND_TOPMOST, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOSIZE, SW_SHOWNOACTIVATE,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

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
    pub name: String,
    pub native: bool,
    pub ultrawide: bool,
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
    // Cursor position at click time, as a fraction (0..1) of the MONITOR
    // bounds (not the work area, a tray icon click happens inside the
    // taskbar strip, which the work area excludes) from its top-left. The
    // native window covers the monitor work area, and the frontend converts
    // these monitor-relative fractions into work-area-local CSS pixels.
    pub cursor_x: f64,
    pub cursor_y: f64,
    // Work area bounds as fractions (0..1) of the same monitor rect. The
    // frontend clamps the card within this box (not the full monitor) so it
    // never overlaps the taskbar strip.
    pub work_left: f64,
    pub work_top: f64,
    pub work_right: f64,
    pub work_bottom: f64,
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

static HIT_RECTS: OnceLock<Mutex<Vec<HitRect>>> = OnceLock::new();

#[tauri::command]
pub fn tray_menu_set_hit_rects(_app: AppHandle, rects: Vec<HitRect>) {
    if let Ok(mut stored) = HIT_RECTS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        *stored = rects;
    }
}

// Frontend reports the rendered card height (CSS px) after the menu card
// mounts. The value feeds the NEXT placement, and while the popup is already
// open it is also used to shift the visible window so the card's taskbar-
// facing edge lands exactly on the work-area boundary (a shorter menu, e.g.
// 0-1 favorites, otherwise floats above the taskbar because the initial
// placement assumed the taller default card).
#[tauri::command]
#[cfg(target_os = "windows")]
pub fn tray_menu_set_card_height(app: AppHandle, height_css: f64) {
    // Bounded to the card's own max-height (see .tray-menu-card max-height)
    // so a stale or nonsensical report can never shift the popup off-screen.
    let actual = height_css.round();
    if !actual.is_finite() || !(120.0..=800.0).contains(&actual) {
        return;
    }
    MEASURED_CARD_HEIGHT_CSS.store(actual as u32, Ordering::Release);
    if !MENU_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    let Some(state) = LAST_PLACEMENT.lock().ok().and_then(|guard| *guard) else {
        return;
    };
    if (state.used_height_css - actual).abs() < 1.0 {
        return;
    }
    // The card is placed at local (24,24) inside a viewport whose origin is
    // (card_left, card_top) - HIT_CUSHION on screen (see place_window). A
    // bottom-flush card pins its bottom edge to the work-area bottom, so a
    // shorter card must shift the window down by the height difference. A
    // top-flush card pins its top edge, so no shift is needed. A centered
    // card re-centers on the same cursor line.
    let delta_css = if state.bottom_flush {
        state.used_height_css - actual
    } else if state.top_flush {
        0.0
    } else {
        (state.used_height_css - actual) / 2.0
    };
    let delta_y = (delta_css * state.scale).round() as i32;
    if delta_y == 0 {
        // Update the used height even without a move so a repeat report
        // (e.g. after a submenu opens) does not retrigger the math.
        if let Ok(mut guard) = LAST_PLACEMENT.lock() {
            if let Some(stored) = guard.as_mut() {
                stored.used_height_css = actual;
            }
        }
        return;
    }
    let Some(window) = app.get_webview_window(MENU_LABEL) else {
        return;
    };
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    unsafe {
        let mut rect = windows::Win32::Foundation::RECT::default();
        if GetWindowRect(HWND(hwnd.0), &mut rect).is_err() {
            return;
        }
        let _ = SetWindowPos(
            HWND(hwnd.0),
            Some(HWND_TOPMOST),
            rect.left,
            rect.top + delta_y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    log::debug!(
        "[tray] measured card height {actual}px (was {}px, {delta_css:.0}px shift)",
        state.used_height_css
    );
    if let Ok(mut guard) = LAST_PLACEMENT.lock() {
        if let Some(stored) = guard.as_mut() {
            stored.used_height_css = actual;
        }
    }
}

#[tauri::command]
#[cfg(not(target_os = "windows"))]
pub fn tray_menu_set_card_height(_app: AppHandle, _height_css: f64) {}

// Timestamp (ms) of the last time the menu was closed, by any path. The tray
// right-click toggle uses it so a click landing right after a close is treated
// as a toggle-off instead of reopening. Reopening while the exit fade is still
// running swaps the fade to the entrance transform mid-flight, which renders as
// a glitchy sideways exit on some GPU/display setups.
static LAST_CLOSE_MS: AtomicU64 = AtomicU64::new(0);
static MENU_ACTIVE: AtomicBool = AtomicBool::new(false);
// Measured rendered card height (CSS px) reported by the frontend after the
// menu card mounts. place_window uses it instead of the hardcoded estimate so
// the taskbar-facing edge lands exactly on the work-area boundary even when
// the menu is shorter (0-1 favorites). 0 means "not measured yet".
static MEASURED_CARD_HEIGHT_CSS: AtomicU32 = AtomicU32::new(0);
// Geometry captured by the last place_window so a later measured-height report
// can shift the already-visible popup flush without re-deriving the placement
// inputs (the click anchor is consumed on show).
#[derive(Clone, Copy)]
struct PlacementState {
    scale: f64,
    used_height_css: f64,
    bottom_flush: bool,
    top_flush: bool,
}
static LAST_PLACEMENT: Mutex<Option<PlacementState>> = Mutex::new(None);
// A right-click on the tray icon produces both Down and Up events. When the
// popup is already open, close it on Down and consume the matching Up so the
// same click cannot close, then reopen the menu after a focus-loss callback.
static SUPPRESS_NEXT_TRAY_RIGHT_UP_MS: AtomicU64 = AtomicU64::new(0);
// When the tray popup is already open, the low-level mouse hook consumes the
// icon's second right-click before it reaches Explorer or the tray-icon window.
// This closes the popup without allowing a native context menu to be created.
static SUPPRESS_HOOK_RIGHT_UP_MS: AtomicU64 = AtomicU64::new(0);
static APP_HANDLE: OnceLock<Mutex<Option<AppHandle>>> = OnceLock::new();

fn app_handle() -> Option<AppHandle> {
    APP_HANDLE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
}

pub fn tray_right_click_hook_needed() -> bool {
    MENU_ACTIVE.load(Ordering::Acquire) || SUPPRESS_HOOK_RIGHT_UP_MS.load(Ordering::Acquire) != 0
}

pub fn suppress_tray_icon_right_button(x: i32, y: i32, msg_type: u32) -> bool {
    const RIGHT_DOWN: u32 = 0x0204;
    const RIGHT_UP: u32 = 0x0205;
    if msg_type == RIGHT_UP {
        // The next right-up belongs to the suppressed tray down. A new
        // right-down clears stale state before any later click can start, so
        // this does not need a duration limit and also handles a held button.
        return SUPPRESS_HOOK_RIGHT_UP_MS.swap(0, Ordering::AcqRel) != 0;
    }
    if msg_type != RIGHT_DOWN {
        return false;
    }

    // A stale button-up must never affect the next independent click after a
    // hook event was lost. Clear it as soon as a new right-button down starts.
    SUPPRESS_HOOK_RIGHT_UP_MS.store(0, Ordering::Release);
    if !MENU_ACTIVE.load(Ordering::Acquire) {
        return false;
    }
    let on_tray_icon = TRAY_ICON_RECT
        .lock()
        .ok()
        .and_then(|guard| *guard)
        .map(|(left, top, width, height)| {
            x >= left
                && x < left.saturating_add(width)
                && y >= top
                && y < top.saturating_add(height)
        })
        .unwrap_or(false);
    if !on_tray_icon {
        return false;
    }

    // Returning a non-zero value from the WH_MOUSE_LL hook prevents this
    // second right-click from reaching the tray icon and Explorer. Close the
    // custom popup asynchronously, then consume the matching button-up too.
    // Without consuming both messages, Windows can still open a context menu
    // after the custom popup has closed.
    SUPPRESS_HOOK_RIGHT_UP_MS.store(now_ms(), Ordering::Release);
    if let Some(app) = app_handle() {
        std::thread::spawn(move || close_tray_menu(&app));
    }
    true
}

pub fn handle_global_mouse_down(x: i32, y: i32, msg_type: u32) {
    if !MENU_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if msg_type == 0x0204 {
        let on_tray_icon = TRAY_ICON_RECT
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .map(|(left, top, width, height)| {
                x >= left
                    && x < left.saturating_add(width)
                    && y >= top
                    && y < top.saturating_add(height)
            })
            .unwrap_or(false);
        if on_tray_icon {
            return;
        }
    }
    let Some(app) = app_handle() else {
        return;
    };
    let Some(window) = app.get_webview_window(MENU_LABEL) else {
        return;
    };
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    #[cfg(target_os = "windows")]
    unsafe {
        let mut rect = windows::Win32::Foundation::RECT::default();
        if windows::Win32::UI::WindowsAndMessaging::GetWindowRect(HWND(hwnd.0), &mut rect).is_err()
        {
            return;
        }
        // For a right-click, trust the native popup bounds before consulting
        // the asynchronously reported card/submenu rectangles. The submenu
        // can be repositioning or revealing on its first frame after reopen,
        // so HIT_RECTS may briefly contain its previous geometry and wrongly
        // classify a click on a submenu item as an outside click. The compact
        // native window already contains the card, its submenus, and the
        // interaction cushion, so any right-click inside it belongs to the
        // custom menu and must not start the close path.
        if msg_type == 0x0204
            && x >= rect.left
            && x < rect.right
            && y >= rect.top
            && y < rect.bottom
        {
            return;
        }
        let local_x = x - rect.left;
        let local_y = y - rect.top;
        let inside_menu = HIT_RECTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .map(|rects| {
                rects.iter().any(|hit| {
                    local_x >= hit.x
                        && local_x < hit.x.saturating_add(hit.width)
                        && local_y >= hit.y
                        && local_y < hit.y.saturating_add(hit.height)
                })
            })
            .unwrap_or(false);
        if !inside_menu {
            close_tray_menu(&app);
        }
    }
}

// Screen position (physical px) to anchor the menu on, set from the tray
// icon's own event rect just before toggle_tray_menu runs. This is far more
// reliable than a later GetCursorPos call: the tray click handler in lib.rs
// receives the icon's exact bounds directly from Tauri/Windows in the click
// event itself, with no risk of the cursor having moved, no dependency on the
// taskbar dimmer's async mouse-hook cache, and no ambiguity about which
// monitor/icon was clicked.
static CLICK_ANCHOR: Mutex<Option<(f64, f64)>> = Mutex::new(None);
static TRAY_ICON_RECT: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);

pub fn set_tray_icon_rect(x: i32, y: i32, width: i32, height: i32) {
    if let Ok(mut guard) = TRAY_ICON_RECT.lock() {
        *guard = Some((x, y, width.max(0), height.max(0)));
    }
}

pub fn set_click_anchor(x: f64, y: f64) {
    if let Ok(mut guard) = CLICK_ANCHOR.lock() {
        *guard = Some((x, y));
    }
}

fn click_anchor() -> Option<(f64, f64)> {
    CLICK_ANCHOR.lock().ok().and_then(|guard| *guard)
}

fn clear_click_anchor() {
    if let Ok(mut guard) = CLICK_ANCHOR.lock() {
        *guard = None;
    }
}

fn snapshot() -> TrayMenuSnapshot {
    let guard = state().lock().ok();
    TrayMenuSnapshot {
        open: guard.as_ref().map(|value| value.open).unwrap_or(false),
        data: guard.as_ref().and_then(|value| value.data.clone()),
    }
}

pub fn handle_tray_right_down(app: &AppHandle) {
    // The frontend exit animation keeps the native window visible for a short
    // time after state.open becomes false. MENU_ACTIVE is cleared only when
    // the native close path begins, so it is the reliable click-sequence state
    // and does not depend on Tauri's visibility cache after native ShowWindow.
    let state_open = state().lock().map(|guard| guard.open).unwrap_or(false);
    let was_open = state_open || MENU_ACTIVE.load(Ordering::Acquire);
    if was_open {
        // Set the guard before starting the close notification. The matching
        // button-up can arrive while the frontend is beginning its fade, and
        // must never be interpreted as a fresh open request.
        SUPPRESS_NEXT_TRAY_RIGHT_UP_MS.store(now_ms(), Ordering::Release);
        close_tray_menu(app);
    } else {
        SUPPRESS_NEXT_TRAY_RIGHT_UP_MS.store(0, Ordering::Release);
    }
}

pub fn consume_tray_right_up() -> bool {
    const RIGHT_CLICK_PAIR_TIMEOUT_MS: u64 = 750;
    let closed_at = SUPPRESS_NEXT_TRAY_RIGHT_UP_MS.swap(0, Ordering::AcqRel);
    closed_at != 0 && now_ms().saturating_sub(closed_at) <= RIGHT_CLICK_PAIR_TIMEOUT_MS
}

pub fn close_tray_menu(app: &AppHandle) {
    MENU_ACTIVE.store(false, Ordering::Release);
    crate::taskbar_dimmer::set_tray_menu_active(false);
    if let Ok(mut guard) = state().lock() {
        guard.open = false;
    }
    if let Some(rects) = HIT_RECTS.get() {
        if let Ok(mut stored) = rects.lock() {
            stored.clear();
        }
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
    MENU_ACTIVE.store(false, Ordering::Release);
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
        // Move and hide the HWND in one native operation. Using
        // WebviewWindow::set_position here stores a logical position in Tauri's
        // window state, which can be restored once at the old resolution after
        // a display-mode change. The native operation keeps the stale position
        // out of the first frame; place_window sets the new physical position
        // before the next show.
        #[cfg(target_os = "windows")]
        if let Ok(hwnd) = window.hwnd() {
            let size = window
                .outer_size()
                .unwrap_or(tauri::PhysicalSize::new(1, 1));
            unsafe {
                let _ = SetWindowPos(
                    HWND(hwnd.0),
                    Some(HWND_TOPMOST),
                    -32000,
                    -32000,
                    size.width as i32,
                    size.height as i32,
                    SWP_HIDEWINDOW | SWP_NOACTIVATE,
                );
            }
        }
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
    let Some((monitor, work)) = menu_monitor_work_area() else {
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
            Some(info) => {
                let native_width = info.native_width;
                let native_height = info.native_height;
                let mut modes = info.resolutions;
                let (fallback_width, fallback_height) = modes
                    .iter()
                    .max_by_key(|mode| mode.width * mode.height)
                    .map(|mode| (mode.width, mode.height))
                    .unwrap_or((0, 0));
                let native_width = if native_width > 0 {
                    native_width
                } else {
                    fallback_width
                };
                let native_height = if native_height > 0 {
                    native_height
                } else {
                    fallback_height
                };
                let resolutions = modes
                    .drain(..)
                    .map(|mode| TrayResolution {
                        width: mode.width,
                        height: mode.height,
                        name: resolution_name(mode.width, mode.height),
                        native: mode.width == native_width && mode.height == native_height,
                        ultrawide: is_ultrawide(mode.width, mode.height),
                    })
                    .collect();
                (
                    info.monitor_name,
                    info.width,
                    info.height,
                    info.refresh_hz,
                    resolutions,
                    info.refresh_rates,
                )
            }
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

    let (cursor_x, cursor_y, work_left, work_top, work_right, work_bottom) = cursor_and_work_area();
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
        cursor_x,
        cursor_y,
        work_left,
        work_top,
        work_right,
        work_bottom,
    }
}

fn resolution_name(width: u32, height: u32) -> String {
    match (width, height) {
        (1280, 720) => "HD".to_string(),
        (1920, 1080) => "FHD".to_string(),
        (2560, 1080) => "Ultrawide FHD".to_string(),
        (2560, 1440) => "QHD".to_string(),
        (3440, 1440) => "UWQHD".to_string(),
        (3840, 2160) => "4K".to_string(),
        (5120, 2160) => "5K Ultrawide".to_string(),
        (5120, 2880) => "5K".to_string(),
        (7680, 4320) => "8K".to_string(),
        _ => String::new(),
    }
}

fn is_ultrawide(width: u32, height: u32) -> bool {
    height > 0 && (width as f64 / height as f64) > 1.9
}

#[cfg(target_os = "windows")]
fn cursor_and_work_area() -> (f64, f64, f64, f64, f64, f64) {
    let Some((monitor, work)) = menu_monitor_work_area() else {
        return (0.0, 0.0, 0.0, 0.0, 1.0, 1.0);
    };
    // Prefer the click anchor set from the tray icon's own event rect
    // (lib.rs, TrayIconEvent::Click) over GetCursorPos. The click event
    // carries the icon's exact screen bounds directly from Windows at the
    // moment of the click ,  a later GetCursorPos call has no such guarantee:
    // it can race the dimmer's async mouse-hook cache, or simply run after
    // the cursor has moved (e.g. toward the menu that is about to open).
    // GetCursorPos remains the fallback for the non-Windows-tray-icon-click
    // path (menu re-opened via some other trigger) and old builds without
    // the anchor set.
    let anchor = click_anchor();
    let (x, y, source) = if let Some((ax, ay)) = anchor {
        (ax as i32, ay as i32, "click_anchor")
    } else {
        let mut point = POINT::default();
        let got = unsafe { GetCursorPos(&mut point).is_ok() };
        if got {
            crate::taskbar_dimmer::set_cursor_position(point.x, point.y);
            (point.x, point.y, "cursor_fallback")
        } else {
            let (x, y) = crate::taskbar_dimmer::last_cursor_position().unwrap_or((0, 0));
            (x, y, "dimmer_cache_fallback")
        }
    };
    // Fractions are relative to the full MONITOR bounds, not the work area.
    // A tray icon click happens inside the taskbar strip, which Windows
    // excludes from the work area entirely (work area = monitor minus
    // taskbar) ,  so a work-area-relative cursor fraction always clamps to
    // the bottom/right edge for any taskbar click. The webview itself is
    // placed to cover the full monitor bounds (see place_window) so cursor
    // and work-area fractions share the same coordinate space; the frontend
    // then clamps the card within the work-area box instead of the full
    // monitor so it never overlaps the taskbar strip.
    let width = (monitor.right - monitor.left) as f64;
    let height = (monitor.bottom - monitor.top) as f64;
    let fx = ((x - monitor.left) as f64 / width).clamp(0.0, 1.0);
    let fy = ((y - monitor.top) as f64 / height).clamp(0.0, 1.0);
    let work_left = ((work.left - monitor.left) as f64 / width).clamp(0.0, 1.0);
    let work_top = ((work.top - monitor.top) as f64 / height).clamp(0.0, 1.0);
    let work_right = ((work.right - monitor.left) as f64 / width).clamp(0.0, 1.0);
    let work_bottom = ((work.bottom - monitor.top) as f64 / height).clamp(0.0, 1.0);
    log::info!(
        "[tray] cursor_and_work_area source={source} used=({x},{y}) monitor={:?} work={:?} cursor_frac=({fx:.3},{fy:.3}) work_frac=({work_left:.3},{work_top:.3},{work_right:.3},{work_bottom:.3})",
        monitor, work
    );
    (fx, fy, work_left, work_top, work_right, work_bottom)
}

#[cfg(not(target_os = "windows"))]
fn cursor_and_work_area() -> (f64, f64, f64, f64, f64, f64) {
    (0.0, 0.0, 0.0, 0.0, 1.0, 1.0)
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

// Work area of the monitor the menu should open on: the monitor under the
// click anchor (the tray icon's own rect, set via set_click_anchor) or,
// failing that, the current cursor position. This matters when the taskbar
// or its icon area sits on a secondary monitor (the old primary-only
// behaviour kept the menu stuck on the primary corner). Falls back to the
// primary monitor when neither is available.
#[cfg(target_os = "windows")]
fn menu_monitor_work_area() -> Option<(
    windows::Win32::Foundation::RECT,
    windows::Win32::Foundation::RECT,
)> {
    let anchor_point = anchor_screen_point();
    let point = match anchor_point {
        Some((x, y)) => POINT {
            x: x as i32,
            y: y as i32,
        },
        None => {
            let mut point = POINT::default();
            if unsafe { GetCursorPos(&mut point).is_err() } {
                return primary_monitor_work_area();
            }
            point
        }
    };
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            return primary_monitor_work_area();
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

// Reads without consuming the click anchor set by the tray icon's own
// event rect. It remains available through build_data and place_window so
// monitor selection, taskbar-edge detection, cursor fractions, and native
// window placement all use the same click. It is cleared after placement.
#[cfg(target_os = "windows")]
fn anchor_screen_point() -> Option<(f64, f64)> {
    CLICK_ANCHOR.lock().ok().and_then(|guard| *guard)
}

#[cfg(target_os = "windows")]
fn place_window(window: &WebviewWindow) -> Result<(), String> {
    let hwnd = window
        .hwnd()
        .map_err(|error| format!("could not get tray menu HWND: {error}"))?;
    let Some((_monitor_rect, work)) = menu_monitor_work_area() else {
        return Err("could not determine the tray monitor work area".to_string());
    };

    // Use a small content-sized viewport rather than a transparent
    // work-area-sized window. A large topmost WebView2 surface can make Edge
    // defer compositor paints and video frames even though input still gets
    // processed. The frontend uses local coordinates inside this viewport.
    // The card is 340px wide. Reserve room for a flyout on either side,
    // while keeping the native surface compact enough that its right/bottom
    // edge does not force the card back to the screen's left/top corner.
    const INITIAL_VIEWPORT_WIDTH_CSS: f64 = 640.0;
    const INITIAL_VIEWPORT_HEIGHT_CSS: f64 = 430.0;
    const CARD_WIDTH_CSS: f64 = 340.0;
    // Fallback until the frontend reports the rendered card height (the card
    // is shorter with 0-1 favorites, taller with many rows). The measured
    // height keeps the taskbar-facing edge exactly on the work-area boundary.
    const CARD_HEIGHT_DEFAULT_CSS: f64 = 361.0;
    let measured_height = MEASURED_CARD_HEIGHT_CSS.load(Ordering::Acquire) as f64;
    let card_height_css = if measured_height >= 120.0 {
        measured_height
    } else {
        CARD_HEIGHT_DEFAULT_CSS
    };
    // The work area already ends exactly at the taskbar boundary. Keep the
    // visible card flush with that edge instead of adding another visual
    // margin; HIT_CUSHION_CSS still keeps the transparent native hit area
    // outside the card without moving the card away from the taskbar.
    const CARD_PAD_CSS: f64 = 0.0;
    const HIT_CUSHION_CSS: f64 = 24.0; // Query DPI from the monitor selected by the current click anchor. The
                                       // hidden window can retain the old monitor's DPI after a resolution or
                                       // display transition, so its own scale factor is not authoritative here.
    let target_monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: anchor_screen_point()
                    .map(|(x, _)| x as i32)
                    .unwrap_or(work.left),
                y: anchor_screen_point()
                    .map(|(_, y)| y as i32)
                    .unwrap_or(work.top),
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    if !target_monitor.is_invalid() {
        let _ =
            unsafe { GetDpiForMonitor(target_monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    }
    let scale = (dpi_x.max(dpi_y) as f64 / 96.0).max(0.5);
    let work_width = (work.right - work.left) as f64;
    let work_height = (work.bottom - work.top) as f64;
    let (cursor_x, cursor_y) = anchor_screen_point().unwrap_or_else(|| {
        let mut point = POINT::default();
        if unsafe { GetCursorPos(&mut point).is_ok() } {
            (point.x as f64, point.y as f64)
        } else {
            (work.right as f64, work.bottom as f64)
        }
    });
    // The anchor is already an absolute physical screen position. Convert it
    // directly into work-area-local CSS pixels. The previous implementation
    // normalized through the full monitor and then clamped the oversized
    // popup, which left only its fixed 24px card inset visible near the
    // monitor's left/top edge.
    let cursor_local_x = ((cursor_x - work.left as f64) / scale).clamp(0.0, work_width / scale);
    let cursor_local_y = ((cursor_y - work.top as f64) / scale).clamp(0.0, work_height / scale);
    let work_width_css = work_width / scale;
    let work_height_css = work_height / scale;
    let max_card_left = (work_width_css - CARD_PAD_CSS - CARD_WIDTH_CSS).max(CARD_PAD_CSS);
    let max_card_top = (work_height_css - CARD_PAD_CSS - card_height_css).max(CARD_PAD_CSS);
    let card_left = (cursor_local_x - CARD_WIDTH_CSS / 2.0).clamp(CARD_PAD_CSS, max_card_left);
    let card_top = (cursor_local_y - card_height_css / 2.0).clamp(CARD_PAD_CSS, max_card_top);
    // Capture the placement inputs so a later measured-height report can shift
    // the already-visible popup flush with the taskbar. The height delta is
    // applied relative to the current window rect, so the work area and cursor
    // do not need to be re-derived (the click anchor is consumed on show).
    if let Ok(mut guard) = LAST_PLACEMENT.lock() {
        *guard = Some(PlacementState {
            scale,
            used_height_css: card_height_css,
            bottom_flush: card_top >= max_card_top - 0.5,
            top_flush: card_top <= CARD_PAD_CSS + 0.5,
        });
    }
    let width = (INITIAL_VIEWPORT_WIDTH_CSS + HIT_CUSHION_CSS * 2.0) * scale;
    let height = (INITIAL_VIEWPORT_HEIGHT_CSS + HIT_CUSHION_CSS * 2.0) * scale;
    let desired_x = work.left as f64 + (card_left - HIT_CUSHION_CSS) * scale;
    let desired_y = work.top as f64 + (card_top - HIT_CUSHION_CSS) * scale;
    // Do not clamp the native viewport itself to the monitor. The visible
    // card was already clamped above, and the flyout can extend beyond the
    // card toward the monitor edge. Clamping the larger viewport shifts the
    // card away from the tray click even when the card itself fits perfectly.
    let x = desired_x;
    let y = desired_y;
    unsafe {
        SetWindowPos(
            HWND(hwnd.0),
            Some(HWND_TOPMOST),
            x.round() as i32,
            y.round() as i32,
            width.round() as i32,
            height.round() as i32,
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

// Chromium's native occlusion tracking can treat a transparent, topmost
// WebView2 window as an opaque occluder when it also has a custom window
// region. Marking the tray popup as a tool window makes it behave like a
// palette or tooltip, so it is ignored by that occlusion calculation. This
// preserves the region-based cross-process click-through used by the menu.
#[cfg(target_os = "windows")]
fn mark_as_tool_window(window: &WebviewWindow) -> Result<(), String> {
    let hwnd = window
        .hwnd()
        .map_err(|error| format!("could not get tray menu HWND: {error}"))?;
    unsafe {
        let current = GetWindowLongPtrW(HWND(hwnd.0), GWL_EXSTYLE);
        let updated = current | WS_EX_TOOLWINDOW.0 as isize | WS_EX_NOACTIVATE.0 as isize;
        SetWindowLongPtrW(HWND(hwnd.0), GWL_EXSTYLE, updated);
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn mark_as_tool_window(_window: &WebviewWindow) -> Result<(), String> {
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
    if let Err(error) = mark_as_tool_window(&window) {
        log::warn!("[tray] could not mark popup as a tool window: {error}");
    }
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
    if let Ok(mut guard) = APP_HANDLE.get_or_init(|| Mutex::new(None)).lock() {
        *guard = Some(app.clone());
    }
    let data = build_data(&app);
    set_current_data(data.clone());
    let Ok(window) = get_or_create_window(&app) else {
        clear_click_anchor();
        if let Ok(mut guard) = state().lock() {
            guard.open = false;
        }
        return;
    };
    // Position the hidden HWND exactly once. SetWindowPos uses the current
    // anchor, work area, and monitor DPI; the native no-activate show below
    // then reveals that final position without a stale intermediate frame.
    if let Err(error) = place_window(&window) {
        clear_click_anchor();
        log::warn!("[tray] custom menu show failed: {error}");
        if let Ok(mut guard) = state().lock() {
            guard.open = false;
        }
        return;
    }
    // Arm the tray suppression hook before showing the popup. The hook is
    // ready before the window becomes visible, so an immediate second
    // right-click cannot slip through to tray-icon's native context path.
    if let Ok(mut guard) = state().lock() {
        guard.open = true;
    }
    MENU_ACTIVE.store(true, Ordering::Release);
    crate::taskbar_dimmer::set_tray_menu_active(true);
    #[cfg(target_os = "windows")]
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let _ = ShowWindow(HWND(hwnd.0), SW_SHOWNOACTIVATE);
        }
    }
    clear_click_anchor();
    // Keep the popup non-activating. Edge remains the foreground window, so
    // its compositor and video frames continue updating while the menu is
    // open. WS_EX_NOACTIVATE is applied when the HWND is created, and this
    // placement uses SWP_NOACTIVATE as an additional guard.
    if let Ok(hwnd) = window.hwnd() {
        crate::taskbar_dimmer::set_flyout_hwnd(hwnd.0 as isize);
    }
    if let Ok(size) = window.outer_size() {
        log::info!(
            "[tray] custom menu opened (size {}x{}, edge {}, cursor {:.3}/{:.3})",
            size.width,
            size.height,
            data.taskbar_edge,
            data.cursor_x,
            data.cursor_y
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
