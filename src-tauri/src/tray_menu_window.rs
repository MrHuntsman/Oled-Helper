// tray_menu_window.rs, custom CSS-styled tray popup (replaces the native
// muda menu from tray_menu.rs for the right-click path).
//
// Built the same way as flyout_window.rs: a small undecorated/transparent
// Tauri WebviewWindow rendering real Svelte/CSS instead of a Win32
// TrackPopupMenu, anchored flush against the taskbar edge near the tray
// icon using the same Win32 SetWindowPos approach as flyout_window.rs's
// taskbar_position, rather than tauri-plugin-positioner's TrayCenter (which
// only centers on the tray icon at a fixed offset, not flush to the edge).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
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
    GetAncestor, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    GA_ROOT, GWL_EXSTYLE, GWL_STYLE, IsWindowVisible, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow,
    SW_HIDE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE,
    WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
};

pub(crate) const LABEL: &str = "tray-menu";

// Verbose tray lifecycle diagnostics for the open/hide/positioning paths.
// These log many lines per interaction, so they are emitted only when the
// Debug tab's "Detailed logging" toggle is on, same as the vendor/EDID
// diagnostics. Warnings (log::warn!) stay visible regardless.
macro_rules! tray_debug {
    ($($arg:tt)*) => {
        if crate::detailed_logging_enabled() {
            log::debug!($($arg)*);
        }
    };
}

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
static LAST_DISPLAY_CHANGE_MS: AtomicU64 = AtomicU64::new(0);

// How long a hidden tray menu may keep its webview (renderer + its share of
// Chromium's process set) after the last interaction. Hidden-but-alive costs
// roughly the renderer's commit even with the memory policy suspending it,
// and the suspension cannot touch the browser/GPU processes it pins; the only
// real release is destruction. One slower first open after each destroy is
// the price, same trade the tray teardown already makes for the main window.
const IDLE_DESTROY_MS: u64 = 60_000;
// Last time the menu was created, opened or asked to hide. The destroyer
// measures against this so a click that lands while it is deciding wins.
static LAST_USE_MS: AtomicU64 = AtomicU64::new(0);
static KEEP_TRAY_MENU_WARM: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_keep_tray_menu_warm(enabled: bool) {
    KEEP_TRAY_MENU_WARM.store(enabled, Ordering::Release);
}

pub(crate) fn keep_tray_menu_warm() -> bool {
    KEEP_TRAY_MENU_WARM.load(Ordering::Acquire)
}

#[tauri::command]
pub fn tray_menu_set_keep_warm(app: AppHandle, enabled: bool) {
    set_keep_tray_menu_warm(enabled);
    if enabled {
        prewarm(&app);
    } else if let Some(window) = app.get_webview_window(LABEL) {
        if !native_window_visible(&window) && tray_phase() == TrayMenuPhase::Hidden {
            if let Err(error) = window.destroy() {
                log::warn!("[tray-menu] could not release the hidden tray menu: {error}");
            } else {
                log::info!("[tray-menu] warm tray menu disabled; hidden webview released");
                reset_session_state();
            }
        }
    }
}

fn touch_usage() {
    LAST_USE_MS.store(now_ms(), Ordering::Release);
}

/// Starts the thread that destroys a tray-menu webview which has been hidden
/// and untouched for IDLE_DESTROY_MS, so its memory is actually returned.
/// Called once from setup(). The window is rebuilt on the next open; phase
/// state is reset with it.
pub(crate) fn start_idle_destroyer(app: &AppHandle) {
    let spawned = std::thread::Builder::new()
        .name("tray-menu-idle-destroy".into())
        .spawn({
            let app = app.clone();
            move || loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let Some(window) = app.get_webview_window(LABEL) else {
                    continue; // nothing to release; rebuilt on the next open
                };
                if native_window_visible(&window) || tray_phase() != TrayMenuPhase::Hidden {
                    // Open, mid-fade or mid-reveal: not idle in any way that
                    // matters.
                    continue;
                }
                if KEEP_TRAY_MENU_WARM.load(Ordering::Acquire) {
                    continue;
                }
                let elapsed = now_ms().saturating_sub(LAST_USE_MS.load(Ordering::Acquire));
                if elapsed < IDLE_DESTROY_MS {
                    continue;
                }
                // A click may have landed between the checks above and here.
                // The 3s grace window is far wider than any toggle path needs
                // to reach the show, so a fresh interaction is never destroyed
                // under the cursor.
                if now_ms().saturating_sub(LAST_USE_MS.load(Ordering::Acquire)) < 3_000 {
                    continue;
                }
                log::info!(
                    "[tray-menu] hidden for {}s; releasing the tray menu webview (rebuilt on next open)",
                    elapsed / 1000
                );
                if let Err(error) = window.destroy() {
                    log::warn!("[tray-menu] idle destroy failed: {error}");
                }
                reset_session_state();
            }
        });
    if let Err(error) = spawned {
        log::warn!("[tray-menu] could not start the idle destroyer: {error}");
    }
}

