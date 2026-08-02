use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, State};

#[derive(Clone, Serialize)]
pub struct RuntimeLogEvent {
    pub level: String,
    pub message: String,
}

pub struct TaskbarDimmerState {
    pub enabled: Mutex<bool>,
    pub opacity: Mutex<f64>,
    pub ease_duration_ms: Mutex<u64>,
    // Store the native HWND directly. A separate WebView2 window can exist
    // without having a usable HWND during startup, which made the old overlay
    // fail when the user enabled the dimmer.
    pub overlay: Mutex<Option<isize>>,
}

impl Default for TaskbarDimmerState {
    fn default() -> Self {
        Self {
            enabled: Mutex::new(false),
            opacity: Mutex::new(0.95),
            ease_duration_ms: Mutex::new(250),
            overlay: Mutex::new(None),
        }
    }
}

#[derive(Serialize)]
pub struct TaskbarDimmerStatus {
    pub enabled: bool,
    pub opacity: f64,
    pub ease_duration_ms: u64,
    pub available: bool,
}

#[cfg(target_os = "windows")]
mod platform {
    use std::ffi::c_void;
    use std::sync::{atomic::{AtomicBool, Ordering}, Mutex, OnceLock};
    use tauri::{AppHandle, Emitter};
    use super::RuntimeLogEvent;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, EndPaint, FillRect, GetStockObject, GetMonitorInfoW, HBRUSH, MONITORINFO,
        MonitorFromWindow, BLACK_BRUSH, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, FindWindowW, GetWindowLongPtrW, GetWindowRect,
        RegisterClassW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
        GetCursorPos, GetForegroundWindow, IsIconic, IsWindowVisible, SetTimer,
        KillTimer, ShowWindow, GWLP_HWNDPARENT, GWL_EXSTYLE, GWL_STYLE, HWND_TOPMOST, LWA_ALPHA, SW_HIDE,
        SET_WINDOW_POS_FLAGS, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOSENDCHANGING, SWP_SHOWWINDOW, SW_SHOW,
        WS_MAXIMIZE, WM_NCHITTEST, WM_PAINT, WM_TIMER, WS_CAPTION, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP, WS_THICKFRAME,
        WNDCLASSW, HTTRANSPARENT,
    };
    use windows::core::PCWSTR;

    const TIMER_ID: usize = 1;
    // 30 Hz is enough for a smooth short fade while keeping cursor polling
    // and layered-window updates well below the rendering path's workload.
    const TIMER_INTERVAL_MS: u32 = 33;
    const DEFAULT_EASE_DURATION_MS: u64 = 250;
    const MIN_EASE_DURATION_MS: u64 = 0;
    const MAX_EASE_DURATION_MS: u64 = 1000;

    struct FadeState {
        enabled: bool,
        base_alpha: f64,
        ease_duration_ms: u64,
        current_alpha: f64,
        target_alpha: f64,
        last_tick: Instant,
        timer_started: bool,
        last_hovered: Option<bool>,
        last_diagnostic: Instant,
    }

    static FADE_STATE: OnceLock<Mutex<FadeState>> = OnceLock::new();
    static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();
    static VERBOSE_LOGGING: AtomicBool = AtomicBool::new(false);

    pub fn set_verbose_logging(enabled: bool) {
        VERBOSE_LOGGING.store(enabled, Ordering::Relaxed);
    }

    fn emit_runtime(level: &str, message: String) {
        match level {
            "warn" => log::warn!("{message}"),
            "error" => log::error!("{message}"),
            _ => log::info!("{message}"),
        }
        if let Some(app) = APP_HANDLE.get() {
            let _ = app.emit("runtime-log", RuntimeLogEvent {
                level: level.to_string(),
                message,
            });
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn class_name() -> &'static [u16] {
        static NAME: OnceLock<Vec<u16>> = OnceLock::new();
        NAME.get_or_init(|| wide("OledHelperTaskbarDimmer"))
    }

    unsafe extern "system" fn overlay_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            WM_TIMER => {
                update_fade(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut paint);
                let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
                FillRect(hdc, &paint.rcPaint, black);
                let _ = EndPaint(hwnd, &paint);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }

    fn register_class() {
        let name = class_name();
        let class = WNDCLASSW {
            lpfnWndProc: Some(overlay_window_proc),
            lpszClassName: PCWSTR(name.as_ptr()),
            ..Default::default()
        };
        unsafe {
            // A second registration simply means this process already created
            // its overlay class; the existing class is still valid.
            let _ = RegisterClassW(&class);
        }
    }

    fn taskbar_info() -> Option<(HWND, RECT)> {
        unsafe {
            let class = wide("Shell_TrayWnd");
            let hwnd = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()).ok()?;
            let mut rect = RECT::default();
            GetWindowRect(hwnd, &mut rect).ok()?;
            Some((hwnd, rect))
        }
    }

    // Windows popup menus, including the native Tauri tray menu, use the
    // standard #32768 window class. The taskbar overlay must sit below this
    // popup or it can cover the menu and intercept its visual surface.
    fn open_popup_menu() -> Option<HWND> {
        unsafe {
            let class = wide("#32768");
            FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()).ok()
        }
    }

    pub fn available() -> bool {
        taskbar_info().is_some()
    }

    // Returns true when another application occupies the entire monitor in
    // exclusive fullscreen or borderless fullscreen mode. Normal maximized
    // windows retain caption/frame styles and are intentionally excluded.
    fn fullscreen_app_active(overlay: HWND) -> bool {
        unsafe {
            let foreground = GetForegroundWindow();
            if foreground.0.is_null() || foreground == overlay {
                return false;
            }

            let mut process_id = 0u32;
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                foreground,
                Some(&mut process_id),
            );
            if process_id == std::process::id() || process_id == 0 || IsIconic(foreground).as_bool() {
                return false;
            }

            let mut window_rect = RECT::default();
            if GetWindowRect(foreground, &mut window_rect).is_err() {
                return false;
            }
            let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
            if monitor.0.is_null() {
                return false;
            }
            let mut monitor_info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut monitor_info).as_bool() {
                return false;
            }

            let covers_monitor = window_rect.left <= monitor_info.rcMonitor.left
                && window_rect.top <= monitor_info.rcMonitor.top
                && window_rect.right >= monitor_info.rcMonitor.right
                && window_rect.bottom >= monitor_info.rcMonitor.bottom;
            if !covers_monitor {
                return false;
            }

            let style = GetWindowLongPtrW(foreground, GWL_STYLE) as u32;
            let has_window_chrome = (style & (WS_CAPTION.0 | WS_THICKFRAME.0)) != 0;
            let exact_monitor = window_rect.left == monitor_info.rcMonitor.left
                && window_rect.top == monitor_info.rcMonitor.top
                && window_rect.right == monitor_info.rcMonitor.right
                && window_rect.bottom == monitor_info.rcMonitor.bottom;
            let is_maximized = (style & WS_MAXIMIZE.0) != 0;
            !has_window_chrome || (exact_monitor && !is_maximized)
        }
    }

    fn cursor_over_rect(rect: RECT) -> (bool, windows::Win32::Foundation::POINT) {
        let mut point = windows::Win32::Foundation::POINT::default();
        unsafe {
            if GetCursorPos(&mut point).is_err() { return (false, point); }
        }
        (
            point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom,
            point,
        )
    }

    fn rect_text(rect: Option<RECT>) -> String {
        match rect {
            Some(r) => format!("({},{} {}x{})", r.left, r.top, r.right - r.left, r.bottom - r.top),
            None => "none".into(),
        }
    }

    fn maintain_overlay_zorder(hwnd: HWND, info: Option<(HWND, RECT)>) {
        let Some((taskbar, rect)) = info else { return; };
        let popup_menu = open_popup_menu();
        unsafe {
            // Make the taskbar the popup owner. This keeps the overlay in the
            // taskbar's shell-owned relationship instead of relying on a bare
            // HWND_TOPMOST request, which Explorer can disturb after Start/
            // taskbar activation transitions.
            // The owner only needs to change when Explorer gives us a new
            // taskbar HWND. Rewriting it on every 33 ms tick can itself cause
            // needless shell window-manager churn while the app is clicked.
            if GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT) != taskbar.0 as isize {
                SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, taskbar.0 as isize);
            }

            // If a native popup menu is open, insert the overlay directly
            // below that menu instead of above every topmost window. Once the
            // menu closes, the next tick restores the normal topmost placement.
            let insert_after = popup_menu.or(Some(HWND_TOPMOST));
            let visibility = if popup_menu.is_some() {
                SET_WINDOW_POS_FLAGS(0)
            } else {
                SWP_SHOWWINDOW
            };
            if let Err(error) = SetWindowPos(
                hwnd,
                insert_after,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                visibility | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            ) {
                emit_runtime("warn", format!("[taskbar-dimmer] z-order repair failed: {error}"));
            }
            // Do not call BringWindowToTop or repeatedly ShowWindow here.
            // BringWindowToTop can activate/focus the overlay despite the
            // no-activate styles, making the main app visibly flicker when it
            // is clicked. SetWindowPos with HWND_TOPMOST + SWP_NOACTIVATE is
            // sufficient to restore z-order without an activation transition.
        }
    }

    fn update_fade(hwnd: HWND) {
        let Some(state) = FADE_STATE.get() else { return; };
        let taskbar = taskbar_info();
        let taskbar_rect = taskbar.map(|(_, rect)| rect);
        let (hovered, cursor) = taskbar_rect.map(cursor_over_rect).unwrap_or((false, windows::Win32::Foundation::POINT::default()));
        let now = Instant::now();
        let fullscreen = fullscreen_app_active(hwnd);
        let mut alpha_to_apply = None;

        let mut diagnostic = None;
        {
            let mut fade = state.lock().unwrap();
            fade.target_alpha = if fade.enabled && !hovered && !fullscreen {
                fade.base_alpha
            } else {
                0.0
            };
            if fade.last_hovered != Some(hovered) || (VERBOSE_LOGGING.load(Ordering::Relaxed) && now.duration_since(fade.last_diagnostic) >= Duration::from_secs(1)) {
                fade.last_hovered = Some(hovered);
                fade.last_diagnostic = now;
                diagnostic = Some((fade.current_alpha, fade.target_alpha, fade.enabled));
            }
            let elapsed = now.duration_since(fade.last_tick);
            fade.last_tick = now;
            let distance = fade.target_alpha - fade.current_alpha;
            if distance.abs() > 0.01 {
                let progress = if fade.ease_duration_ms == 0 {
                    1.0
                } else {
                    let duration = Duration::from_millis(fade.ease_duration_ms.clamp(MIN_EASE_DURATION_MS, MAX_EASE_DURATION_MS));
                    (elapsed.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
                };
                // Fast ease-out: move quickly at the beginning, then settle
                // smoothly over the final few alpha levels. This only updates
                // the native layered window and never touches the frontend.
                let eased = 1.0 - (1.0 - progress).powi(3);
                fade.current_alpha += distance * eased;
                if distance.abs() < 0.5 || progress >= 1.0 {
                    fade.current_alpha = fade.target_alpha;
                }
                alpha_to_apply = Some(fade.current_alpha.round().clamp(0.0, 255.0) as u8);
            }
        }

        if let Some(alpha) = alpha_to_apply {
            unsafe {
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
            }
        }
        // Do the z-order repair after changing alpha as well as on steady
        // ticks, so the shell cannot leave the newly visible overlay behind
        // the taskbar during the same transition.
        maintain_overlay_zorder(hwnd, taskbar);

        if let Some((current, target, enabled)) = diagnostic {
            let mut overlay_rect = RECT::default();
            let overlay_rect = unsafe {
                if GetWindowRect(hwnd, &mut overlay_rect).is_ok() { Some(overlay_rect) } else { None }
            };
            let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
            let foreground = unsafe { GetForegroundWindow() };
            emit_runtime("info", format!(
                "[taskbar-dimmer] tick enabled={} hovered={} cursor=({}, {}) taskbar={} visible={} overlay={} alpha={:.0}->{:.0} foreground=0x{:x}",
                enabled,
                hovered,
                cursor.x,
                cursor.y,
                rect_text(taskbar_rect),
                visible,
                rect_text(overlay_rect),
                current,
                target,
                foreground.0 as isize,
            ));
        }
    }

    pub fn create(app: &AppHandle) -> Result<isize, String> {
        let _ = APP_HANDLE.set(app.clone());
        register_class();
        let name = class_name();
        let title = wide("OLED Helper Taskbar Dimmer");
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                Some(std::ptr::null::<c_void>()),
            )
        }
        .map_err(|e| format!("could not create native taskbar overlay: {e}"))?;

        let _ = FADE_STATE.set(Mutex::new(FadeState {
            enabled: false,
            base_alpha: 0.0,
            ease_duration_ms: DEFAULT_EASE_DURATION_MS,
            current_alpha: 0.0,
            target_alpha: 0.0,
            last_tick: Instant::now(),
            timer_started: false,
            last_hovered: None,
            last_diagnostic: Instant::now(),
        }));

        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        Ok(hwnd.0 as isize)
    }

    pub fn apply(raw: isize, enabled: bool, opacity: f64, ease_duration_ms: u64, immediate: bool) -> Result<(), String> {
        let taskbar = taskbar_info().ok_or_else(|| "Windows taskbar was not found".to_string())?;
        let rect = taskbar.1;
        let hwnd = HWND(raw as *mut c_void);
        let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round();
        let hovered = enabled && cursor_over_rect(rect).0;

        let mut immediate_alpha = None;
        if let Some(state) = FADE_STATE.get() {
            let mut fade = state.lock().unwrap();
            fade.enabled = enabled;
            fade.base_alpha = alpha;
            fade.ease_duration_ms = ease_duration_ms.clamp(MIN_EASE_DURATION_MS, MAX_EASE_DURATION_MS);
            fade.target_alpha = if enabled && !hovered { alpha } else { 0.0 };
            if immediate {
                fade.current_alpha = fade.target_alpha;
                immediate_alpha = Some(fade.current_alpha.round().clamp(0.0, 255.0) as u8);
            }
            fade.last_tick = Instant::now();
            if !enabled {
                fade.current_alpha = 0.0;
            }
            if enabled && !fade.timer_started {
                let timer = unsafe { SetTimer(Some(hwnd), TIMER_ID, TIMER_INTERVAL_MS, None) };
                if timer == 0 {
                    return Err("could not start taskbar dimmer fade timer".into());
                }
                fade.timer_started = true;
            } else if !enabled && fade.timer_started {
                unsafe { let _ = KillTimer(Some(hwnd), TIMER_ID); }
                fade.timer_started = false;
            }
        }

        unsafe {
            let old = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let style = old
                | WS_EX_LAYERED.0 as isize
                | WS_EX_TRANSPARENT.0 as isize
                | WS_EX_TOOLWINDOW.0 as isize
                | WS_EX_NOACTIVATE.0 as isize;
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
            if let Some(alpha) = immediate_alpha {
                SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA)
                    .map_err(|e| format!("could not update overlay opacity: {e}"))?;
            } else if !enabled {
                SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA)
                    .map_err(|e| format!("could not hide overlay: {e}"))?;
            }

            let visibility = if enabled { SWP_SHOWWINDOW } else { SWP_HIDEWINDOW };
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                visibility | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            )
            .map_err(|e| format!("could not position taskbar overlay: {e}"))?;
            if enabled {
                let _ = ShowWindow(hwnd, SW_SHOW);
            } else {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
        if enabled {
            update_fade(hwnd);
        } else {
            maintain_overlay_zorder(hwnd, Some(taskbar));
        }
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    pub fn available() -> bool { false }
    pub fn set_verbose_logging(_enabled: bool) {}
    pub fn create(_app: &AppHandle) -> Result<isize, String> {
        Err("Taskbar dimmer is only available on Windows".into())
    }
    pub fn apply(_raw: isize, _enabled: bool, _opacity: f64, _ease_duration_ms: u64, _immediate: bool) -> Result<(), String> {
        Err("Taskbar dimmer is only available on Windows".into())
    }
}

