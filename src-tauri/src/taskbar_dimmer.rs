use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, State};

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
        // Must match config.rs's default_dimmer_ease_duration() (300 ms) — the
        // frontend pushes its persisted values shortly after launch, but the
        // state defaults govern the first ticks and reset-all.
        Self {
            enabled: Mutex::new(false),
            opacity: Mutex::new(0.95),
            fade_in_duration_ms: Mutex::new(300),
            fade_out_duration_ms: Mutex::new(300),
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
    use std::sync::{
        atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
        Condvar, Mutex, OnceLock,
    };
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};
    use tauri::AppHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, EndPaint, FillRect, GetMonitorInfoW, GetStockObject, MonitorFromWindow,
        BLACK_BRUSH, HBRUSH, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
    };
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, PSID,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW,
        GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowLongPtrW,
        GetWindowRect, IsIconic, IsWindowVisible, PostMessageW, PostThreadMessageW, RegisterClassW,
        SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, SetWindowsHookExW, ShowWindow,
        TranslateMessage, UnhookWindowsHookEx, GWLP_HWNDPARENT, GWL_EXSTYLE, GWL_STYLE,
        HTTRANSPARENT, HWND_TOPMOST, LWA_ALPHA, MSG, SET_WINDOW_POS_FLAGS, SWP_HIDEWINDOW,
        SWP_NOACTIVATE, SWP_NOSENDCHANGING, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW, WH_MOUSE_LL, WM_APP,
        WM_MOUSEFIRST, WM_MOUSEHWHEEL, WM_NCHITTEST, WM_PAINT, WM_QUIT, WNDCLASSW, WS_CAPTION,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_MAXIMIZE,
        WS_POPUP, WS_THICKFRAME,
    };

    // One adaptive timer handles both cursor polling and animation. It runs
    // at the fast cadence only while a transition is active.
    const WM_DIMMER_TICK: u32 = WM_APP;
    const WM_DIMMER_REPAIR_ZORDER: u32 = WM_APP + 1;
    const ACTIVE_TIMER_INTERVAL_MS: u32 = 16;
    const TASKBAR_CACHE_REFRESH_MS: u64 = 1000;
    // Event mode refreshes the taskbar cache on taskbar window events
    // (move, resize, auto-hide slide); this slow catch-all guarantees a
    // stale cache can't persist if an event is ever missed (e.g. Explorer
    // recreating the taskbar with a new HWND mid-transition).
    const TASKBAR_CACHE_CATCHALL_MS: u64 = 5000;
    const ZORDER_REPAIR_INTERVAL_MS: u64 = 250;
    // Fullscreen detection runs the expensive Win32 foreground-window chain
    // (GetForegroundWindow → GetWindowThreadProcessId → GetWindowRect →
    // MonitorFromWindow → GetMonitorInfoW → GetWindowLongPtrW). It is
    // decoupled from the fade animation tick: the result is cached, so the
    // fade timer can stay as fast as possible without paying the detection
    // cost every tick. This cadence is the detection source in poll hover
    // mode; in event hover mode the WinEvent hook replaces the timer
    // entirely, and this timer only takes over if the hook fails to install.
    const FULLSCREEN_CHECK_INTERVAL_MS: u64 = 100;
    const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
    const EVENT_SYSTEM_MOVESIZESTART: u32 = 0x000A;
    const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000B;
    const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;
    const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;
    const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
    const EVENT_OBJECT_STATECHANGE: u32 = 0x800A;
    const DEFAULT_EASE_DURATION_MS: u64 = 300; // must match config.rs default_dimmer_ease_duration()
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
        last_hovered: Option<bool>,
        last_diagnostic: Instant,
        last_poll_tick: Option<Instant>,
        last_poll_gap_diagnostic: Option<Instant>,
        // Cached fullscreen state, refreshed at FULLSCREEN_CHECK_INTERVAL_MS
        // so the expensive detection chain doesn't run on every animation tick.
        fullscreen_active: bool,
        last_fullscreen_check: Instant,
        // Raw HWND value keeps this shared state Send + Sync; reconstruct the
        // HWND only at the point where a Win32 call is made.
        cached_taskbar: Option<(isize, RECT)>,
        last_taskbar_cache_refresh: Instant,
        last_zorder_repair: Instant,
        last_applied_alpha: Option<u8>,
        last_fade_tick: Option<Instant>,
        fade_tick_count: u64,
        max_fade_tick_gap_ms: f64,
        max_alpha_jump: f64,
        max_layered_update_us: u128,
        slow_layered_update_count: u64,
    }

    static FADE_STATE: OnceLock<Mutex<FadeState>> = OnceLock::new();
    static TICK_THREAD: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
    static VERBOSE_LOGGING: AtomicBool = AtomicBool::new(false);
    // Per-detection toggle flags, settable from the Debug tab to isolate
    // which tick-time Win32 call is causing poll-gap stutter.
    static FULLSCREEN_DETECTION: AtomicBool = AtomicBool::new(true);
    static TASKBAR_CACHE_REFRESH: AtomicBool = AtomicBool::new(true);
    static ZORDER_REPAIR: AtomicBool = AtomicBool::new(true);
    // Defaults follow the hover mode: in event mode (the default) the mouse
    // hook is the hover detector and the cursor poll stands down; in poll
    // mode the cursor poll is the detector and the hook is unused. set_hover_mode
    // re-syncs both flags on startup and on mode changes; the Debug tab can
    // override either live for A/B tests.
    static CURSOR_POLLING: AtomicBool = AtomicBool::new(false);
    // Set by the frontend while the app's own window is in monitor fullscreen
    // (the calibration strip). fullscreen_app_active() can't detect that case
    // — it intentionally ignores the app's own window so the dimmer keeps
    // working when the app is merely maximized — so the frontend suspends the
    // overlay explicitly. The dimmer settings are untouched; the overlay just
    // leaves the screen until fullscreen ends.
    static TICK_INTERVAL_MS: AtomicU32 = AtomicU32::new(0);
    static TICK_POSTED: AtomicBool = AtomicBool::new(false);
    static TICK_THREAD_SHUTDOWN: AtomicBool = AtomicBool::new(false);
    // Wake channel for the tick thread. When update_fade switches the tick
    // interval (idle → active on a fade start), it notifies this condvar so
    // the thread abandons the rest of its current sleep and re-reads the new
    // interval immediately. Without this, the thread commits to one full
    // idle-interval sleep after posting a tick, so a fade that starts on that
    // tick wouldn't receive its next animation frame until the whole idle
    // interval elapsed — effectively subtracting the idle timer from the fade
    // duration (a 200ms idle + 200ms fade showed no fade at all).
    static TICK_WAKE_MUTEX: Mutex<()> = Mutex::new(());
    static TICK_WAKE_CONDVAR: Condvar = Condvar::new();
    static OVERLAY_HWND: AtomicIsize = AtomicIsize::new(0);
    static SUSPENDED: AtomicBool = AtomicBool::new(false);
    // WH_MOUSE_LL hover detection — instant fade triggers without polling.
    // A dedicated thread hosts the hook (low-level hooks only fire on the
    // installing thread's message loop) and converts mouse events into a
    // fade wake. This decouples hover response from the tick cadence: the
    // fade starts the moment the cursor crosses the taskbar edge instead of
    // waiting for the next idle tick. The tick thread keeps the slow jobs
    // (fullscreen detection, taskbar cache, z-order repair) and doubles as a
    // GetCursorPos fallback when the hook can't see the cursor (elevated
    // foreground windows don't reach a non-elevated hook) or is disabled
    // from the Debug tab.
    static ELEVATION_WINEVENT_HOOK: AtomicIsize = AtomicIsize::new(0); // HWINEVENTHOOK
    static ELEVATION_WINEVENT_THREAD: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
    static ELEVATION_WINEVENT_THREAD_ID: AtomicU32 = AtomicU32::new(0);
    static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0); // HHOOK handle, 0 = not installed
    static MOUSE_HOOK_THREAD: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
    static MOUSE_HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
    static MOUSE_HOOK_TOGGLE: AtomicBool = AtomicBool::new(true);
    // Taskbar rect the hook hit-tests against, kept in sync with the cached
    // taskbar geometry refreshed by update_fade and apply().
    static HOOK_TASKBAR_RECT: Mutex<Option<RECT>> = Mutex::new(None);
    // Last hover state seen by either the hook (instant) or the poll
    // fallback in update_fade. Written by both, read by update_fade.
    static CURRENT_HOVER: AtomicBool = AtomicBool::new(false);
    // True when the foreground window is at a higher integrity level than
    // this process (e.g. an elevated admin prompt / game). WH_MOUSE_LL
    // still fires, but MSLLHOOKSTRUCT.pt is zeroed — the hook callback
    // can't GetCursorPos accurately either (the hook's thread is not
    // trusted). When this is true, update_fade falls back to the tick
    // thread's slow GetCursorPos poll so the overlay doesn't freeze.
    static FOREGROUND_ELEVATED: AtomicBool = AtomicBool::new(false);
    // When true (default), the hook is the primary hover detector and the
    // tick thread only polls cursor when FOREGROUND_ELEVATED is true.
    // When false, the hook is never installed and the tick thread always
    // polls cursor at the idle cadence — a universal fallback for users
    // whose security software, remote desktop, or other edge cases
    // conflict with WH_MOUSE_LL.
    static PREFER_EVENT_HOVER: AtomicBool = AtomicBool::new(true);
    // Set by the WinEvent hooks (foreground switch, move/size, minimize/
    // restore, location change). In event hover mode this is the ONLY
    // trigger for a fullscreen recheck — the timer stands down entirely,
    // so fullscreen enter/exit reacts on the same tick as the triggering
    // event with zero polling. In poll mode it stays unused; the timer-
    // based check is the only path (and takes over if the WinEvent hook
    // fails to install, so event-mode detection never silently stops).
    static FULLSCREEN_RECHECK_REQUESTED: AtomicBool = AtomicBool::new(false);
    // Set by the WinEvent callback when an event targets the cached taskbar
    // window itself (moved to another edge, resized, auto-hide slide,
    // minimized). In event mode this triggers an immediate taskbar-cache
    // refresh so the hook's hit-test rect and the overlay position react
    // instantly instead of waiting for the periodic timer. Poll mode (no
    // WinEvent hook) keeps the plain timer cadence; the flag is simply
    // never set there.
    static TASKBAR_CACHE_RECHECK_REQUESTED: AtomicBool = AtomicBool::new(false);
    // Diagnostic: timestamp of the last logged WinEvent, used to throttle the
    // winevent log so continuous window drags don't flood the log while still
    // showing the burst pattern of a fullscreen enter/exit transition.
    static LAST_WINEVENT_LOG: Mutex<Option<Instant>> = Mutex::new(None);
    // User-configurable idle polling cadence (ms), used both as the
    // GetCursorPos poll interval in poll mode and as the elevated-fallback
    // poll interval in event mode. Default matches the previous hardcoded
    // IDLE_TIMER_INTERVAL_MS. Bounds enforced in the Tauri command below.
    const DEFAULT_POLL_INTERVAL_MS: u32 = 100; // must match config.rs default_poll_interval() and the frontend store
    static POLL_INTERVAL_MS: AtomicU32 = AtomicU32::new(DEFAULT_POLL_INTERVAL_MS);

    pub fn set_poll_interval_ms(ms: u32) {
        POLL_INTERVAL_MS.store(ms, Ordering::Relaxed);
        // If the tick thread is currently idling (not mid-fade), wake it so
        // the new interval takes effect immediately instead of waiting out
        // whatever cadence was previously in flight.
        let guard = TICK_WAKE_MUTEX.lock().unwrap();
        TICK_INTERVAL_MS.store(ms, Ordering::Relaxed);
        TICK_WAKE_CONDVAR.notify_all();
        drop(guard);
    }

    pub fn set_verbose_logging(enabled: bool) {
        VERBOSE_LOGGING.store(enabled, Ordering::Relaxed);
    }

    pub fn set_detection(name: &str, enabled: bool) {
        let flag = match name {
            "fullscreen" => &FULLSCREEN_DETECTION,
            "taskbar_cache" | "taskbarCache" => &TASKBAR_CACHE_REFRESH,
            "zorder" => &ZORDER_REPAIR,
            "cursor" => &CURSOR_POLLING,
            "mouse_hook" | "mouseHook" => &MOUSE_HOOK_TOGGLE,
            _ => return,
        };
        flag.store(enabled, Ordering::Relaxed);
        // Mouse-hook toggling needs lifecycle management beyond the flag: the
        // hook is uninstalled when disabled and (re)installed when enabled
        // while the dimmer is on, so the Debug tab can A/B test it live.
        if name == "mouse_hook" || name == "mouseHook" {
            if enabled {
                let dimmer_on = FADE_STATE
                    .get()
                    .map(|state| state.lock().unwrap().enabled)
                    .unwrap_or(false);
                if dimmer_on {
                    ensure_mouse_hook();
                }
            } else {
                stop_mouse_hook();
                // No fallback is forced here: the Debug-tab toggles are meant to
                // be independent A/B switches, so disabling the hook leaves
                // cursor polling exactly as the user set it (both may be off —
                // that is an explicit test configuration). Switching the
                // Detection Method in the dimmer tab re-syncs both defaults via
                // set_hover_mode instead.
            }
        }
    }

    pub fn set_hover_mode(mode: &str) {
        let event = mode == "event";
        PREFER_EVENT_HOVER.store(event, Ordering::Relaxed);
        // Keep the per-detection toggles aligned with the mode by default:
        // in event mode the mouse hook is the hover detector and the cursor
        // poll stands down (resuming automatically only while the foreground
        // is elevated); in poll mode the cursor poll is the detector and the
        // hook is unused. The Debug tab can override either live for A/B
        // tests — a later mode switch just re-syncs these defaults.
        CURSOR_POLLING.store(!event, Ordering::Relaxed);
        MOUSE_HOOK_TOGGLE.store(event, Ordering::Relaxed);
        // If the dimmer is on, toggle the hook lifecycles to match. When
        // switching to event mode: install the mouse hook (instant hover)
        // and the WinEvent hook (elevation + event-driven fullscreen). When
        // switching to poll mode: stop both — the tick thread's cursor poll
        // and fullscreen timer handle everything.
        let dimmer_on = FADE_STATE
            .get()
            .map(|state| state.lock().unwrap().enabled)
            .unwrap_or(false);
        if dimmer_on {
            if event {
                ensure_elevation_win_event();
                ensure_mouse_hook();
            } else {
                stop_mouse_hook();
                stop_elevation_win_event();
            }
        }
    }

    // Checks whether the foreground window runs at a higher integrity level
    // than this process by reading the foreground process's token. Returns
    // true when the foreground is elevated (admin prompt, UAC-high game)
    // and the mouse hook can't provide accurate cursor positions.
    fn check_foreground_elevation() -> bool {
        unsafe {
            let foreground = GetForegroundWindow();
            if foreground.0.is_null() {
                return false;
            }
            let mut process_id = 0u32;
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                foreground,
                Some(&mut process_id),
            );
            if process_id == 0 || process_id == std::process::id() {
                return false;
            }
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id);
            let Ok(process) = process else { return false };
            let mut token = windows::Win32::Foundation::HANDLE::default();
            const TOKEN_QUERY: u32 = 0x0008;
            if OpenProcessToken(
                process,
                windows::Win32::Security::TOKEN_ACCESS_MASK(TOKEN_QUERY),
                &mut token,
            )
            .is_err()
            {
                let _ = CloseHandle(process);
                return false;
            }
            let mut buffer = [0u8; 256];
            let mut return_length = 0u32;
            let result = GetTokenInformation(
                token,
                windows::Win32::Security::TOKEN_INFORMATION_CLASS(25),
                Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
                buffer.len() as u32,
                &mut return_length,
            );
            let _ = CloseHandle(token);
            let _ = CloseHandle(process);
            if result.is_err() {
                return false;
            }
            // Parse the TOKEN_INTEGRITY_LEVEL struct from the buffer.
            // The struct starts with a TOKEN_LABEL which has a SID_AND_ATTRIBUTES,
            // and the first field of that is a PSID.
            // SECURITY_MANDATORY_HIGH_RID = 0x3000. Any integrity level at
            // or above High is elevated from our medium-integrity perspective.
            // Verify the buffer is large enough for the response.
            if return_length < 8 || return_length > buffer.len() as u32 {
                return false;
            }
            // Parse the TOKEN_INTEGRITY_LEVEL from the buffer manually.
            // The struct is: TOKEN_LABEL { SID_AND_ATTRIBUTES { PSID, Attributes } }
            // The PSID is at offset 0 (8 bytes on 64-bit).
            let sid_ptr = std::ptr::read::<*mut std::ffi::c_void>(buffer.as_ptr() as *const _);
            if sid_ptr.is_null() {
                return false;
            }
            let sid_authority = sid_ptr;
            let sub_auth = GetSidSubAuthorityCount(PSID(sid_authority));
            if sub_auth.is_null() || *sub_auth == 0 {
                return false;
            }
            let count = (*sub_auth as u32) - 1;
            let value = *GetSidSubAuthority(PSID(sid_authority), count);
            const SECURITY_MANDATORY_HIGH_RID: u32 = 0x0000_3000;
            value >= SECURITY_MANDATORY_HIGH_RID
        }
    }

    // WinEvent callback for foreground switches and window move/size/
    // minimize/restore changes. Foreground switches re-check elevation
    // (as before) and always request a fullscreen recheck, since a new
    // foreground window means fullscreen state can only be evaluated fresh.
    // The other events (move/size end, minimize start/end) don't change
    // elevation but can flip fullscreen state without a foreground change
    // (e.g. a borderless window resizing to fill the screen, or restoring
    // from minimized back into the same fullscreen geometry) — those just
    // set the recheck flag without touching FOREGROUND_ELEVATED.
    fn winevent_name(event: u32) -> &'static str {
        match event {
            EVENT_SYSTEM_FOREGROUND => "foreground",
            EVENT_SYSTEM_MOVESIZESTART => "movesize_start",
            EVENT_SYSTEM_MOVESIZEEND => "movesize_end",
            EVENT_SYSTEM_MINIMIZESTART => "minimize_start",
            EVENT_SYSTEM_MINIMIZEEND => "minimize_end",
            EVENT_OBJECT_STATECHANGE => "state_change",
            EVENT_OBJECT_LOCATIONCHANGE => "location_change",
            _ => "other",
        }
    }

    unsafe extern "system" fn elevation_win_event_proc(
        _h_hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        id_object: i32,
        _id_child: i32,
        _event_thread: u32,
        _dwms_event_time: u32,
    ) {
        // EVENT_OBJECT_LOCATIONCHANGE / EVENT_OBJECT_STATECHANGE fire very
        // frequently (any object, any window) when subscribed without
        // idObject filtering — restrict to top-level window events (idObject
        // == OBJID_WINDOW, which SetWinEventHook reports as 0) so ordinary
        // child-control state flips (checkboxes, buttons) don't flood the
        // tick thread with reposts. STATE_CHANGE covers the one transition
        // the other subscriptions can't see: a style-only fullscreen flip
        // (maximize state change, chrome removal) with identical geometry,
        // which fires neither a foreground switch nor a location change.
        const OBJID_WINDOW: i32 = 0;
        if (event == EVENT_OBJECT_LOCATIONCHANGE || event == EVENT_OBJECT_STATECHANGE)
            && id_object != OBJID_WINDOW
        {
            return;
        }
        let mut elevated = false;
        if event == EVENT_SYSTEM_FOREGROUND {
            elevated = check_foreground_elevation();
            FOREGROUND_ELEVATED.store(elevated, Ordering::Relaxed);
        }
        if VERBOSE_LOGGING.load(Ordering::Relaxed) {
            // Log each top-level window event that actually sets the recheck
            // flag, throttled by gap so a continuous drag can't flood the log
            // while a fullscreen transition's event burst stays visible.
            let now = Instant::now();
            let gap_ms = {
                let mut last = LAST_WINEVENT_LOG.lock().unwrap();
                let gap = match *last {
                    Some(prev) => now.duration_since(prev).as_secs_f64() * 1000.0,
                    None => 0.0,
                };
                *last = Some(now);
                gap
            };
            if gap_ms >= 25.0 {
                emit_runtime("info", format!(
                    "[taskbar-dimmer] winevent event={} idObject={} hwnd=0x{:x} elevated={} gap_ms={:.1}",
                    winevent_name(event), id_object, hwnd.0 as usize, elevated, gap_ms,
                ));
            }
        }
        // Only the foreground window (or a foreground-switch event) can flip
        // fullscreen state — a location/state change on a background window
        // cannot. Filtering here keeps system-wide window churn (other apps
        // animating, tray balloons, background resizes) from triggering
        // main-thread fullscreen rechecks on every event.
        let foreground = unsafe { GetForegroundWindow() };
        let affects_fullscreen =
            event == EVENT_SYSTEM_FOREGROUND || (!foreground.0.is_null() && foreground == hwnd);
        if affects_fullscreen {
            FULLSCREEN_RECHECK_REQUESTED.store(true, Ordering::Relaxed);
        }
        // Event-driven taskbar cache: when an event targets the cached
        // taskbar window itself (drag to another edge, resize, auto-hide
        // slide, minimize), request an immediate cache refresh so the hook
        // hit-test rect and the overlay position react instantly. The hwnd
        // comparison is cheap; a taskbar recreated with a new HWND (Explorer
        // restart) is left to the slow catch-all timer.
        let targets_taskbar = FADE_STATE
            .get()
            .and_then(|state| state.lock().ok())
            .and_then(|fade| fade.cached_taskbar)
            .map(|(raw, _)| hwnd.0 as isize == raw)
            .unwrap_or(false);
        if targets_taskbar {
            TASKBAR_CACHE_RECHECK_REQUESTED.store(true, Ordering::Relaxed);
        }
        // Poke the overlay to re-evaluate hover/fullscreen. The tick thread
        // will pick up the right source (hook or poll, and the forced
        // fullscreen recheck) on the next tick.
        post_dimmer_tick();
    }

    fn elevation_win_event_thread_loop() {
        ELEVATION_WINEVENT_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
        let foreground_hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(elevation_win_event_proc),
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::WINEVENT_OUTOFCONTEXT,
            )
        };
        if foreground_hook.0.is_null() {
            emit_runtime(
                "warn",
                "[taskbar-dimmer] elevation WinEvent hook install failed".into(),
            );
            ELEVATION_WINEVENT_HOOK.store(0, Ordering::Relaxed);
            ELEVATION_WINEVENT_THREAD_ID.store(0, Ordering::Relaxed);
            return;
        }
        ELEVATION_WINEVENT_HOOK.store(foreground_hook.0 as isize, Ordering::Release);
        // Seed the elevation flag and request an initial fullscreen recheck
        // so event mode starts from a correct state instead of waiting for
        // the first foreground event (e.g. the dimmer was enabled while an
        // elevated or fullscreen window was already active).
        FOREGROUND_ELEVATED.store(check_foreground_elevation(), Ordering::Relaxed);
        FULLSCREEN_RECHECK_REQUESTED.store(true, Ordering::Relaxed);
        post_dimmer_tick();
        // Second hook on the same thread, covering MOVESIZESTART..MINIMIZEEND
        // (0x000A..0x0017, spanning both the move/size and minimize/restore
        // pairs plus a few harmless codes between them — SetWinEventHook
        // takes a min/max range, not a set). This catches interactive drag-
        // resize and minimize/restore, but NOT title-bar double-click
        // maximize or programmatic resizing — neither of those raise
        // MOVESIZESTART/END. The third hook below (LOCATIONCHANGE) is what
        // actually catches those, since it fires on any bounds change
        // regardless of cause; this range is kept anyway because it
        // distinguishes drag-start from drag-end, which LOCATIONCHANGE alone
        // does not. A different numeric range (0x8000+) so it can't be
        // merged into the first hook's range. Multiple SetWinEventHook calls
        // per thread are supported — all deliver to the same message loop
        // below.
        let movesize_hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_MOVESIZESTART,
                EVENT_SYSTEM_MINIMIZEEND,
                None,
                Some(elevation_win_event_proc),
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::WINEVENT_OUTOFCONTEXT,
            )
        };
        let location_hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_LOCATIONCHANGE,
                EVENT_OBJECT_LOCATIONCHANGE,
                None,
                Some(elevation_win_event_proc),
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::WINEVENT_OUTOFCONTEXT,
            )
        };
        let statechange_hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_STATECHANGE,
                EVENT_OBJECT_STATECHANGE,
                None,
                Some(elevation_win_event_proc),
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::WINEVENT_OUTOFCONTEXT,
            )
        };
        if movesize_hook.0.is_null() {
            emit_runtime("warn", "[taskbar-dimmer] move/size WinEvent hook install failed — fullscreen detection stays timer-based for resize/minimize".into());
        }
        if location_hook.0.is_null() {
            emit_runtime("warn", "[taskbar-dimmer] location-change WinEvent hook install failed — fullscreen detection stays timer-based for in-place resize".into());
        }
        if statechange_hook.0.is_null() {
            emit_runtime("warn", "[taskbar-dimmer] state-change WinEvent hook install failed — style-only fullscreen flips stay undetected in event mode".into());
        }
        let mut msg = MSG::default();
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
        }
        unsafe {
            let _ = UnhookWinEvent(foreground_hook);
            if !movesize_hook.0.is_null() {
                let _ = UnhookWinEvent(movesize_hook);
            }
            if !location_hook.0.is_null() {
                let _ = UnhookWinEvent(location_hook);
            }
            if !statechange_hook.0.is_null() {
                let _ = UnhookWinEvent(statechange_hook);
            }
        }
        ELEVATION_WINEVENT_HOOK.store(0, Ordering::Relaxed);
        ELEVATION_WINEVENT_THREAD_ID.store(0, Ordering::Relaxed);
    }

    fn ensure_elevation_win_event() {
        let threads = ELEVATION_WINEVENT_THREAD.get_or_init(|| Mutex::new(None));
        let mut guard = threads.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let handle = std::thread::Builder::new()
            .name("taskbar-dimmer-elevation".into())
            .spawn(elevation_win_event_thread_loop)
            .expect("taskbar dimmer elevation WinEvent thread");
        *guard = Some(handle);
    }

    fn stop_elevation_win_event() {
        let thread_exists = ELEVATION_WINEVENT_THREAD
            .get()
            .map(|threads| threads.lock().unwrap().is_some())
            .unwrap_or(false);
        if !thread_exists {
            ELEVATION_WINEVENT_HOOK.store(0, Ordering::Relaxed);
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut id = ELEVATION_WINEVENT_THREAD_ID.load(Ordering::Relaxed);
        while id == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
            id = ELEVATION_WINEVENT_THREAD_ID.load(Ordering::Relaxed);
        }
        if id != 0 {
            let _ = unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
        if let Some(threads) = ELEVATION_WINEVENT_THREAD.get() {
            if let Ok(mut guard) = threads.lock() {
                if let Some(handle) = guard.take() {
                    let _ = handle.join();
                }
            }
        }
        ELEVATION_WINEVENT_HOOK.store(0, Ordering::Relaxed);
    }

    pub fn set_suspended(enabled: bool) {
        SUSPENDED.store(enabled, Ordering::Relaxed);
    }

    fn emit_runtime(level: &str, message: String) {
        // The standard logger is the single fan-out point: the terminal,
        // rotating file, Debug tab, and DevTools all receive this exact record.
        // Only the periodic info diagnostics are opt-in (to avoid idle log
        // floods); warn/error records are real failures and always pass
        // through, otherwise the exact clues needed to diagnose a broken
        // hook or slow overlay update would be invisible by default.
        let verbose_only = level != "warn" && level != "error";
        if verbose_only && !VERBOSE_LOGGING.load(Ordering::Relaxed) {
            return;
        }
        match level {
            "warn" => log::warn!("{message}"),
            "error" => log::error!("{message}"),
            _ => log::info!("{message}"),
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
            WM_DIMMER_TICK => {
                // Tick messages are posted by a background thread so the
                // overlay never owns a WM_TIMER. This avoids SetTimer's
                // message-pump interference during window drags — background
                // PostMessageW is a regular queued message, not a synthesized
                // timer that can starve the drag modal loop.
                TICK_POSTED.store(false, Ordering::Release);
                update_fade(hwnd);
                LRESULT(0)
            }
            WM_DIMMER_REPAIR_ZORDER => {
                if !ZORDER_REPAIR.load(Ordering::Relaxed) {
                    return LRESULT(0);
                }
                // Z-order repair is deferred out of the animation tick to
                // prevent SetWindowPos latency from starving the fade timer.
                // The animation tick signals this request via PostMessageW
                // and returns immediately, keeping the WM_TIMER callback fast.
                if let Some(state) = FADE_STATE.get() {
                    let fade = state.lock().unwrap();
                    // Skip the repair while the overlay is hidden for
                    // fullscreen (or the app's own fullscreen strip):
                    // maintain_overlay_zorder's SetWindowPos uses
                    // SWP_SHOWWINDOW, which would re-show a hidden overlay
                    // and flash it back on screen. A repair that was already
                    // queued when fullscreen was detected must not execute.
                    // The restore after exit is covered by the self-heal in
                    // update_fade, which posts a fresh repair only after
                    // fullscreen_active is false again.
                    if fade.fullscreen_active || SUSPENDED.load(Ordering::Relaxed) {
                        return LRESULT(0);
                    }
                    if let Some((raw, rect)) = fade.cached_taskbar {
                        let zorder_started = Instant::now();
                        drop(fade);
                        maintain_overlay_zorder(hwnd, Some((HWND(raw as *mut c_void), rect)));
                        let zorder_us = zorder_started.elapsed().as_micros();
                        // A healthy repair routinely costs 2-5 ms: SetWindowPos
                        // to HWND_TOPMOST triggers DWM z-order reflow plus shell
                        // notifications, and the popup-menu FindWindowW scan and
                        // owner check add to it. The old 2 ms floor fired on
                        // every normal repair, spamming warn. Only >=10 ms
                        // indicates a real stall (hung Explorer/DWM, pathological
                        // shell state) worth surfacing.
                        if zorder_us >= 10_000 {
                            emit_runtime(
                                "warn",
                                format!(
                                    "[taskbar-dimmer] slow_zorder_update duration_us={}",
                                    zorder_us
                                ),
                            );
                        }
                    }
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

    // Returns the window class name of the given window, if any.
    fn window_class_name(hwnd: HWND) -> Option<String> {
        unsafe {
            let mut buf = [0u16; 128];
            let len = GetClassNameW(hwnd, &mut buf);
            if len == 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        }
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
            if process_id == std::process::id() || process_id == 0 || IsIconic(foreground).as_bool()
            {
                return false;
            }

            // The desktop itself ("Progman" / "WorkerW") is a top-level window
            // that covers the entire monitor with no window chrome, so it would
            // be misclassified as a fullscreen app whenever the user clicks the
            // desktop — hiding the overlay until the foreground changes back to
            // a real window. Exclude it explicitly.
            if let Some(class) = window_class_name(foreground) {
                if class == "Progman" || class == "WorkerW" {
                    return false;
                }
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
            if GetCursorPos(&mut point).is_err() {
                return (false, point);
            }
        }
        (
            point.x >= rect.left
                && point.x < rect.right
                && point.y >= rect.top
                && point.y < rect.bottom,
            point,
        )
    }

    fn rect_text(rect: Option<RECT>) -> String {
        match rect {
            Some(r) => format!(
                "({},{} {}x{})",
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top
            ),
            None => "none".into(),
        }
    }

    fn maintain_overlay_zorder(hwnd: HWND, info: Option<(HWND, RECT)>) {
        let Some((taskbar, rect)) = info else {
            return;
        };
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
            if popup_menu.is_none() && VERBOSE_LOGGING.load(Ordering::Relaxed) {
                // The fullscreen path hides the overlay with SW_HIDE; nothing
                // else re-shows it except this SetWindowPos(SWP_SHOWWINDOW),
                // so a hidden→visible transition here marks the restore.
                if !IsWindowVisible(hwnd).as_bool() {
                    emit_runtime(
                        "info",
                        "[taskbar-dimmer] overlay_reshown via zorder repair".into(),
                    );
                }
            }
            if let Err(error) = SetWindowPos(
                hwnd,
                insert_after,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                visibility | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            ) {
                emit_runtime(
                    "warn",
                    format!("[taskbar-dimmer] z-order repair failed: {error}"),
                );
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

    fn ensure_tick_thread() {
        let threads = TICK_THREAD.get_or_init(|| Mutex::new(None));
        let mut guard = threads.lock().unwrap();
        if guard.is_some() {
            return;
        }
        TICK_THREAD_SHUTDOWN.store(false, Ordering::Relaxed);
        TICK_INTERVAL_MS.store(POLL_INTERVAL_MS.load(Ordering::Relaxed), Ordering::Relaxed);
        let handle = std::thread::Builder::new()
            .name("taskbar-dimmer-tick".into())
            .spawn(tick_thread_loop)
            .expect("taskbar dimmer tick thread");
        *guard = Some(handle);
    }

    fn stop_tick_thread() {
        TICK_THREAD_SHUTDOWN.store(true, Ordering::Relaxed);
        TICK_INTERVAL_MS.store(0, Ordering::Relaxed);
        // Wake the thread so it exits its wait and sees the shutdown flag
        // immediately instead of sleeping out the current interval.
        TICK_WAKE_CONDVAR.notify_all();
        if let Some(threads) = TICK_THREAD.get() {
            if let Ok(mut guard) = threads.lock() {
                if let Some(handle) = guard.take() {
                    let _ = handle.join();
                }
            }
        }
    }

    fn tick_thread_loop() {
        while !TICK_THREAD_SHUTDOWN.load(Ordering::Relaxed) {
            // Read the interval while holding the wake mutex, then wait on the
            // condvar. update_fade performs its interval swap + notify under
            // the same mutex, so the read and the wait are atomic with respect
            // to the notify — a fade that starts between iterations can never
            // be missed (which would otherwise keep the thread sleeping the
            // stale idle interval and subtract it from the fade duration).
            let guard = TICK_WAKE_MUTEX.lock().unwrap();
            let interval = TICK_INTERVAL_MS.load(Ordering::Relaxed);
            if TICK_THREAD_SHUTDOWN.load(Ordering::Relaxed) {
                drop(guard);
                break;
            }
            // Wait the current interval (or a short poll while interval == 0),
            // waking early when update_fade notifies an interval change.
            let wait_ms = if interval == 0 {
                100u64
            } else {
                interval as u64
            };
            let (guard, wait_result) = TICK_WAKE_CONDVAR
                .wait_timeout(guard, Duration::from_millis(wait_ms))
                .unwrap();
            drop(guard);
            if TICK_THREAD_SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }
            // Woken early (interval changed): loop and re-read it rather than
            // posting a tick on a stale cadence.
            if !wait_result.timed_out() {
                continue;
            }
            // Coalesce: don't post another tick if the previous one hasn't
            // been processed yet. This prevents queue buildup during modal
            // loops (window drag, menu open, etc.).
            post_dimmer_tick();
        }
    }

    // Posts a WM_DIMMER_TICK to the overlay, coalescing so at most one tick
    // is queued at a time. Used by the tick thread (scheduled cadence) and by
    // the mouse hook (instant hover-change wake).
    fn post_dimmer_tick() {
        if TICK_POSTED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let overlay_raw = OVERLAY_HWND.load(Ordering::Acquire);
        if overlay_raw == 0 {
            TICK_POSTED.store(false, Ordering::Release);
            return;
        }
        let result = unsafe {
            PostMessageW(
                Some(HWND(overlay_raw as *mut c_void)),
                WM_DIMMER_TICK,
                WPARAM(0),
                LPARAM(0),
            )
        };
        if result.is_err() {
            TICK_POSTED.store(false, Ordering::Release);
        }
    }

    // WH_MOUSE_LL callback. The system delivers every mouse event to this
    // procedure on the installing thread, which is what makes hover response
    // instant. The hover hit-test itself uses GetCursorPos rather than
    // MSLLHOOKSTRUCT.pt: the raw point can sit one or more pixels beyond the
    // screen edge while the cursor rests against the left / bottom / right
    // boundary (the taskbar's screen-edge sides), which would fail the rect
    // test and make the overlay re-appear there. GetCursorPos always clamps
    // to the virtual screen — the same source as the polling fallback — so
    // both detectors agree. Still event-driven: one cheap call per mouse
    // event, zero idle polling.
    unsafe extern "system" fn mouse_hook_proc(
        code: i32,
        wparam: WPARAM,
        _lparam: LPARAM,
    ) -> LRESULT {
        if code >= 0 && MOUSE_HOOK_TOGGLE.load(Ordering::Relaxed) {
            // Any mouse message (move, button, wheel) is a hover candidate,
            // so react to the whole mouse message range WM_MOUSEFIRST..=
            // WM_MOUSEHWHEEL (0x0200..=0x020E).
            let msg_type = wparam.0 as u32;
            if (WM_MOUSEFIRST..=WM_MOUSEHWHEEL).contains(&msg_type) {
                let hovered = {
                    let guard = HOOK_TASKBAR_RECT.lock().unwrap();
                    match *guard {
                        Some(rect) => {
                            let mut point = windows::Win32::Foundation::POINT::default();
                            GetCursorPos(&mut point).is_ok()
                                && point.x >= rect.left
                                && point.x < rect.right
                                && point.y >= rect.top
                                && point.y < rect.bottom
                        }
                        None => false,
                    }
                };
                if CURRENT_HOVER.swap(hovered, Ordering::AcqRel) != hovered {
                    post_dimmer_tick();
                }
            }
        }
        CallNextHookEx(None, code, wparam, _lparam)
    }

    fn mouse_hook_thread_loop() {
        // Registered first so stop_mouse_hook() can find this thread id even
        // if a stop races an in-flight start (WM_QUIT queues before the loop
        // begins and GetMessageW returns immediately).
        MOUSE_HOOK_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
        let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) };
        match hook {
            Ok(hook) => {
                MOUSE_HOOK.store(hook.0 as isize, Ordering::Release);
                emit_runtime("info", "[taskbar-dimmer] mouse hook installed".into());
                let mut msg = MSG::default();
                unsafe {
                    // Low-level hooks only fire while the installing thread
                    // pumps messages; WM_QUIT ends the loop on shutdown.
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        let _ = DispatchMessageW(&msg);
                    }
                }
                let _ = unsafe { UnhookWindowsHookEx(hook) };
                MOUSE_HOOK.store(0, Ordering::Relaxed);
            }
            Err(error) => {
                // Hook unavailable (security software, broken system hook
                // chain) — the tick thread's GetCursorPos fallback still
                // detects hover, just with the idle-timer latency.
                emit_runtime(
                    "warn",
                    format!("[taskbar-dimmer] mouse hook install failed: {error}"),
                );
                MOUSE_HOOK.store(0, Ordering::Relaxed);
            }
        }
        MOUSE_HOOK_THREAD_ID.store(0, Ordering::Relaxed);
    }

    fn ensure_mouse_hook() {
        if !MOUSE_HOOK_TOGGLE.load(Ordering::Relaxed) {
            return;
        }
        let threads = MOUSE_HOOK_THREAD.get_or_init(|| Mutex::new(None));
        let mut guard = threads.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let handle = std::thread::Builder::new()
            .name("taskbar-dimmer-mouse-hook".into())
            .spawn(mouse_hook_thread_loop)
            .expect("taskbar dimmer mouse hook thread");
        *guard = Some(handle);
    }

    fn stop_mouse_hook() {
        // If no hook thread was ever spawned (dimmer never enabled) there is
        // nothing to stop — bail immediately so disable/shutdown paths with
        // the dimmer off don't pay the registration-wait below.
        let thread_exists = MOUSE_HOOK_THREAD
            .get()
            .map(|threads| threads.lock().unwrap().is_some())
            .unwrap_or(false);
        if !thread_exists {
            MOUSE_HOOK.store(0, Ordering::Relaxed);
            return;
        }
        // The hook thread blocks in GetMessageW until WM_QUIT arrives. Post it
        // to the thread id, retrying briefly so a stop racing an in-flight
        // start can't leave the thread parked forever (the id is registered as
        // the very first statement of the thread body).
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut id = MOUSE_HOOK_THREAD_ID.load(Ordering::Relaxed);
        while id == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
            id = MOUSE_HOOK_THREAD_ID.load(Ordering::Relaxed);
        }
        if id != 0 {
            let _ = unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
        if let Some(threads) = MOUSE_HOOK_THREAD.get() {
            if let Ok(mut guard) = threads.lock() {
                if let Some(handle) = guard.take() {
                    let _ = handle.join();
                }
            }
        }
        MOUSE_HOOK.store(0, Ordering::Relaxed);
    }

    fn update_fade(hwnd: HWND) {
        let Some(state) = FADE_STATE.get() else {
            return;
        };
        let now = Instant::now();

        // Fullscreen suppression is decoupled from the animation tick: the
        // expensive detection chain result is cached, so the fade timer can
        // stay fast. The detection source follows the hover mode setting:
        //  - Event mode: the WinEvent hook is the sole detector — rechecks
        //    run only on events (foreground switch, move/size, minimize/
        //    restore, location change), no timer polling at all.
        //  - Poll mode: the timer cadence below is the only path.
        //  - Event mode with a failed WinEvent install: the timer takes
        //    over so detection never silently stops.
        // When the detection toggle is off the overlay never hides for
        // external fullscreen (but SUSPENDED — the app's own monitor-
        // fullscreen strip — still takes effect immediately).
        let fullscreen = if SUSPENDED.load(Ordering::Relaxed) {
            true
        } else {
            let mut fade = state.lock().unwrap();
            // Event mode: the WinEvent hook is the sole fullscreen detector.
            // The check runs only when an event (foreground switch, move/
            // size, minimize/restore, location change) requests a recheck —
            // no timer polling at all. Poll mode keeps the timer cadence;
            // event mode falls back to the timer only when the hook failed
            // to install so detection never silently stops.
            let event_driven = PREFER_EVENT_HOVER.load(Ordering::Relaxed)
                && ELEVATION_WINEVENT_HOOK.load(Ordering::Acquire) != 0;
            let recheck_requested =
                event_driven && FULLSCREEN_RECHECK_REQUESTED.swap(false, Ordering::Relaxed);
            let timer_due = !event_driven
                && now.duration_since(fade.last_fullscreen_check).as_millis() as u64
                    >= FULLSCREEN_CHECK_INTERVAL_MS;
            if recheck_requested || timer_due {
                // since_last_ms shows the gap between consecutive rechecks —
                // during a slow multi-step transition it grows to the full
                // transition tail before the active flag finally drops.
                let check_gap_ms =
                    now.duration_since(fade.last_fullscreen_check).as_secs_f64() * 1000.0;
                fade.last_fullscreen_check = now;
                let prev_active = fade.fullscreen_active;
                drop(fade);
                let active =
                    FULLSCREEN_DETECTION.load(Ordering::Relaxed) && fullscreen_app_active(hwnd);
                state.lock().unwrap().fullscreen_active = active;
                if VERBOSE_LOGGING.load(Ordering::Relaxed) {
                    // fg_class helps confirm the detection source — e.g. the
                    // desktop (WorkerW/Progman) must never count as fullscreen.
                    let fg_class =
                        window_class_name(unsafe { GetForegroundWindow() }).unwrap_or_default();
                    emit_runtime("info", format!(
                        "[taskbar-dimmer] fullscreen_check trigger={} active={} prev={} since_last_ms={:.1} fg_class={}",
                        if recheck_requested { "event" } else { "timer" },
                        active, prev_active, check_gap_ms, fg_class,
                    ));
                }
                active
            } else {
                fade.fullscreen_active
            }
        };
        if fullscreen {
            unsafe {
                if IsWindowVisible(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    if VERBOSE_LOGGING.load(Ordering::Relaxed) {
                        emit_runtime(
                            "info",
                            "[taskbar-dimmer] fullscreen_hide overlay hidden".into(),
                        );
                    }
                }
            }
            return;
        }

        let mut diagnostic = None;
        let mut transition_log = None;
        let mut poll_gap_log = None;
        let mut should_repair_zorder = false;
        let mut should_reshow_overlay = false;
        let mut hovered = false;
        let mut cursor = windows::Win32::Foundation::POINT::default();
        let mut cursor_polled = false;
        let mut alpha_to_apply = None;
        let mut transition_done = false;

        {
            let mut fade = state.lock().unwrap();
            if let Some(previous) = fade.last_poll_tick {
                let gap_ms = now.duration_since(previous).as_secs_f64() * 1000.0;
                let can_report = fade
                    .last_poll_gap_diagnostic
                    .map(|last| now.duration_since(last) >= Duration::from_secs(1))
                    .unwrap_or(true);
                // The tick thread sleeps the FULL interval between ticks, so a
                // healthy gap equals the cadence (the poll interval when idle,
                // 16 ms during a fade) plus a few ms of timer/message overhead
                // — NOT zero. The old fixed 60 ms floor fired on every tick of
                // a 100 ms poll interval, spamming warn once per second. Flag
                // only gaps that exceed the expected cadence by ~1.5x: that is
                // real stutter (stalled message pump, modal loop, slow z-order
                // repair), while a 16 ms fade cadence still keeps a 60 ms floor.
                let interval = TICK_INTERVAL_MS.load(Ordering::Relaxed);
                let expected_ms = if interval == 0 {
                    POLL_INTERVAL_MS.load(Ordering::Relaxed) as f64
                } else {
                    interval as f64
                };
                if gap_ms >= (expected_ms * 1.5).max(60.0) && can_report {
                    fade.last_poll_gap_diagnostic = Some(now);
                    poll_gap_log = Some(format!("[taskbar-dimmer] poll_gap_ms={:.1}", gap_ms));
                }
            }
            fade.last_poll_tick = Some(now);

            if TASKBAR_CACHE_REFRESH.load(Ordering::Relaxed) {
                // Event mode refreshes the cache on taskbar window events
                // (flag set by the WinEvent callback) with only a slow
                // catch-all timer as a safety net; poll mode keeps the
                // regular timer cadence.
                let event_driven_cache = PREFER_EVENT_HOVER.load(Ordering::Relaxed)
                    && ELEVATION_WINEVENT_HOOK.load(Ordering::Acquire) != 0;
                let cache_recheck_requested = event_driven_cache
                    && TASKBAR_CACHE_RECHECK_REQUESTED.swap(false, Ordering::Relaxed);
                let cache_interval = if event_driven_cache {
                    TASKBAR_CACHE_CATCHALL_MS
                } else {
                    TASKBAR_CACHE_REFRESH_MS
                };
                let refresh_cache = fade.cached_taskbar.is_none()
                    || cache_recheck_requested
                    || now
                        .duration_since(fade.last_taskbar_cache_refresh)
                        .as_millis() as u64
                        >= cache_interval;
                if refresh_cache {
                    let trigger = if fade.cached_taskbar.is_none() {
                        "initial"
                    } else if cache_recheck_requested {
                        "event"
                    } else if event_driven_cache {
                        "catchall"
                    } else {
                        "timer"
                    };
                    fade.last_taskbar_cache_refresh = now;
                    let previous = fade.cached_taskbar;
                    fade.cached_taskbar =
                        taskbar_info().map(|(taskbar, rect)| (taskbar.0 as isize, rect));
                    // Keep the hook's hit-test rect in sync with the cache so
                    // the two detectors never disagree about the taskbar.
                    *HOOK_TASKBAR_RECT.lock().unwrap() = fade.cached_taskbar.map(|(_, rect)| rect);
                    if fade.cached_taskbar != previous {
                        should_repair_zorder = true;
                    }
                    if VERBOSE_LOGGING.load(Ordering::Relaxed) {
                        emit_runtime(
                            "info",
                            format!(
                                "[taskbar-dimmer] taskbar_cache refresh trigger={} taskbar={}",
                                trigger,
                                rect_text(fade.cached_taskbar.map(|(_, rect)| rect)),
                            ),
                        );
                    }
                }
            }
            let cached = fade.cached_taskbar;
            let hook_active = MOUSE_HOOK.load(Ordering::Acquire) != 0
                && MOUSE_HOOK_TOGGLE.load(Ordering::Relaxed)
                && PREFER_EVENT_HOVER.load(Ordering::Relaxed);
            let elevated = FOREGROUND_ELEVATED.load(Ordering::Relaxed);
            if let Some((_, rect)) = cached {
                // Poll cursor when:
                // 1) CURSOR_POLLING is explicitly enabled (Debug tab toggle), OR
                // 2) Event mode is active but the foreground is elevated (hook
                //    can't provide accurate positions), so we fall back to
                //    polling until the foreground returns to normal.
                let should_poll =
                    CURSOR_POLLING.load(Ordering::Relaxed) || (hook_active && elevated);
                if should_poll {
                    let (is_hovered, point) = cursor_over_rect(rect);
                    hovered = is_hovered;
                    cursor = point;
                    cursor_polled = true;
                    // Keep the hook's hover cache in sync so an immediate hook
                    // poke after this tick never sees a stale value.
                    CURRENT_HOVER.store(hovered, Ordering::Relaxed);
                } else if hook_active {
                    // Cursor polling off (low-CPU mode): the hook is the sole
                    // hover detector.
                    hovered = CURRENT_HOVER.load(Ordering::Relaxed);
                }
            }

            // Self-heal overlay visibility. The z-order repair that re-shows
            // the overlay is throttled to 250ms and rides the periodic tick,
            // so after a fullscreen exit (or an external hide) it can leave
            // the overlay hidden for up to ~500ms — the visible flicker when
            // toggling fullscreen quickly. Flag a direct re-show whenever the
            // overlay is hidden but the dimmer is on and the cursor is not
            // over the taskbar (i.e. it should be showing at base alpha); the
            // ShowWindow itself runs after the lock is dropped.
            let overlay_visible = unsafe { IsWindowVisible(hwnd).as_bool() };
            if fade.enabled && !hovered && !overlay_visible {
                should_reshow_overlay = true;
                should_repair_zorder = true;
            }

            let new_target = if fade.enabled && !hovered {
                fade.base_alpha
            } else {
                0.0
            };
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
                    fade.max_fade_tick_gap_ms = fade
                        .max_fade_tick_gap_ms
                        .max(now.duration_since(previous).as_secs_f64() * 1000.0);
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
                        let duration = Duration::from_millis(
                            duration_ms.clamp(MIN_EASE_DURATION_MS, MAX_EASE_DURATION_MS),
                        );
                        (now.duration_since(fade.fade_start_time).as_secs_f64()
                            / duration.as_secs_f64())
                        .clamp(0.0, 1.0)
                    };
                    let eased = 1.0 - (1.0 - progress).powi(3);
                    fade.current_alpha = fade.start_alpha + distance * eased;
                    if progress >= 1.0 {
                        fade.current_alpha = fade.target_alpha;
                    }
                    fade.max_alpha_jump =
                        fade.max_alpha_jump.max((fade.current_alpha - before).abs());
                    alpha_to_apply = Some(fade.current_alpha.round().clamp(0.0, 255.0) as u8);
                } else {
                    fade.current_alpha = fade.target_alpha;
                }

                // Only mark completion when this tick was actually advancing
                // a transition. Otherwise an idle timer tick would look like
                // a completed fade and reset the diagnostic counters.
                transition_done =
                    distance.abs() > 0.01 && (fade.target_alpha - fade.current_alpha).abs() <= 0.01;
                if transition_done {
                    fade.current_alpha = fade.target_alpha;
                    fade.start_alpha = fade.target_alpha;
                    fade.transition_active = false;
                    should_repair_zorder = true;
                }
            }

            let active = fade.enabled && fade.transition_active;
            let desired_interval = if active {
                ACTIVE_TIMER_INTERVAL_MS
            } else {
                POLL_INTERVAL_MS.load(Ordering::Relaxed)
            };
            // Swap + notify under the wake mutex so the tick thread's
            // read-then-wait (also under this mutex) sees either the new
            // interval or the notify — never a stale long sleep that would
            // eat into the fade duration.
            {
                let guard = TICK_WAKE_MUTEX.lock().unwrap();
                if TICK_INTERVAL_MS.swap(desired_interval, Ordering::Relaxed) != desired_interval {
                    TICK_WAKE_CONDVAR.notify_all();
                }
                drop(guard);
            }

            if now.duration_since(fade.last_zorder_repair).as_millis() as u64
                >= ZORDER_REPAIR_INTERVAL_MS
            {
                should_repair_zorder = true;
            }
            if should_repair_zorder {
                fade.last_zorder_repair = now;
            }
        }
        // Re-show the overlay outside the lock, matching the codebase pattern
        // of not holding FADE_STATE across window calls (ShowWindow can send
        // WM_SHOWWINDOW / WM_WINDOWPOSCHANGED synchronously).
        if should_reshow_overlay {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            if VERBOSE_LOGGING.load(Ordering::Relaxed) {
                emit_runtime("info", "[taskbar-dimmer] overlay_reshown self-heal".into());
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
                let result =
                    unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA) };
                let duration = started.elapsed().as_micros();
                if let Some(state) = FADE_STATE.get() {
                    let mut fade = state.lock().unwrap();
                    fade.max_layered_update_us = fade.max_layered_update_us.max(duration);
                    if duration >= 2000 {
                        fade.slow_layered_update_count =
                            fade.slow_layered_update_count.saturating_add(1);
                    }
                    if result.is_ok() {
                        // Only suppress future writes after Windows accepted
                        // this one; failures must remain retryable.
                        fade.last_applied_alpha = Some(alpha);
                    }
                }
                if let Err(error) = result {
                    emit_runtime(
                        "warn",
                        format!(
                            "[taskbar-dimmer] layered_update_failed alpha={} error={}",
                            alpha, error,
                        ),
                    );
                }
            }
        }

        // Defer z-order repair to the next message-loop iteration so the
        // animation tick never blocks on SetWindowPos / shell re-parenting.
        // The repair handler checks the throttle clock and the cached taskbar
        // geometry independently.
        if should_repair_zorder && ZORDER_REPAIR.load(Ordering::Relaxed) {
            let _ =
                unsafe { PostMessageW(Some(hwnd), WM_DIMMER_REPAIR_ZORDER, WPARAM(0), LPARAM(0)) };
        }
        if let Some(message) = transition_log {
            emit_runtime("info", message);
        }
        if let Some(message) = poll_gap_log {
            emit_runtime("warn", message);
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
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    Some(rect)
                } else {
                    None
                }
            };
            // Cursor is only meaningful when it was actually polled this
            // tick; in event mode (hook-active, polling off) show n/a instead
            // of a misleading (0, 0).
            let cursor_text = if cursor_polled {
                format!("({}, {})", cursor.x, cursor.y)
            } else {
                "n/a".into()
            };
            emit_runtime("info", format!(
                "[taskbar-dimmer] tick enabled={} hovered={} cursor={} taskbar={} visible={} overlay={} alpha={:.0}->{:.0}",
                enabled, hovered, cursor_text, rect_text(cached_taskbar_for_log.map(|(_, rect)| rect)),
                visible, rect_text(overlay_rect), current, target,
            ));
        }
    }

    pub fn create(_app: &AppHandle) -> Result<isize, String> {
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
        OVERLAY_HWND.store(hwnd.0 as isize, Ordering::Release);

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
            last_hovered: None,
            last_diagnostic: Instant::now(),
            last_poll_tick: None,
            last_poll_gap_diagnostic: None,
            fullscreen_active: false,
            last_fullscreen_check: Instant::now(),
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

    pub fn shutdown() {
        stop_tick_thread();
        stop_mouse_hook();
        stop_elevation_win_event();
    }

    pub fn apply(
        raw: isize,
        enabled: bool,
        opacity: f64,
        fade_in_duration_ms: u64,
        fade_out_duration_ms: u64,
        immediate: bool,
    ) -> Result<(), String> {
        let taskbar = taskbar_info().ok_or_else(|| "Windows taskbar was not found".to_string())?;
        let rect = taskbar.1;
        let hwnd = HWND(raw as *mut c_void);
        let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round();
        let hovered = enabled && cursor_over_rect(rect).0;
        // Seed the hook's hover cache + hit-test rect so it doesn't fire a
        // spurious poke for a state that predates its installation.
        CURRENT_HOVER.store(hovered, Ordering::Relaxed);
        *HOOK_TASKBAR_RECT.lock().unwrap() = if enabled { Some(rect) } else { None };

        let mut immediate_alpha = None;
        if let Some(state) = FADE_STATE.get() {
            let mut fade = state.lock().unwrap();
            fade.enabled = enabled;
            fade.base_alpha = alpha;
            fade.fade_in_duration_ms =
                fade_in_duration_ms.clamp(MIN_EASE_DURATION_MS, MAX_EASE_DURATION_MS);
            fade.fade_out_duration_ms =
                fade_out_duration_ms.clamp(MIN_EASE_DURATION_MS, MAX_EASE_DURATION_MS);
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
            if enabled {
                fade.cached_taskbar = Some((taskbar.0 .0 as isize, rect));
                fade.last_taskbar_cache_refresh = Instant::now();
                ensure_tick_thread();
                // Event mode installs both hooks: the mouse hook for instant
                // hover, and the WinEvent hook for elevation detection +
                // event-driven fullscreen. Poll mode installs neither — the
                // tick thread's cursor poll and fullscreen timer handle
                // everything.
                if PREFER_EVENT_HOVER.load(Ordering::Relaxed) {
                    ensure_elevation_win_event();
                    ensure_mouse_hook();
                }
            } else {
                stop_tick_thread();
                stop_mouse_hook();
                stop_elevation_win_event();
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

            let visibility = if enabled {
                SWP_SHOWWINDOW
            } else {
                SWP_HIDEWINDOW
            };
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
    pub fn available() -> bool {
        false
    }
    pub fn set_verbose_logging(_enabled: bool) {}
    pub fn set_detection(_name: &str, _enabled: bool) {}
    pub fn set_hover_mode(_mode: &str) {}
    pub fn set_poll_interval_ms(_ms: u32) {}
    pub fn set_suspended(_enabled: bool) {}
    pub fn create(_app: &AppHandle) -> Result<isize, String> {
        Err("Taskbar dimmer is only available on Windows".into())
    }
    pub fn apply(
        _raw: isize,
        _enabled: bool,
        _opacity: f64,
        _fade_in_duration_ms: u64,
        _fade_out_duration_ms: u64,
        _immediate: bool,
    ) -> Result<(), String> {
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
    let raw = state
        .overlay
        .lock()
        .unwrap()
        .ok_or_else(|| "taskbar dimmer native overlay is not initialized".to_string())?;
    let fade_in_duration_ms = fade_in_duration_ms.clamp(0, 1000);
    let fade_out_duration_ms = fade_out_duration_ms.clamp(0, 1000);
    platform::apply(
        raw,
        enabled,
        opacity,
        fade_in_duration_ms,
        fade_out_duration_ms,
        immediate,
    )?;
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
pub fn set_taskbar_dimmer_detection(name: String, enabled: bool) {
    platform::set_detection(&name, enabled);
}

#[tauri::command]
pub fn set_taskbar_dimmer_suspended(suspended: bool) {
    platform::set_suspended(suspended);
}

#[tauri::command]
pub fn set_taskbar_dimmer_hover_mode(mode: String) {
    platform::set_hover_mode(&mode);
}

#[tauri::command]
pub fn set_taskbar_dimmer_poll_interval(ms: u32) {
    // UI bounds: the frontend slider clamps 16..200, so keep the backend
    // clamp identical instead of allowing a hidden 2000 ms ceiling.
    platform::set_poll_interval_ms(ms.clamp(16, 200));
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

pub fn shutdown() {
    platform::shutdown();
}