// Explicit lifecycle phase for the tray popup, tracked independently of
// Win32's IsWindowVisible. The native HWND is "visible" (per Windows) both
// while it is fading out after a hide request and while a deferred reveal
// is settling off-screen after a display/resolution change, so a raw
// visibility check cannot tell "really on screen" from "about to
// appear/disappear". toggle() used to treat any visible HWND as open and
// hide it, which meant a click landing during either transition was
// silently swallowed by the wrong branch and needed a follow-up click to
// reach the state the user actually wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayMenuPhase {
    Hidden = 0,
    // Native window shown (possibly off-screen) but not yet the final
    // on-screen state the user asked for.
    Revealing = 1,
    Shown = 2,
    // Hide requested; native window still on screen until the CSS fade
    // completes and the delayed thread actually hides it.
    Hiding = 3,
}

impl TrayMenuPhase {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => TrayMenuPhase::Revealing,
            2 => TrayMenuPhase::Shown,
            3 => TrayMenuPhase::Hiding,
            _ => TrayMenuPhase::Hidden,
        }
    }
}

static TRAY_PHASE: AtomicU8 = AtomicU8::new(TrayMenuPhase::Hidden as u8);

fn tray_phase() -> TrayMenuPhase {
    TrayMenuPhase::from_u8(TRAY_PHASE.load(Ordering::Acquire))
}

fn set_tray_phase(phase: TrayMenuPhase) {
    TRAY_PHASE.store(phase as u8, Ordering::Release);
}/// Forgets the phase recorded for the current window when that window is
/// destroyed for the tray teardown (see main_window). The next open builds a
/// fresh window and has to start from Hidden: a stale Shown/Hiding would make
/// the first click on the rebuilt menu hide it instead of showing it.
pub(crate) fn reset_session_state() {
    set_tray_phase(TrayMenuPhase::Hidden);
    MENU_GENERATION.fetch_add(1, Ordering::AcqRel);
}

// ── Focus acquisition and outside-click dismissal ─────────────────────────
//
// The menu is shown with raw ShowWindow(SW_SHOWNOACTIVATE) +
// SetForegroundWindow rather than window.show(), because Tauri's show path can
// briefly restore the decorated style before painting. That leaves two gaps
// the old WebviewWindow-built window did not have:
//
// 1. tao's set_focus() consults its *internal* VISIBLE window flag, which is
//    only set by window.show() (tao window_state.rs), never by a raw
//    ShowWindow, so window.set_focus() after a raw show is a silent no-op.
//    The WebView child therefore never holds keyboard focus, the HWND never
//    receives WM_KILLFOCUS when the user clicks elsewhere, and the
//    WindowEvent::Focused(false) dismissal never fires - the menu stayed on
//    screen until the user clicked the tray icon again.
// 2. SetForegroundWindow from a background thread does not move focus when
//    the calling thread owns no foreground window (Windows grants foreground
//    rights only to the thread that owns the current foreground window, or
//    one recently started by it). Standard fix: AttachThreadInput to the
//    current foreground thread, force SetFocus, then detach.
//
// The foreground watcher below closes the loop for good: while the menu is
// shown it polls which window has focus, and the moment focus lands on any
// window other than the menu (a click elsewhere, another app, the taskbar)
// it plays the regular hide. This is the dismissal users expect from every
// Windows context menu; the blur event remains as the first line and the
// watcher as the guarantee.

/// Gives the tray menu's HWND real keyboard focus, attaching to the current
/// foreground thread first so Windows permits the focus change (see above).
#[cfg(target_os = "windows")]
fn force_focus(hwnd: HWND) {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    unsafe {
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let this_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != this_thread
            && AttachThreadInput(this_thread, foreground_thread, true).as_bool();
        let _ = SetFocus(Some(hwnd));
        if attached {
            let _ = AttachThreadInput(this_thread, foreground_thread, false);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn force_focus(_: ()) {}

/// Polls foreground focus while the tray menu is shown and hides it the
/// moment the foreground window is anything but the menu. One thread per
/// open, same lifecycle as the blur handler's poll: created when the menu
/// reaches Shown, gone as soon as the menu hides (or the generation moves,
/// i.e. a fast-forward reopened it).
///
/// GetFocus is deliberately not used here: it reports the *calling thread's*
/// focused window, which is always NULL from this worker thread. The
/// foreground window is the process-wide truth, and its root ancestor is
/// compared against the menu's HWND so the WebView child (or any descendant)
/// holding the foreground still counts as "the menu".
#[cfg(target_os = "windows")]
fn start_foreground_watcher(window: &tauri::WebviewWindow, generation: u64) {
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    let menu_hwnd = hwnd.0 as isize;
    let delayed_window = window.clone();
    std::thread::Builder::new()
        .name("tray-menu-focus-watch".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(120));
                if MENU_GENERATION.load(Ordering::Acquire) != generation {
                    return; // re-opened or reset: the new session owns the menu
                }
                if tray_phase() != TrayMenuPhase::Shown {
                    return; // already hiding/hid
                }
                // No foreground window at all is a transient state (menu
                // closing on its own, lock screen): not a dismissal.
                let foreground = unsafe { GetForegroundWindow() };
                if foreground.0.is_null() {
                    continue;
                }
                let foreground_root = unsafe { GetAncestor(foreground, GA_ROOT) };
                if foreground_root.0 as isize == menu_hwnd {
                    continue;
                }
                tray_debug!("[tray-debug] focus watcher: foreground left the menu, hiding");
                request_tray_hide(&delayed_window);
                return;
            }
        })
        .ok();
}