#[tauri::command]
pub fn set_taskbar_dimmer(
    state: State<TaskbarDimmerState>,
    enabled: bool,
    opacity: f64,
    ease_duration_ms: u64,
    immediate: bool,
) -> Result<(), String> {
    let raw = state.overlay.lock().unwrap().ok_or_else(|| {
        "taskbar dimmer native overlay is not initialized".to_string()
    })?;
    let ease_duration_ms = ease_duration_ms.clamp(0, 1000);
    platform::apply(raw, enabled, opacity, ease_duration_ms, immediate)?;
    *state.enabled.lock().unwrap() = enabled;
    *state.opacity.lock().unwrap() = opacity.clamp(0.0, 1.0);
    *state.ease_duration_ms.lock().unwrap() = ease_duration_ms;
    Ok(())
}

#[tauri::command]
pub fn set_taskbar_dimmer_verbose_logging(enabled: bool) {
    platform::set_verbose_logging(enabled);
}

#[tauri::command]
pub fn get_taskbar_dimmer_status(state: State<TaskbarDimmerState>) -> TaskbarDimmerStatus {
    TaskbarDimmerStatus {
        enabled: *state.enabled.lock().unwrap(),
        opacity: *state.opacity.lock().unwrap(),
        ease_duration_ms: *state.ease_duration_ms.lock().unwrap(),
        available: platform::available() && state.overlay.lock().unwrap().is_some(),
    }
}

pub fn create_overlay(app: &AppHandle, state: &TaskbarDimmerState) -> Result<(), String> {
    let hwnd = platform::create(app)?;
    *state.overlay.lock().unwrap() = Some(hwnd);
    Ok(())
}
