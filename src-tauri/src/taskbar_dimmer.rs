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
    pub fade_in_duration_ms: Mutex<u64>,
    pub fade_out_duration_ms: Mutex<u64>,
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
            fade_in_duration_ms: Mutex::new(250),
            fade_out_duration_ms: Mutex::new(250),
            overlay: Mutex::new(None),
        }
    }
}

#[derive(Serialize)]
pub struct TaskbarDimmerStatus {
    pub enabled: bool,
    pub opacity: f64,
    pub fade_in_duration_ms: u64,
    pub fade_out_duration_ms: u64,
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

    // One adaptive timer handles both cursor polling and animation. It runs
    // at the fast cadence only while a transition is active.
    const TIMER_ID: usize = 1;
    const IDLE_TIMER_INTERVAL_MS: u32 = 33;
    const ACTIVE_TIMER_INTERVAL_MS: u32 = 16;
    const TASKBAR_CACHE_REFRESH_MS: u64 = 1000;
    const ZORDER_REPAIR_INTERVAL_MS: u64 = 250;
    const DEFAULT_EASE_DURATION_MS: u64 = 250;
    const MIN_EASE_DURATION_MS: u64 = 0;
    const MAX_EASE_DURATION_MS: u64 = 1000;

    struct FadeState {
        enabled: bool,
        base_alpha: f64,
        fade_in_duration_ms: u64,
        fade_out_duration_ms: u64,
        current_alpha: f64,
        target_alpha: f64,
        // Absolute transition state. When the target reverses, the current
        // alpha becomes the new start point so the fade continues smoothly
        // from wherever the previous transition stopped.
        start_alpha: f64,
        fade_start_time: Instant,
        transition_active: bool,
        timer_started: bool,
        timer_interval_ms: u32,
        last_hovered: Option<bool>,
        last_diagnostic: Instant,
        last_poll_tick: Option<Instant>,
        last_poll_gap_diagnostic: Option<Instant>,
        // Raw HWND value keeps this shared state Send + Sync; reconstruct the
        // HWND only at the point where a Win32 call is made.
        cached_taskbar: Option<(isize, RECT)>,
        last_taskbar_cache_refresh: Instant,
        last_zorder_repair: Instant,
        last_applied_alpha: Option<u8>,
        last_timer_failure_diagnostic: Option<Instant>,
        last_fade_tick: Option<Instant>,
        fade_tick_count: u64,
        max_fade_tick_gap_ms: f64,
        max_alpha_jump: f64,
        max_layered_update_us: u128,
        slow_layered_update_count: u64,
    }

    static FADE_STATE: OnceLock<Mutex<FadeState>> = OnceLock::new();
    static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();
    static VERBOSE_LOGGING: AtomicBool = AtomicBool::new(false);
    // Set by the frontend while the app's own window is in monitor fullscreen
    // (the calibration strip). fullscreen_app_active() can't detect that case
    // — it intentionally ignores the app's own window so the dimmer keeps
    // working when the app is merely maximized — so the frontend suspends the
    // overlay explicitly. The dimmer settings are untouched; the overlay just
    // leaves the screen until fullscreen ends.
    static SUSPENDED: AtomicBool = AtomicBool::new(false);

    pub fn set_verbose_logging(enabled: bool) {
        VERBOSE_LOGGING.store(enabled, Ordering::Relaxed);
    }

    pub fn set_suspended(enabled: bool) {
        SUSPENDED.store(enabled, Ordering::Relaxed);
    }