#[cfg(not(target_os = "windows"))]
fn start_foreground_watcher(_window: &tauri::WebviewWindow, _generation: u64) {}

// Opens whose last show predates a real display change use the deferred
// reveal path (see schedule_deferred_reveal): Windows defers a hidden
// window's DPI transition until its next show, so the first show after a
// resolution switch lands at stale geometry and can briefly re-create the
// non-client frame. The deferred path keeps the window off-screen until the
// geometry settles, so neither the stale position nor a transient title bar
// can ever be painted on-screen.
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
// How long the frontend's CSS fade takes (.tray-menu-wrap opacity
// transition). Every hide path keeps the native window visible for this long
// so the fade-out is actually visible before the HWND disappears.
const TRAY_FADE_OUT_MS: u64 = 170;
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
fn log_native_window_state(window: &tauri::WebviewWindow, phase: &str) {
    // Pure diagnostics: skip the Win32 reads entirely unless detailed
    // logging is enabled, since this runs on every open and recovery pass.
    if !crate::detailed_logging_enabled() {
        return;
    }
    let Ok(hwnd) = window.hwnd() else { return };
    unsafe {
        let style = GetWindowLongPtrW(HWND(hwnd.0), GWL_STYLE) as u32;
        let ex_style = GetWindowLongPtrW(HWND(hwnd.0), GWL_EXSTYLE) as u32;
        let mut rect = windows::Win32::Foundation::RECT::default();
        let has_rect = GetWindowRect(HWND(hwnd.0), &mut rect).is_ok();
        let outer = window.outer_size().ok();
        let inner = window.inner_size().ok();
        log::info!(
            "[tray-debug] native state phase={phase}, hwnd={:?}, visible={}, style=0x{style:08X}, ex_style=0x{ex_style:08X}, rect={}x{}+{},{} outer={}x{} inner={}x{}",
            hwnd.0,
            window.is_visible().unwrap_or(false),
            if has_rect { rect.right - rect.left } else { 0 },
            if has_rect { rect.bottom - rect.top } else { 0 },
            if has_rect { rect.left } else { 0 },
            if has_rect { rect.top } else { 0 },
            outer.map(|s| s.width).unwrap_or(0),
            outer.map(|s| s.height).unwrap_or(0),
            inner.map(|s| s.width).unwrap_or(0),
            inner.map(|s| s.height).unwrap_or(0),
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn log_native_window_state(_window: &tauri::WebviewWindow, _phase: &str) {}

fn native_window_visible(window: &tauri::WebviewWindow) -> bool {
    let Ok(hwnd) = window.hwnd() else { return false };
    unsafe { IsWindowVisible(HWND(hwnd.0)).as_bool() }
}

#[cfg(not(target_os = "windows"))]
fn native_window_visible(window: &tauri::WebviewWindow) -> bool {
    window.is_visible().unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn native_window_hide(window: &tauri::WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else { return };
    unsafe {
        let _ = ShowWindow(HWND(hwnd.0), SW_HIDE);
    }
    // Release the hidden webview's memory only once the fade-out has finished:
    // suspending the renderer earlier would freeze the CSS transition.
    crate::webview_env::set_inactive(window.app_handle(), LABEL);
}

#[cfg(not(target_os = "windows"))]
fn native_window_hide(window: &tauri::WebviewWindow) {
    let _ = window.hide();
    crate::webview_env::set_inactive(window.app_handle(), LABEL);
}

// Hide the menu with its CSS fade-out: notify the frontend so it can play
// the opacity transition, bump the menu generation so every pending
// show-retry/anchor/deferred-reveal thread for the current open stops (they
// only check native visibility, which stays true during the fade and would
// otherwise re-emit tray-menu-show mid-fade), then hide the native window
// once the fade has completed.
fn request_tray_hide(window: &tauri::WebviewWindow) {
    // This hide owns the generation it bumps to. The delayed native hide
    // below stands down if a newer session (a re-open) bumped the generation
    // in the meantime: without this guard, a blur dismissal's pending hide
    // fires ~170ms later and hides a menu the user just reopened, which reads
    // as "opens then closes itself" (and as "doesn't open" when the click
    // lands while the fade-out is still running).
    let generation = MENU_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    // The hide itself is a use: the destroyer must not decide the menu has
    // been idle since before the user last touched it.
    touch_usage();
    set_tray_phase(TrayMenuPhase::Hiding);
    match window.emit_to(LABEL, "tray-menu-hide", ()) {
        Ok(()) => tray_debug!("[tray-debug] hide event emitted"),
        Err(error) => log::warn!("[tray-debug] hide event emit failed: {error}"),
    }
    let delayed_window = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(TRAY_FADE_OUT_MS));
        if MENU_GENERATION.load(Ordering::Acquire) != generation {
            tray_debug!(
                "[tray-debug] stale native hide skipped generation={generation}, current_generation={}",
                MENU_GENERATION.load(Ordering::Acquire)
            );
            return;
        }
        native_window_hide(&delayed_window);
        set_tray_phase(TrayMenuPhase::Hidden);
        tray_debug!(
            "[tray-debug] native hide requested, visible_after={}",
            native_window_visible(&delayed_window)
        );
    });
}

#[cfg(target_os = "windows")]
fn force_borderless_window(window: &tauri::WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else { return };
    unsafe {
        let old_style = GetWindowLongPtrW(HWND(hwnd.0), windows::Win32::UI::WindowsAndMessaging::GWL_STYLE);
        log_native_window_state(window, "before-borderless");
        let frame_bits = (WS_CAPTION.0
            | WS_THICKFRAME.0
            | WS_SYSMENU.0
            | WS_MINIMIZEBOX.0
            | WS_MAXIMIZEBOX.0) as isize;
        let new_style = (old_style & !frame_bits) | WS_POPUP.0 as isize;
        if new_style != old_style {
            SetWindowLongPtrW(
                HWND(hwnd.0),
                windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
                new_style,
            );
            let _ = SetWindowPos(
                HWND(hwnd.0),
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
            );
        }
        log_native_window_state(window, "after-borderless");
    }
}

#[cfg(not(target_os = "windows"))]
fn force_borderless_window(_window: &tauri::WebviewWindow) {}

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

#[cfg(target_os = "windows")]
fn move_window_offscreen(window: &tauri::WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else { return };
    // The hidden HWND can retain its previous monitor position across a DPI or
    // resolution change. Show it outside every normal desktop coordinate, then
    // anchor it after Windows has applied the new display context. This avoids
    // exposing the old position for even one painted frame.
    unsafe {
        let _ = SetWindowPos(
            HWND(hwnd.0),
            None,
            -100_000,
            -100_000,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn move_window_offscreen(_window: &tauri::WebviewWindow) {}

fn get_or_create_window(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    tray_debug!("[tray-debug] get_or_create_window begin");
    if let Some(window) = app.get_webview_window(LABEL) {
        let visible = native_window_visible(&window);
        tray_debug!("[tray-debug] existing window found, visible={visible}");
        // Reassert the fixed logical canvas on every open. This repairs a
        // native size retained across a remote-display session without ever
        // resizing the window while it is visible.
        if !visible {
            let _ = window.set_size(LogicalSize::new(FULL_WIDTH, FULL_HEIGHT));
        }
        return Ok(window);
    }
    log::info!("[tray-menu] creating tray menu window");
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
    // Must match the main window's arguments exactly: WebView2 only shares one
    // environment (and so one browser process) between webviews whose browser
    // arguments are identical, and refuses to share one when they differ.
    if let Some(args) = crate::webview_env::shared_browser_args(app) {
        builder = builder.additional_browser_args(&args);
    }
    tray_debug!("[tray-debug] building tray window");
    let window = builder
        .build()
        .map_err(|e| format!("could not create tray menu window: {e}"))?;
    tray_debug!("[tray-debug] tray window built");
    // Tauri's decoration flag is normally sufficient, but Windows can briefly
    // recreate the non-client frame during the first show after a DPI change.
    // Remove caption and frame styles directly while the window is still
    // hidden, so the title bar can never be painted for one frame.
    force_borderless_window(&window);
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
            tray_debug!("[tray-debug] native focus lost, generation={}", MENU_GENERATION.load(Ordering::Acquire));
            // Capture the menu generation so an old blur callback can never
            // hide a newly reopened tray menu.
            //
            // The frontend closes an open submenu asynchronously: mouseleave
            // schedules a ~100ms bridge (scheduleSubmenuClose) before it even
            // calls closeSubmenu(), which then round-trips an IPC call
            // (syncHitRegion -> tray_menu_set_expanded) to clear
            // SUBMENU_EXPANDED on this side. A single fixed-delay check here
            // used to run before that finished often enough that an outside
            // click left the menu stuck open. Poll instead, but only for
            // this one blur event of this one open tray menu (this thread
            // only exists because the window just had focus, i.e. was open),
            // so there is no background polling while the menu is closed.
            let generation = MENU_GENERATION.load(Ordering::Acquire);
            let delayed_window = blur_window.clone();
            std::thread::spawn(move || {
                const POLL_INTERVAL_MS: u64 = 50;
                const MAX_WAIT_MS: u64 = 300;
                let mut waited_ms: u64 = 0;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
                    waited_ms += POLL_INTERVAL_MS;
                    if MENU_GENERATION.load(Ordering::Acquire) != generation {
                        tray_debug!("[tray-debug] blur poll stopped, generation changed, waited_ms={waited_ms}");
                        return;
                    }
                    let submenu_expanded = SUBMENU_EXPANDED.load(Ordering::Acquire);
                    let too_soon = LAST_SHOW_AT_MS.load(Ordering::Acquire).saturating_add(250) > now_ms();
                    if !submenu_expanded && !too_soon {
                        tray_debug!("[tray-debug] delayed blur hiding generation={generation}, waited_ms={waited_ms}");
                        request_tray_hide(&delayed_window);
                        return;
                    }
                    if waited_ms >= MAX_WAIT_MS {
                        tray_debug!("[tray-debug] blur poll timed out, hiding anyway generation={generation}, submenu_expanded={submenu_expanded}, too_soon={too_soon}");
                        request_tray_hide(&delayed_window);
                        return;
                    }
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
    tray_debug!("[tray-debug] prewarm begin");
    touch_usage();
    if let Err(e) = get_or_create_window(app) {
        log::warn!("[tray-menu] prewarm failed: {e}");
        return;
    }
    // Build the first snapshot while the tray window is still hidden. Later
    // refreshes are event-driven, so the click path never performs these reads.
    let taskbar_state = app.state::<crate::taskbar_dimmer::TaskbarDimmerState>();
    let snapshot = collect_tray_menu_state(app, taskbar_state);
    tray_debug!("[tray-debug] prewarm snapshot collected");
    let payload = serde_json::to_value(&snapshot).unwrap_or(Value::Null);
    if let Ok(mut cached) = CACHED_TRAY_STATE.get_or_init(|| Mutex::new(None)).lock() {
        *cached = Some(payload);
        tray_debug!("[tray-debug] prewarm cache populated");
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
                || !native_window_visible(&delayed_window)
            {
                return;
            }
            tray_debug!("[tray-debug] show retry generation={generation}, delay_ms={delay_ms}, visible={}", native_window_visible(&delayed_window));
            match delayed_window.emit_to(LABEL, "tray-menu-show", payload.clone()) {
                Ok(()) => tray_debug!("[tray-debug] show retry emitted generation={generation}"),
                Err(error) => log::warn!("[tray-debug] show retry emit failed generation={generation}: {error}"),
            }
        }
    });
}

fn stabilize_canvas(window: &tauri::WebviewWindow, generation: u64, phase: &str) {
    // Reassert the fixed logical canvas after Windows applies a new DPI
    // context. This keeps the native transparent surface aligned with the
    // WebView coordinate system before the measured region is installed.
    match window.set_size(LogicalSize::new(FULL_WIDTH, FULL_HEIGHT)) {
        Ok(()) => tray_debug!("[tray-debug] canvas stabilized generation={generation}, phase={phase}"),
        Err(error) => log::warn!("[tray-debug] canvas stabilization failed generation={generation}, phase={phase}: {error}"),
    }
}

#[cfg(not(target_os = "windows"))]
fn stabilize_canvas(_window: &tauri::WebviewWindow, _generation: u64, _phase: &str) {}

/// Re-anchor the already-visible window while Windows finishes applying a
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
                || !native_window_visible(&delayed_window)
            {
                return;
            }
            stabilize_canvas(&delayed_window, generation, "post-show-recovery");
            force_borderless_window(&delayed_window);
            anchor_to_taskbar(&delayed_window, anchor_x_px);
        }
    });
}

#[cfg(target_os = "windows")]
fn native_window_rect(window: &tauri::WebviewWindow) -> Option<(i32, i32, i32, i32)> {
    let Ok(hwnd) = window.hwnd() else { return None };
    let mut rect = windows::Win32::Foundation::RECT::default();
    unsafe {
        GetWindowRect(HWND(hwnd.0), &mut rect).ok()?;
    }
    Some((rect.left, rect.top, rect.right, rect.bottom))
}

#[cfg(not(target_os = "windows"))]
fn native_window_rect(_window: &tauri::WebviewWindow) -> Option<(i32, i32, i32, i32)> {
    None
}

/// First-open-after-display-change path: keep the menu hidden (the frontend
/// wrapper only becomes visible on the tray-menu-show event) while the
/// sizing passes below re-derive the window's geometry under the new DPI
/// context. The native window itself stays off-screen during these passes:
/// anchoring it on-screen before the geometry settles would flash the stale
/// position and any transient frame Windows re-created during the DPI
/// transition. The reveal is emitted exactly once, once two consecutive
/// passes agree on the native rect after the rect provably changed from the
/// stale post-show baseline; the reveal pass then anchors the window at its
/// final position. If the geometry never changes (it was already correct),
/// the 280ms cumulative escape hatch reveals, and the final pass always
/// reveals as a fallback. The passes are cumulative (~40+60+80+100+120ms),
/// so a typical transition reveals around 100ms instead of waiting seconds.
fn schedule_deferred_reveal(
    window: &tauri::WebviewWindow,
    payload: Value,
    anchor_x_px: Option<i32>,
    generation: u64,
    initial_rect: Option<(i32, i32, i32, i32)>,
) {
    let delayed_window = window.clone();
    std::thread::spawn(move || {
        let mut previous_rect: Option<(i32, i32, i32, i32)> = None;
        let mut elapsed_ms: u64 = 0;
        let mut revealed = false;
        for step_ms in [40_u64, 60, 80, 100, 120] {
            std::thread::sleep(std::time::Duration::from_millis(step_ms));
            elapsed_ms += step_ms;
            if MENU_GENERATION.load(Ordering::Acquire) != generation
                || !native_window_visible(&delayed_window)
            {
                return;
            }
            stabilize_canvas(&delayed_window, generation, "post-show-recovery");
            force_borderless_window(&delayed_window);
            // The window stays off-screen while its geometry is settling:
            // anchoring it on-screen here would flash the stale position (and
            // any transient frame) before the reveal decides where it belongs.
            // Once revealed, every pass re-anchors normally.
            if revealed {
                anchor_to_taskbar(&delayed_window, anchor_x_px);
            }
            let rect = native_window_rect(&delayed_window);
            tray_debug!(
                "[tray-debug] deferred anchor pass generation={generation}, elapsed_ms={elapsed_ms}, rect={}x{}+{},{} initial={}x{}+{},{}",
                rect.map(|r| r.2 - r.0).unwrap_or(0),
                rect.map(|r| r.3 - r.1).unwrap_or(0),
                rect.map(|r| r.0).unwrap_or(0),
                rect.map(|r| r.1).unwrap_or(0),
                initial_rect.map(|r| r.2 - r.0).unwrap_or(0),
                initial_rect.map(|r| r.3 - r.1).unwrap_or(0),
                initial_rect.map(|r| r.0).unwrap_or(0),
                initial_rect.map(|r| r.1).unwrap_or(0),
            );
            let changed_from_initial = rect.is_some() && initial_rect != rect;
            let stable = rect.is_some() && previous_rect == rect;
            previous_rect = rect;
            // Reveal once the geometry provably changed off the stale
            // baseline and then held still for two passes, or as an escape
            // hatch when it never changed (it was already correct).
            let should_reveal = (stable && changed_from_initial) || elapsed_ms >= 280;
            if !revealed && should_reveal {
                revealed = true;
                // First on-screen appearance: the geometry has settled, so
                // anchor at the final position, re-strip any frame the DPI
                // transition may have re-created, and only then reveal.
                anchor_to_taskbar(&delayed_window, anchor_x_px);
                force_borderless_window(&delayed_window);
                set_tray_phase(TrayMenuPhase::Shown);
                start_foreground_watcher(&delayed_window, generation);
                match delayed_window.emit_to(LABEL, "tray-menu-show", payload.clone()) {
                    Ok(()) => tray_debug!(
                        "[tray-debug] deferred reveal emitted generation={generation}, elapsed_ms={elapsed_ms}, stable={stable}, changed_from_initial={changed_from_initial}"
                    ),
                    Err(error) => log::warn!(
                        "[tray-debug] deferred reveal emit failed generation={generation}: {error}"
                    ),
                }
            }
        }
        if !revealed {
            anchor_to_taskbar(&delayed_window, anchor_x_px);
            force_borderless_window(&delayed_window);
            set_tray_phase(TrayMenuPhase::Shown);
            start_foreground_watcher(&delayed_window, generation);
            match delayed_window.emit_to(LABEL, "tray-menu-show", payload) {
                Ok(()) => tray_debug!("[tray-debug] deferred reveal emitted generation={generation}, elapsed_ms=final"),
                Err(error) => log::warn!("[tray-debug] deferred reveal emit failed generation={generation}: {error}"),
            }
        }
    });
}