    fn emit_runtime(level: &str, message: String) {
        // Taskbar-dimmer logging is disabled by default: the hover/periodic
        // tick diagnostics can fill the on-disk log while the app idles. The
        // code stays in place — the Debug tab's "Verbose periodic logs" toggle
        // flips VERBOSE_LOGGING to re-enable all of it for debugging.
        if !VERBOSE_LOGGING.load(Ordering::Relaxed) {
            return;
        }
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
                // A single adaptive timer handles hover detection, animation,
                // and occasional z-order maintenance. Keeping these operations
                // in one callback avoids competing WM_TIMER streams on the
                // overlay's window thread.
                if wparam.0 == TIMER_ID {
                    update_fade(hwnd);
                }
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

    fn transition_summary(fade: &FadeState, reason: &str, now: Instant) -> String {
        let elapsed_ms = now.duration_since(fade.fade_start_time).as_secs_f64() * 1000.0;
        format!(
            "[taskbar-dimmer] fade_summary reason={} start={:.1} target={:.1} current={:.1} elapsed_ms={:.1} ticks={} max_tick_gap_ms={:.1} max_alpha_jump={:.1} max_set_layered_us={} slow_set_layered_count={}",
            reason,
            fade.start_alpha,
            fade.target_alpha,
            fade.current_alpha,
            elapsed_ms,
            fade.fade_tick_count,
            fade.max_fade_tick_gap_ms,
            fade.max_alpha_jump,
            fade.max_layered_update_us,
            fade.slow_layered_update_count,
        )
    }

    fn update_fade(hwnd: HWND) {
        let Some(state) = FADE_STATE.get() else { return; };
        let now = Instant::now();

        // Fullscreen suppression remains on the adaptive timer, but does not
        // perform any taskbar/window queries while it is active.
        let fullscreen = fullscreen_app_active(hwnd) || SUSPENDED.load(Ordering::Relaxed);
        if fullscreen {
            unsafe {
                if IsWindowVisible(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
            }
            return;
        }

        let mut diagnostic = None;
        let mut transition_log = None;
        let mut poll_gap_log = None;
        let mut timer_start_failed = false;
        let mut should_repair_zorder = false;
        let mut hovered = false;
        let mut cursor = windows::Win32::Foundation::POINT::default();
        let mut alpha_to_apply = None;
        let mut transition_done = false;

        {
            let mut fade = state.lock().unwrap();
            if let Some(previous) = fade.last_poll_tick {
                let gap_ms = now.duration_since(previous).as_secs_f64() * 1000.0;
                let can_report = fade.last_poll_gap_diagnostic
                    .map(|last| now.duration_since(last) >= Duration::from_secs(1))
                    .unwrap_or(true);
                if gap_ms >= 60.0 && can_report {
                    fade.last_poll_gap_diagnostic = Some(now);
                    poll_gap_log = Some(format!("[taskbar-dimmer] poll_gap_ms={:.1}", gap_ms));
                }
            }
            fade.last_poll_tick = Some(now);

            let refresh_cache = fade.cached_taskbar.is_none()
                || now.duration_since(fade.last_taskbar_cache_refresh).as_millis() as u64 >= TASKBAR_CACHE_REFRESH_MS;
            if refresh_cache {
                fade.last_taskbar_cache_refresh = now;
                let previous = fade.cached_taskbar;
                fade.cached_taskbar = taskbar_info().map(|(taskbar, rect)| (taskbar.0 as isize, rect));
                if fade.cached_taskbar != previous {
                    should_repair_zorder = true;
                }
            }
            let cached = fade.cached_taskbar;
            if let Some((_, rect)) = cached {
                let (is_hovered, point) = cursor_over_rect(rect);
                hovered = is_hovered;
                cursor = point;
            }

            let new_target = if fade.enabled && !hovered { fade.base_alpha } else { 0.0 };
            if (fade.target_alpha - new_target).abs() > 0.01 {
                if fade.fade_tick_count > 0 {
                    transition_log = Some(transition_summary(&fade, "reversed", now));
                }
                fade.start_alpha = fade.current_alpha;
                fade.fade_start_time = now;
                fade.target_alpha = new_target;
                fade.transition_active = true;
                fade.last_fade_tick = None;
                fade.fade_tick_count = 0;
                fade.max_fade_tick_gap_ms = 0.0;
                fade.max_alpha_jump = 0.0;
                fade.max_layered_update_us = 0;
                fade.slow_layered_update_count = 0;
                should_repair_zorder = true;
            }

            if fade.last_hovered != Some(hovered)
                || (VERBOSE_LOGGING.load(Ordering::Relaxed)
                    && now.duration_since(fade.last_diagnostic) >= Duration::from_secs(1))
            {
                fade.last_hovered = Some(hovered);
                fade.last_diagnostic = now;
                diagnostic = Some((fade.current_alpha, fade.target_alpha, fade.enabled));
            }

            if fade.enabled && fade.transition_active {
                if let Some(previous) = fade.last_fade_tick {
                    fade.max_fade_tick_gap_ms = fade.max_fade_tick_gap_ms.max(
                        now.duration_since(previous).as_secs_f64() * 1000.0,
                    );
                }
                fade.last_fade_tick = Some(now);
                fade.fade_tick_count = fade.fade_tick_count.saturating_add(1);

                let before = fade.current_alpha;
                let distance = fade.target_alpha - fade.start_alpha;
                if distance.abs() > 0.01 {
                    let duration_ms = if distance > 0.0 {
                        fade.fade_in_duration_ms
                    } else {
                        fade.fade_out_duration_ms
                    };
                    let progress = if duration_ms == 0 {
                        1.0
                    } else {
                        let duration = Duration::from_millis(duration_ms.clamp(MIN_EASE_DURATION_MS, MAX_EASE_DURATION_MS));
                        (now.duration_since(fade.fade_start_time).as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
                    };
                    let eased = 1.0 - (1.0 - progress).powi(3);
                    fade.current_alpha = fade.start_alpha + distance * eased;
                    if progress >= 1.0 {
                        fade.current_alpha = fade.target_alpha;
                    }
                    fade.max_alpha_jump = fade.max_alpha_jump.max((fade.current_alpha - before).abs());
                    alpha_to_apply = Some(fade.current_alpha.round().clamp(0.0, 255.0) as u8);
                } else {
                    fade.current_alpha = fade.target_alpha;
                }

                // Only mark completion when this tick was actually advancing
                // a transition. Otherwise an idle timer tick would look like
                // a completed fade and reset the diagnostic counters.
                transition_done = distance.abs() > 0.01
                    && (fade.target_alpha - fade.current_alpha).abs() <= 0.01;
                if transition_done {
                    fade.current_alpha = fade.target_alpha;
                    fade.start_alpha = fade.target_alpha;
                    fade.transition_active = false;
                    should_repair_zorder = true;
                }
            }

            let active = fade.enabled && fade.transition_active;
            let desired_interval = if active { ACTIVE_TIMER_INTERVAL_MS } else { IDLE_TIMER_INTERVAL_MS };
            if !fade.timer_started {
                let timer = unsafe { SetTimer(Some(hwnd), TIMER_ID, desired_interval, None) };
                if timer != 0 {
                    fade.timer_started = true;
                    fade.timer_interval_ms = desired_interval;
                } else {
                    let can_report = fade.last_timer_failure_diagnostic
                        .map(|last| now.duration_since(last) >= Duration::from_secs(1))
                        .unwrap_or(true);
                    if can_report {
                        fade.last_timer_failure_diagnostic = Some(now);
                        timer_start_failed = true;
                    }
                }
            } else if fade.timer_interval_ms != desired_interval {
                let timer = unsafe { SetTimer(Some(hwnd), TIMER_ID, desired_interval, None) };
                if timer != 0 {
                    fade.timer_interval_ms = desired_interval;
                } else {
                    let can_report = fade.last_timer_failure_diagnostic
                        .map(|last| now.duration_since(last) >= Duration::from_secs(1))
                        .unwrap_or(true);
                    if can_report {
                        fade.last_timer_failure_diagnostic = Some(now);
                        timer_start_failed = true;
                    }
                }
            }

            if now.duration_since(fade.last_zorder_repair).as_millis() as u64 >= ZORDER_REPAIR_INTERVAL_MS {
                should_repair_zorder = true;
            }
            if should_repair_zorder {
                fade.last_zorder_repair = now;
            }
        }
        let cached_taskbar_for_log = if let Some(state) = FADE_STATE.get() {
            state.lock().unwrap().cached_taskbar
        } else {
            None
        };

        if let Some(alpha) = alpha_to_apply {
            let should_apply = FADE_STATE
                .get()
                .map(|state| state.lock().unwrap().last_applied_alpha != Some(alpha))
                .unwrap_or(false);
            if should_apply {
                let started = Instant::now();
                let result = unsafe {
                    SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA)
                };
                let duration = started.elapsed().as_micros();
                if let Some(state) = FADE_STATE.get() {
                    let mut fade = state.lock().unwrap();
                    fade.max_layered_update_us = fade.max_layered_update_us.max(duration);
                    if duration >= 2000 {
                        fade.slow_layered_update_count = fade.slow_layered_update_count.saturating_add(1);
                    }
                    if result.is_ok() {
                        // Only suppress future writes after Windows accepted
                        // this one; failures must remain retryable.
                        fade.last_applied_alpha = Some(alpha);
                    }
                }
                if let Err(error) = result {
                    emit_runtime("warn", format!(
                        "[taskbar-dimmer] layered_update_failed alpha={} error={}",
                        alpha, error,
                    ));
                }
            }
        }

        if should_repair_zorder {
            let zorder_started = Instant::now();
            maintain_overlay_zorder(
                hwnd,
                cached_taskbar_for_log.map(|(raw, rect)| (HWND(raw as *mut c_void), rect)),
            );
            let zorder_us = zorder_started.elapsed().as_micros();
            if zorder_us >= 2000 {
                emit_runtime("warn", format!("[taskbar-dimmer] slow_zorder_update duration_us={}", zorder_us));
            }
        }
        if let Some(message) = transition_log {
            emit_runtime("info", message);
        }
        if let Some(message) = poll_gap_log {
            emit_runtime("warn", message);
        }
        if timer_start_failed {
            emit_runtime("error", "[taskbar-dimmer] timer_start_or_update_failed".to_string());
        }

        if transition_done {
            if let Some(state) = FADE_STATE.get() {
                let mut fade = state.lock().unwrap();
                if fade.fade_tick_count > 0 {
                    let summary = transition_summary(&fade, "complete", now);
                    fade.fade_tick_count = 0;
                    fade.last_fade_tick = None;
                    fade.max_fade_tick_gap_ms = 0.0;
                    fade.max_alpha_jump = 0.0;
                    fade.max_layered_update_us = 0;
                    fade.slow_layered_update_count = 0;
                    drop(fade);
                    emit_runtime("info", summary);
                }
            }
        }

        if let Some((current, target, enabled)) = diagnostic {
            let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
            let overlay_rect = unsafe {
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() { Some(rect) } else { None }
            };
            emit_runtime("info", format!(
                "[taskbar-dimmer] tick enabled={} hovered={} cursor=({}, {}) taskbar={} visible={} overlay={} alpha={:.0}->{:.0}",
                enabled, hovered, cursor.x, cursor.y, rect_text(cached_taskbar_for_log.map(|(_, rect)| rect)),
                visible, rect_text(overlay_rect), current, target,
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
            fade_in_duration_ms: DEFAULT_EASE_DURATION_MS,
            fade_out_duration_ms: DEFAULT_EASE_DURATION_MS,
            current_alpha: 0.0,
            target_alpha: 0.0,
            start_alpha: 0.0,
            fade_start_time: Instant::now(),
            transition_active: false,
            timer_started: false,
            timer_interval_ms: IDLE_TIMER_INTERVAL_MS,
            last_hovered: None,
            last_diagnostic: Instant::now(),
            last_poll_tick: None,
            last_poll_gap_diagnostic: None,
            last_timer_failure_diagnostic: None,
            last_fade_tick: None,
            fade_tick_count: 0,
            max_fade_tick_gap_ms: 0.0,
            max_alpha_jump: 0.0,
            max_layered_update_us: 0,
            slow_layered_update_count: 0,
            cached_taskbar: None,
            last_taskbar_cache_refresh: Instant::now(),
            last_zorder_repair: Instant::now(),
            last_applied_alpha: None,
        }));

        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        Ok(hwnd.0 as isize)
    }

    pub fn apply(raw: isize, enabled: bool, opacity: f64, fade_in_duration_ms: u64, fade_out_duration_ms: u64, immediate: bool) -> Result<(), String> {
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
            fade.fade_in_duration_ms = fade_in_duration_ms.clamp(MIN_EASE_DURATION_MS, MAX_EASE_DURATION_MS);
            fade.fade_out_duration_ms = fade_out_duration_ms.clamp(MIN_EASE_DURATION_MS, MAX_EASE_DURATION_MS);
            let new_target = if enabled && !hovered { alpha } else { 0.0 };
            let now = Instant::now();
            if (fade.target_alpha - new_target).abs() > 0.01 {
                fade.start_alpha = fade.current_alpha;
                fade.fade_start_time = now;
                fade.target_alpha = new_target;
                fade.transition_active = true;
            }
            if immediate {
                fade.current_alpha = fade.target_alpha;
                fade.start_alpha = fade.current_alpha;
                fade.fade_start_time = now;
                fade.transition_active = false;
                let immediate_value = fade.current_alpha.round().clamp(0.0, 255.0) as u8;
                fade.last_applied_alpha = Some(immediate_value);
                immediate_alpha = Some(immediate_value);
            }
            if !enabled {
                fade.current_alpha = 0.0;
                fade.start_alpha = 0.0;
                fade.target_alpha = 0.0;
                fade.transition_active = false;
                fade.last_applied_alpha = Some(0);
            }
            if enabled && !fade.timer_started {
                let timer = unsafe { SetTimer(Some(hwnd), TIMER_ID, IDLE_TIMER_INTERVAL_MS, None) };
                if timer == 0 {
                    return Err("could not start taskbar dimmer timer".into());
                }
                fade.timer_started = true;
                fade.timer_interval_ms = IDLE_TIMER_INTERVAL_MS;
                fade.cached_taskbar = Some((taskbar.0.0 as isize, rect));
                fade.last_taskbar_cache_refresh = Instant::now();
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
    pub fn set_suspended(_enabled: bool) {}
    pub fn create(_app: &AppHandle) -> Result<isize, String> {
        Err("Taskbar dimmer is only available on Windows".into())
    }
    pub fn apply(_raw: isize, _enabled: bool, _opacity: f64, _fade_in_duration_ms: u64, _fade_out_duration_ms: u64, _immediate: bool) -> Result<(), String> {
        Err("Taskbar dimmer is only available on Windows".into())
    }
}

#[tauri::command]
pub fn set_taskbar_dimmer(
    state: State<TaskbarDimmerState>,
    enabled: bool,
    opacity: f64,
    fade_in_duration_ms: u64,
    fade_out_duration_ms: u64,
    immediate: bool,
) -> Result<(), String> {
    let raw = state.overlay.lock().unwrap().ok_or_else(|| {
        "taskbar dimmer native overlay is not initialized".to_string()
    })?;
    let fade_in_duration_ms = fade_in_duration_ms.clamp(0, 1000);
    let fade_out_duration_ms = fade_out_duration_ms.clamp(0, 1000);
    platform::apply(raw, enabled, opacity, fade_in_duration_ms, fade_out_duration_ms, immediate)?;
    *state.enabled.lock().unwrap() = enabled;
    *state.opacity.lock().unwrap() = opacity.clamp(0.0, 1.0);
    *state.fade_in_duration_ms.lock().unwrap() = fade_in_duration_ms;
    *state.fade_out_duration_ms.lock().unwrap() = fade_out_duration_ms;
    Ok(())
}

#[tauri::command]
pub fn set_taskbar_dimmer_verbose_logging(enabled: bool) {
    platform::set_verbose_logging(enabled);
}

#[tauri::command]
pub fn set_taskbar_dimmer_suspended(suspended: bool) {
    platform::set_suspended(suspended);
}

#[tauri::command]
pub fn get_taskbar_dimmer_status(state: State<TaskbarDimmerState>) -> TaskbarDimmerStatus {
    TaskbarDimmerStatus {
        enabled: *state.enabled.lock().unwrap(),
        opacity: *state.opacity.lock().unwrap(),
        fade_in_duration_ms: *state.fade_in_duration_ms.lock().unwrap(),
        fade_out_duration_ms: *state.fade_out_duration_ms.lock().unwrap(),
        available: platform::available() && state.overlay.lock().unwrap().is_some(),
    }
}

pub fn create_overlay(app: &AppHandle, state: &TaskbarDimmerState) -> Result<(), String> {
    let hwnd = platform::create(app)?;
    *state.overlay.lock().unwrap() = Some(hwnd);
    Ok(())
}