/// Fast-forward a tray window that is mid-transition (still fading out from
/// a hide, or still settling off-screen after a deferred reveal) straight to
/// fully shown. Called when toggle() is invoked again while in that state:
/// without this, the click would be misread as "close" (IsWindowVisible
/// reads true in both Hiding and Revealing), cancelling the transition
/// already in flight and leaving nothing on screen, which is what made the
/// popup need a follow-up click (or two) to actually appear.
fn reveal_now(window: &tauri::WebviewWindow, anchor_x_px: Option<i32>) {
    // Bump the generation to invalidate whatever hide/reveal thread is
    // currently in flight for this window; this call takes over from here.
    let generation = MENU_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    LAST_SHOW_AT_MS.store(now_ms(), Ordering::Release);
    let payload = cached_tray_state().unwrap_or(Value::Null);
    // Mid-transition fast-forward: resume before the emit below so the
    // renderer can act on it.
    crate::webview_env::set_active(window.app_handle(), LABEL);
    stabilize_canvas(window, generation, "reveal-now");
    force_borderless_window(window);
    anchor_to_taskbar(window, anchor_x_px);
    #[cfg(target_os = "windows")]
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let hwnd = HWND(hwnd.0);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetForegroundWindow(hwnd);
        }
        // Raw ShowWindow never sets tao's internal VISIBLE flag, so
        // window.set_focus() below is a silent no-op (tao's set_focus checks
        // that flag). Force real keyboard focus the Win32 way instead.
        force_focus(HWND(hwnd.0));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window.show();
    }
    match window.emit_to(LABEL, "tray-menu-show", payload.clone()) {
        Ok(()) => tray_debug!("[tray-debug] reveal-now show emitted generation={generation}"),
        Err(error) => log::warn!("[tray-debug] reveal-now show emit failed generation={generation}: {error}"),
    }
    match window.set_focus() {
        Ok(()) => tray_debug!("[tray-debug] reveal-now focus request succeeded"),
        Err(error) => log::warn!("[tray-debug] reveal-now focus request failed: {error}"),
    }
    set_tray_phase(TrayMenuPhase::Shown);
    schedule_post_show_event(window, payload, generation);
    schedule_post_show_anchor(window, anchor_x_px, generation);
    start_foreground_watcher(window, generation);
}

pub fn toggle(app: &AppHandle, anchor_x: f64) {
    tray_debug!("[tray-debug] toggle begin anchor_x={anchor_x:.1}");
    // Any interaction counts as use, including the click that opens the menu
    // after an idle destroy: the 3s grace window in the destroyer is measured
    // from here, so it can never tear down a window this call is about to use.
    touch_usage();
    let window = match get_or_create_window(app) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("[tray-menu] could not open tray menu: {e}");
            return;
        }
    };
    let phase = tray_phase();
    tray_debug!("[tray-debug] toggle phase={phase:?}");
    match phase {
        TrayMenuPhase::Shown => {
            tray_debug!("[tray-debug] hiding shown tray window");
            request_tray_hide(&window);
            return;
        }
        TrayMenuPhase::Hiding | TrayMenuPhase::Revealing => {
            // IsWindowVisible would also read true here, which is exactly
            // the ambiguity phase tracking exists to resolve: the window is
            // on screen (or about to be), so this click means "show it
            // now", not "hide the thing I can already see".
            tray_debug!("[tray-debug] fast-forwarding {phase:?} tray window to shown");
            let anchor = Some(anchor_x.round() as i32);
            reveal_now(&window, anchor);
            return;
        }
        TrayMenuPhase::Hidden => {}
    }
    MENU_GENERATION.fetch_add(1, Ordering::AcqRel);
    set_tray_phase(TrayMenuPhase::Revealing);
    tray_debug!("[tray-debug] toggle generation={}", MENU_GENERATION.load(Ordering::Acquire));
    SUBMENU_EXPANDED.store(false, Ordering::Release);
    // A display change since the window was last shown means its DPI context
    // is stale: Windows applies a hidden window's DPI transition at the next
    // show, which lands at stale geometry and can briefly re-create the
    // non-client frame. Use the deferred reveal in that case; it keeps the
    // window off-screen until the geometry settles. Read the previous show
    // time before storing the new one.
    let last_show_at = LAST_SHOW_AT_MS.load(Ordering::Acquire);
    LAST_SHOW_AT_MS.store(now_ms(), Ordering::Release);
    // A click outside the region can hide the window before Svelte gets a
    // chance to close its submenu state. Reset the native region before the
    // next show so a stale expanded shape can never intercept the first frame.
    apply_default_hit_region(&window);
    // Resume the webview before the show event below: a suspended renderer
    // cannot process it until it is resumed.
    crate::webview_env::set_active(app, LABEL);
    let payload = cached_tray_state();
    tray_debug!("[tray-debug] cache lookup complete, has_payload={}", payload.is_some());
    let payload = payload.unwrap_or(Value::Null);
    let deferred_reveal =
        LAST_DISPLAY_CHANGE_MS.load(Ordering::Acquire) > last_show_at;
    if !deferred_reveal {
        match window.emit_to(LABEL, "tray-menu-show", payload.clone()) {
            Ok(()) => tray_debug!("[tray-debug] initial show event emitted"),
            Err(error) => log::warn!("[tray-debug] initial show event emit failed: {error}"),
        }
    } else {
        tray_debug!("[tray-debug] initial show event deferred after display change");
    }
    let anchor = Some(anchor_x.round() as i32);
    move_window_offscreen(&window);
    log_native_window_state(&window, "before-show");
    // Tauri's show() path can temporarily restore the configured decorated
    // style before painting. Use ShowWindow directly after the borderless
    // style has been installed, so the first visible frame is borderless too.
    #[cfg(target_os = "windows")]
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let hwnd = HWND(hwnd.0);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetForegroundWindow(hwnd);
        }
        // Raw ShowWindow never sets tao's internal VISIBLE flag, so the
        // window.set_focus() at the end of this function is a silent no-op
        // (tao's set_focus checks that flag, which only window.show() sets).
        // Without real keyboard focus the HWND never sees WM_KILLFOCUS, the
        // Focused(false) dismissal never fires and the menu only ever closed
        // via the tray icon. Force it the Win32 way.
        force_focus(HWND(hwnd.0));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window.show();
    }
    tray_debug!("[tray-debug] native show succeeded, visible_after={}", native_window_visible(&window));
    log_native_window_state(&window, "after-show");
    // Tauri or Windows can reapply the configured decorated style as part of
    // show(). Remove it again immediately, before the WebView presents the
    // first painted frame with a title bar.
    force_borderless_window(&window);
    // On the deferred path the window stays off-screen until the settle
    // passes decide where it belongs: anchoring now would expose the stale
    // geometry (and any transient frame Windows re-created during the DPI
    // transition) on-screen for the 40-280ms before the first recovery pass.
    if !deferred_reveal {
        anchor_to_taskbar(&window, anchor);
        // The immediate path puts the window on screen at its final
        // position right here; the deferred path only reaches Shown once
        // schedule_deferred_reveal actually reveals it (see below).
        set_tray_phase(TrayMenuPhase::Shown);
    }
    log_native_window_state(&window, "after-borderless-show");
    // The geometry right after show() can be stale (the window's DPI context
    // is still transitioning), so remember it as the baseline: the deferred
    // reveal waits until it provably changed from this stale baseline and
    // then held still.
    let initial_rect = native_window_rect(&window);
    let generation = MENU_GENERATION.load(Ordering::Acquire);
    stabilize_canvas(&window, generation, "after-show");
    if deferred_reveal {
        schedule_deferred_reveal(&window, payload, anchor, generation, initial_rect);
    } else {
        schedule_post_show_event(&window, payload, generation);
        schedule_post_show_anchor(&window, anchor, generation);
    }
    match window.set_focus() {
        Ok(()) => tray_debug!("[tray-debug] focus request succeeded"),
        Err(error) => log::warn!("[tray-debug] focus request failed: {error}"),
    }
    // Both dismissal mechanisms (the Focused(false) handler and this watcher)
    // depend on the menu actually holding foreground focus; the watcher is
    // also the catch-all if focus acquisition failed anyway. Deferred-reveal
    // opens start it from their reveal step instead: the phase is not Shown
    // yet here, and the watcher would exit on its first poll.
    if !deferred_reveal {
        start_foreground_watcher(&window, generation);
    }
    tray_debug!("[tray-debug] toggle complete generation={generation}, visible_after={}", native_window_visible(&window));
}

pub fn hide(app: &AppHandle) {
    tray_debug!("[tray-debug] hide requested");
    touch_usage();
    if let Some(window) = app.get_webview_window(LABEL) {
        request_tray_hide(&window);
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
    tray_debug!("[tray-debug] frontend {message}");
}

/// Ungated first-open HDR report. The tray's per-event diagnostics are gated
/// behind Detailed logging (tray_menu_debug), which made the first-open HDR
/// state invisible in normal runs and hard to diagnose. This fires once per
/// first open and logs unconditionally so a repro produces the decision chain
/// (snapshot vs native query vs broadcast) without enabling the toggle.
#[tauri::command]
pub fn tray_menu_hdr_report(message: String) {
    log::info!("[tray-hdr] {message}");
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
    LAST_DISPLAY_CHANGE_MS.store(now_ms(), Ordering::Release);
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
/// main_window::reveal also rebuilds the main webview when the tray teardown
/// took it (opening the tray menu builds webviews again, but the main window's
/// only comes back here or from the tray icon).
#[tauri::command]
pub fn tray_menu_open_main(app: AppHandle) {
    hide(&app);
    crate::main_window::reveal(&app);
}

#[tauri::command]
pub fn tray_menu_quit(app: AppHandle) {
    hide(&app);
    crate::request_shutdown_and_exit(&app);
}
