use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, State};

pub struct TaskbarDimmerState {
    // Store the native HWND directly. A separate WebView2 window can exist
    // without having a usable HWND during startup, which made the old overlay
    // fail when the user enabled the dimmer. The on/off state is per monitor
    // and lives in each monitor's DimmerProfile, so there is no global
    // master toggle here any more.
    pub overlay: Mutex<Option<isize>>,
}

impl Default for TaskbarDimmerState {
    fn default() -> Self {
        Self {
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
    pub hover_detection: String,
    pub poll_interval_ms: u32,
    pub available: bool,
}

// Per-monitor dimmer settings, keyed by the monitor's stable identity
// (monitor_id || sig) in the profiles map. One entry per connected monitor;
// a monitor without an entry uses DimmerProfile::default(). Enabled is
// per monitor (a fresh profile is off); only the detection method (hover
// mode + poll interval) is shared globally.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DimmerProfile {
    pub enabled: bool,
    pub opacity: f64,
    pub fade_in_duration_ms: u64,
    pub fade_out_duration_ms: u64,
}

impl Default for DimmerProfile {
    fn default() -> Self {
        // Must match config.rs's default_dimmer_ease_duration() (300 ms) and
        // the UI defaults for opacity. Enabled defaults to off: a monitor
        // the user has never turned on stays off.
        Self {
            enabled: false,
            opacity: 0.95,
            fade_in_duration_ms: 300,
            fade_out_duration_ms: 300,
        }
    }
}

// Per-monitor settings pushed by the frontend.
static PROFILES: OnceLock<Mutex<HashMap<String, DimmerProfile>>> = OnceLock::new();

pub(crate) fn profiles_map() -> &'static Mutex<HashMap<String, DimmerProfile>> {
    PROFILES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(target_os = "windows")]
mod platform {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::{
        atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering},
        Condvar, Mutex, OnceLock,
    };
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};
    use tauri::AppHandle;
    use windows::core::{BOOL, PCWSTR};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
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
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClassNameW,
        GetCursorInfo, GetCursorPos, GetForegroundWindow, GetMessageW, GetTopWindow, GetWindow,
        GetWindowLongPtrW, GetWindowRect, IsIconic, IsWindowVisible, LoadCursorW, PostMessageW,
        PostThreadMessageW, RegisterClassW, SetCursor, SetLayeredWindowAttributes,
        SetWindowLongPtrW, SetWindowPos, SetWindowsHookExW, ShowWindow, TranslateMessage,
        UnhookWindowsHookEx, CURSORINFO, CURSORINFO_FLAGS, CURSOR_SHOWING, GWLP_HWNDPARENT,
        GWL_EXSTYLE, GWL_STYLE, GW_HWNDNEXT, GW_HWNDPREV, HCURSOR, HTTRANSPARENT, HWND_TOPMOST,
        IDC_ARROW, LWA_ALPHA, MSG, SET_WINDOW_POS_FLAGS, SWP_HIDEWINDOW, SWP_NOACTIVATE,
        SWP_NOSENDCHANGING, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW, WH_MOUSE_LL, WM_APP,
        WM_MOUSEFIRST, WM_MOUSEHWHEEL, WM_NCHITTEST, WM_PAINT, WM_QUIT, WM_SETCURSOR, WNDCLASSW,
        WS_CAPTION, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_EX_TRANSPARENT, WS_MAXIMIZE, WS_POPUP, WS_THICKFRAME,
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
    // After a display-change broadcast the taskbar may not have reached its
    // final geometry yet (Explorer settles a beat later). Refresh the cache
    // on every tick for this long so the overlay tracks the taskbar to its
    // final position instead of sampling once and then waiting out the
    // periodic cadence.
    const DISPLAY_CHANGE_SETTLE_MS: u64 = 500;
    const ZORDER_REPAIR_INTERVAL_MS: u64 = 250;
    // Slow safety net for z-order loss with no shell event to trigger the
    // forced window above (display sleep/wake, which broadcasts no
    // WM_DISPLAYCHANGE, Explorer reflow, another topmost window claiming
    // the front while the app sits idle in the tray). Steady state posts no
    // other repairs, so without this the overlay can sit behind the taskbar
    // until the next fade, shell event, or manual re-apply on window open.
    // The repair handler itself is cheap when in place (reads only, no
    // SetWindowPos), and it stays paused while a popup menu is open or the
    // overlay is hidden for fullscreen.
    const ZORDER_CATCHALL_MS: u64 = 2000;
    // Fullscreen detection runs the expensive Win32 foreground-window chain
    // (GetForegroundWindow → GetWindowThreadProcessId → extended frame
    // bounds → MonitorFromWindow → GetMonitorInfoW → style). It is
    // decoupled from the fade animation tick: the result is cached, so the
    // fade timer can stay as fast as possible without paying the detection
    // cost every tick. This cadence is the detection source in poll hover
    // mode; in event hover mode the WinEvent hook is the primary source and
    // this timer drops to a slow catch-all (FULLSCREEN_CATCHALL_MS) that
    // only exists to self-heal a missed event, never to poll at the full
    // cadence.
    const FULLSCREEN_CHECK_INTERVAL_MS: u64 = 100;
    // Event-mode safety net for fullscreen detection. Even with the WinEvent
    // hook installed, a borderless transition can slip past every subscribed
    // event (the events fire while the window is mid-resize with no follow-up),
    // leaving the overlay floating above the taskbar, which makes the shell
    // keep the taskbar visible over the game for the whole session. In event
    // mode the fullscreen check drops to this slow cadence so a missed event
    // self-heals within a second; poll mode keeps the faster cadence above.
    // The chain only runs when the foreground changed since the last check or
    // a window was covering the monitor (see the gate in update_fade), so a
    // settled desktop never pays for the catch-all at all.
    const FULLSCREEN_CATCHALL_MS: u64 = 1000;
    // Slack (logical px) for the coverage comparison. Borderless games are
    // routinely a pixel or two off the monitor rect (DPI rounding at
    // 125%/150% scaling, invisible DWM resize borders). Only the chrome-less
    // branch uses this tolerance; the chrome-retaining exact-monitor branch
    // stays strict, so a maximized or near-maximized window is never
    // reclassified.
    const FULLSCREEN_RECT_TOLERANCE: i32 = 8;
    const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
    const EVENT_SYSTEM_MENUPOPUPSTART: u32 = 0x0004;
    const EVENT_SYSTEM_MENUPOPUPEND: u32 = 0x0005;
    const EVENT_SYSTEM_MOVESIZESTART: u32 = 0x000A;
    const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000B;
    const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;
    const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;
    const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
    const EVENT_OBJECT_STATECHANGE: u32 = 0x800A;
    const DEFAULT_EASE_DURATION_MS: u64 = 300; // must match config.rs default_dimmer_ease_duration()
    const MIN_EASE_DURATION_MS: u64 = 0;
    const MAX_EASE_DURATION_MS: u64 = 1000;
    // Hover hysteresis margin (logical px): while the overlay is hovered
    // (faded out), the cursor must leave the taskbar rect expanded by this
    // margin before hover clears, and while not hovered it must enter the
    // strict rect. This absorbs GetCursorPos jitter at the taskbar edge
    // that would otherwise flip the fade on every tick (the visible
    // throbbing). Both the mouse hook and the tick poll use the same rule.
    const HOVER_HYSTERESIS_MARGIN: i32 = 3;

    struct FadeState {
        enabled: bool,
        base_alpha: f64,
        fade_in_duration_ms: u64,
        fade_out_duration_ms: u64,
        // Per-overlay hover state (written by the mouse hook in event mode,
        // by the tick cursor poll in poll mode; read by update_fade). Lived
        // in globals before the per-monitor rework.
        hover: bool,
        // Whether this overlay uses event (hook) hover detection vs the tick
        // cursor poll, from its monitor's profile. The global mouse hook
        // still fires; only event-mode overlays consume it. (The poll
        // cadence itself is shared: the tick thread runs at the fastest poll
        // interval across all profiles, set in apply().)
        hover_event: bool,
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
        // (poll mode) or FULLSCREEN_CATCHALL_MS (event mode) so the expensive
        // detection chain doesn't run on every animation tick.
        fullscreen_active: bool,
        // Foreground HWND (raw) the last fullscreen check ran against, and
        // whether that window covered the monitor. Event mode's catch-all
        // only re-runs the Win32 chain when one of these changed meaningfully
        // (foreground switched, or a covering window was seen), so a settled
        // desktop costs nothing between foreground switches.
        last_fullscreen_foreground: isize,
        last_fullscreen_covers: bool,
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

    // One overlay window per monitor's taskbar (primary Shell_TrayWnd plus
    // each Shell_SecondaryTrayWnd). Keyed by overlay HWND so the per-hwnd
    // machinery (update_fade, z-order repair, window proc) finds its state.
    struct OverlayInfo {
        taskbar: isize,
        rect: RECT,
        monitor_key: String,
    }

    // Fade state per overlay HWND (replaces the old single FADE_STATE).
    static FADES: OnceLock<Mutex<HashMap<isize, FadeState>>> = OnceLock::new();
    // Overlay window bookkeeping per overlay HWND.
    static OVERLAYS: OnceLock<Mutex<HashMap<isize, OverlayInfo>>> = OnceLock::new();
    static TICK_THREAD: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
    static VERBOSE_LOGGING: AtomicBool = AtomicBool::new(false);
    // Temporary flicker diagnostics: cumulative counters for fade reversals,
    // overlay hides, and overlay re-shows. Logged with a running total so a
    // flicker storm shows up as a burst of increasing counters instead of a
    // single one-off line. Remove these once the flicker source is confirmed.
    static FLICKER_REVERSAL_COUNT: AtomicU64 = AtomicU64::new(0);
    static FLICKER_HIDE_COUNT: AtomicU64 = AtomicU64::new(0);
    static FLICKER_RESHOW_COUNT: AtomicU64 = AtomicU64::new(0);
    // Diagnostic: how many times the overlay window proc actually handled a
    // WM_SETCURSOR message. The overlay returns HTTRANSPARENT from
    // WM_NCHITTEST, so on Windows the mouse is routed to the window beneath
    // (the taskbar) and the WM_SETCURSOR arm below may never fire. A counter
    // of zero while a cursor problem is live proves the overlay never owns
    // cursor control and the fix must live somewhere else (e.g. the taskbar's
    // cursor or a system-wide SetCursor).
    static WM_SETCURSOR_COUNT: AtomicU64 = AtomicU64::new(0);
    // Diagnostic: the last hCursor handle this process asked Windows to show
    // (overlay WM_SETCURSOR arm, create-overlay class, or the cursor re-show
    // in force_arrow_over_dimmed_taskbar). Lets the probe say whether the
    // arrow was ever set, versus Windows showing whatever state a streaming
    // session left the cursor in.
    static LAST_SET_CURSOR_HANDLE: AtomicIsize = AtomicIsize::new(0);
    // Diagnostic: how many times the arrow was forced back over the dimmed
    // taskbar because Windows reported the cursor hidden (see
    // force_arrow_over_dimmed_taskbar). A reproduction where this climbs
    // while the probe still reports showing=false would prove SetCursor from
    // this process cannot re-show the taskbar cursor, and the fix must move.
    static CURSOR_RESHOW_COUNT: AtomicU64 = AtomicU64::new(0);
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
    // (the calibration strip). fullscreen_app_active() can't detect that case;
    // it intentionally ignores the app's own window so the dimmer keeps
    // working when the app is merely maximized, so the frontend suspends the
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
    // interval elapsed, effectively subtracting the idle timer from the fade
    // duration (a 200ms idle + 200ms fade showed no fade at all).
    static TICK_WAKE_MUTEX: Mutex<()> = Mutex::new(());
    static TICK_WAKE_CONDVAR: Condvar = Condvar::new();
    static SUSPENDED: AtomicBool = AtomicBool::new(false);
    // WH_MOUSE_LL hover detection: instant fade triggers without polling.
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
    // Serializes hook startup with its cleanup. Without this lock, a rapid
    // enable/disable could install a new hook between the cleanup check and
    // stop_mouse_hook().
    static MOUSE_HOOK_LIFECYCLE: Mutex<()> = Mutex::new(());
    static MOUSE_HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
    static MOUSE_HOOK_TOGGLE: AtomicBool = AtomicBool::new(true);

    // True while a system popup menu (any app's context menu) is open. Driven by the WinEvent MENUPOPUPSTART/END hooks when
    // they are installed; the repair's FindWindowW("#32768") scan is the
    // fallback when they are not (poll mode / install failure). While set,
    // z-order repair is paused so it cannot disturb the menu/taskbar boundary.
    static MENU_POPUP_OPEN: AtomicBool = AtomicBool::new(false);
    // When the flag was last set. Lets the repair distinguish a popup that is
    // genuinely open (FindWindowW agrees) from a STALE flag whose
    // MENUPOPUPEND was missed (abnormal dismissal, taskbar restart): a flag
    // that has been set for longer than the grace period while no popup
    // window exists is cleared so the overlay re-shows instead of staying
    // hidden forever.
    static MENU_POPUP_SINCE: Mutex<Option<Instant>> = Mutex::new(None);
    // True when the foreground window is at a higher integrity level than
    // this process (e.g. an elevated admin prompt / game). WH_MOUSE_LL
    // still fires, but MSLLHOOKSTRUCT.pt is zeroed, the hook callback
    // can't GetCursorPos accurately either (the hook's thread is not
    // trusted). When this is true, update_fade falls back to the tick
    // thread's slow GetCursorPos poll so the overlay doesn't freeze.
    static FOREGROUND_ELEVATED: AtomicBool = AtomicBool::new(false);
    // When true (default), the hook is the primary hover detector and the
    // tick thread only polls cursor when FOREGROUND_ELEVATED is true.
    // When false, the hook is never installed and the tick thread always
    // polls cursor at the idle cadence, a universal fallback for users
    // whose security software, remote desktop, or other edge cases
    // conflict with WH_MOUSE_LL.
    // Runtime source of truth for the global Detection Method setting, read
    // by the commands above and the tick/hook threads. pub(super) so the
    // Tauri commands can read the current counterpart when the user changes
    // only the mode or only the interval.
    pub(super) static PREFER_EVENT_HOVER: AtomicBool = AtomicBool::new(true);
    // Set by the WinEvent hooks (foreground switch, move/size, minimize/
    // restore, location change). In event hover mode this is the ONLY
    // trigger for a fullscreen recheck, the timer stands down entirely,
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
    // Set by the display-change notification (WM_DISPLAYCHANGE / WM_DEVICECHANGE,
    // wired from lib.rs). Unlike TASKBAR_CACHE_RECHECK_REQUESTED this is consumed
    // in BOTH hover modes, so a resolution/DPI change repositions the overlay
    // immediately even in poll mode, where the WinEvent flag is never set.
    static TASKBAR_CACHE_FORCE_REFRESH: AtomicBool = AtomicBool::new(false);
    // Timestamp of the last display-change broadcast; while younger than
    // DISPLAY_CHANGE_SETTLE_MS the taskbar cache refreshes on every tick so the
    // overlay follows the taskbar through the transition to its final geometry.
    static DISPLAY_CHANGE_SINCE: Mutex<Option<Instant>> = Mutex::new(None);
    // Throttle for the stale-taskbar rekey (rekey_taskbar_for_overlay). When
    // Explorer recreates Shell_TrayWnd with a new HWND after a display change,
    // GetWindowRect on the dead handle fails on every tick, so without a gate
    // the EnumWindows re-enumeration would run at tick cadence for as long as
    // the taskbar is missing (e.g. during an Explorer restart).
    static LAST_TASKBAR_REKEY: Mutex<Option<Instant>> = Mutex::new(None);
    const TASKBAR_REKEY_INTERVAL_MS: u64 = 1000;
    // After a foreground/minimize transition (Show Desktop, task switch),
    // Explorer can reorder the topmost band, leaving the overlay behind the
    // taskbar. This counter forces the next few z-order repairs to run
    // unconditionally (matching the old 250ms SetWindowPos behaviour), then
    // returns to the normal in_place skip. Set to 3 (= ~750ms at the normal
    // 250ms ZORDER_REPAIR_INTERVAL_MS cadence) on each transition event.
    static FORCE_REPAIR_COUNT: AtomicU32 = AtomicU32::new(0);
    // Diagnostic: timestamp of the last logged WinEvent, used to throttle the
    // winevent log so continuous window drags don't flood the log while still
    // showing the burst pattern of a fullscreen enter/exit transition.
    static LAST_WINEVENT_LOG: Mutex<Option<Instant>> = Mutex::new(None);
    // User-configurable idle polling cadence (ms), used both as the
    // GetCursorPos poll interval in poll mode and as the elevated-fallback
    // poll interval in event mode. Default matches the previous hardcoded
    // IDLE_TIMER_INTERVAL_MS. Bounds enforced in the Tauri command below.
    const DEFAULT_POLL_INTERVAL_MS: u32 = 100; // must match config.rs default_poll_interval() and the frontend store
    pub(super) static POLL_INTERVAL_MS: AtomicU32 = AtomicU32::new(DEFAULT_POLL_INTERVAL_MS);

    // Controls ONLY this module's own verbose diagnostics (tick, winevent,
    // fullscreen/taskbar-cache refreshes, fade summaries). The general
    // vendor/EDID detailed logging is a separate switch (set_detailed_logging
    // in lib.rs) so enabling vendor diagnostics doesn't flood the log with
    // dimmer ticks and vice versa.
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
                if any_fade_enabled() {
                    ensure_mouse_hook();
                }
            } else {
                stop_mouse_hook();
                // No fallback is forced here: the Debug-tab toggles are meant to
                // be independent A/B switches, so disabling the hook leaves
                // cursor polling exactly as the user set it (both may be off ,
                // that is an explicit test configuration). Switching the
                // Detection Method in the dimmer tab re-syncs both defaults via
                // set_hover_mode instead.
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
    // from minimized back into the same fullscreen geometry), those just
    // set the recheck flag without touching FOREGROUND_ELEVATED.
    fn winevent_name(event: u32) -> &'static str {
        match event {
            EVENT_SYSTEM_FOREGROUND => "foreground",
            EVENT_SYSTEM_MENUPOPUPSTART => "menu_popup_start",
            EVENT_SYSTEM_MENUPOPUPEND => "menu_popup_end",
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
        // idObject filtering: restrict to top-level window events (idObject
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
        // While a menu is open, the repair must not rewrite the overlay's
        // z-order: doing so can demote the taskbar or cover a menu. The
        // overlay itself stays visible; MENUPOPUPSTART/END only pause/resume
        // repair activity. The immediate message makes the pause take effect
        // without waiting for the normal 250ms repair cadence.
        if event == EVENT_SYSTEM_MENUPOPUPSTART {
            MENU_POPUP_OPEN.store(true, Ordering::Relaxed);
            *MENU_POPUP_SINCE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            post_dimmer_repair();
        } else if event == EVENT_SYSTEM_MENUPOPUPEND {
            MENU_POPUP_OPEN.store(false, Ordering::Relaxed);
            *MENU_POPUP_SINCE.lock().unwrap_or_else(|e| e.into_inner()) = None;
            post_dimmer_repair();
        }
        let mut elevated = false;
        let foreground_transition = event == EVENT_SYSTEM_FOREGROUND
            || event == EVENT_SYSTEM_MINIMIZESTART
            || event == EVENT_SYSTEM_MINIMIZEEND;
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
                let mut last = LAST_WINEVENT_LOG.lock().unwrap_or_else(|e| e.into_inner());
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
        // fullscreen state, a location/state change on a background window
        // cannot. Filtering here keeps system-wide window churn (other apps
        // animating, tray balloons, background resizes) from triggering
        // main-thread fullscreen rechecks on every event.
        let foreground = unsafe { GetForegroundWindow() };
        let affects_fullscreen =
            event == EVENT_SYSTEM_FOREGROUND || (!foreground.0.is_null() && foreground == hwnd);
        if affects_fullscreen || foreground_transition {
            FULLSCREEN_RECHECK_REQUESTED.store(true, Ordering::Relaxed);
        }
        // Show Desktop changes the foreground to the desktop shell and can
        // minimize/restore a large set of windows without changing the
        // taskbar HWND. Force both caches to be reconsidered so a stale
        // fullscreen/visibility decision cannot leave the overlay hidden
        // until a later mouse event happens to wake it.
        // Determine the foreground class once for both the forced-repair
        // gate and the immediate-repair gate below.
        let is_shell_foreground = || {
            let foreground = unsafe { GetForegroundWindow() };
            window_class_name(foreground)
                .map(|cls| cls == "Progman" || cls == "WorkerW" || cls == "Shell_TrayWnd")
                .unwrap_or(false)
        };
        // Minimize-start/end fire reliably during Show Desktop (Windows
        // minimizes all windows) and are the most robust signal that the
        // shell just reordered the topmost band. The shell-foreground check
        // catches the complementary case: a foreground switch directly to
        // the desktop without a minimize transition.
        let is_minimize = event == EVENT_SYSTEM_MINIMIZESTART || event == EVENT_SYSTEM_MINIMIZEEND;
        if foreground_transition {
            TASKBAR_CACHE_RECHECK_REQUESTED.store(true, Ordering::Relaxed);
            FULLSCREEN_RECHECK_REQUESTED.store(true, Ordering::Release);
            if is_minimize || is_shell_foreground() {
                request_forced_repairs();
            }
        }
        // A foreground switch, minimize, move/size end, or any geometry or
        // state change of the foreground window can start a fullscreen
        // transition, so hide the overlays on those events synchronously, at
        // event time, instead of waiting for the posted tick. The live check
        // keeps this a no-op for plain window switches and moves. Many games
        // activate windowed and then resize programmatically to fullscreen,
        // which raises LOCATIONCHANGE (not MOVESIZEEND), so the foreground-
        // window geometry changes must be covered too. LOCATIONCHANGE fires
        // every frame while the foreground window is dragged, but the chain
        // is a handful of cheap Win32 reads and the check almost always
        // fails for a dragged, non-fullscreen window.
        let may_start_fullscreen =
            foreground_transition || affects_fullscreen || event == EVENT_SYSTEM_MOVESIZEEND;
        if may_start_fullscreen {
            hide_overlays_for_fullscreen_foreground();
        }
        // Event-driven taskbar cache: when an event targets the cached
        // taskbar window itself (drag to another edge, resize, auto-hide
        // slide, minimize), request an immediate cache refresh so the hook
        // hit-test rect and the overlay position react instantly. The hwnd
        // comparison is cheap; a taskbar recreated with a new HWND (Explorer
        // restart) is left to the slow catch-all timer.
        let targets_taskbar = fades_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(|fade| fade.cached_taskbar)
            .any(|(raw, _)| hwnd.0 as isize == raw);
        if targets_taskbar {
            TASKBAR_CACHE_RECHECK_REQUESTED.store(true, Ordering::Relaxed);
            FULLSCREEN_RECHECK_REQUESTED.store(true, Ordering::Relaxed);
        }
        // Poke the overlay to re-evaluate hover/fullscreen. The tick thread
        // will pick up the right source (hook or poll, and the forced
        // fullscreen recheck) on the next tick. An immediate repair message
        // is only needed for taskbar events and desktop-shell transitions
        // (Show Desktop). Normal foreground switches don't need it and can
        // race with popup menu creation; the standard 250ms cadence will
        // quietly re-check z-order on its own schedule.
        post_dimmer_tick();
        if targets_taskbar || foreground_transition {
            post_dimmer_repair();
            // Custom menus may not exist yet when the foreground event
            // fires, the app becomes foreground first, THEN creates its
            // menu. Post a second repair 50ms later so the menu window
            // has time to appear before we scan for it.
            post_dimmer_repair();
        }
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
        // pairs plus a few harmless codes between them, SetWinEventHook
        // takes a min/max range, not a set). This catches interactive drag-
        // resize and minimize/restore, but NOT title-bar double-click
        // maximize or programmatic resizing, neither of those raise
        // MOVESIZESTART/END. The third hook below (LOCATIONCHANGE) is what
        // actually catches those, since it fires on any bounds change
        // regardless of cause; this range is kept anyway because it
        // distinguishes drag-start from drag-end, which LOCATIONCHANGE alone
        // does not. A different numeric range (0x8000+) so it can't be
        // merged into the first hook's range. Multiple SetWinEventHook calls
        // per thread are supported, all deliver to the same message loop
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
        // Popup-menu open/close, the event-driven source for pausing
        // overlay z-order repair while a system menu is open.
        // Distinct numeric range from the other hooks so it stays its own
        // subscription.
        let menupopup_hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_MENUPOPUPSTART,
                EVENT_SYSTEM_MENUPOPUPEND,
                None,
                Some(elevation_win_event_proc),
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::WINEVENT_OUTOFCONTEXT,
            )
        };
        if movesize_hook.0.is_null() {
            emit_runtime("warn", "[taskbar-dimmer] move/size WinEvent hook install failed; fullscreen detection stays timer-based for resize/minimize".into());
        }
        if location_hook.0.is_null() {
            emit_runtime("warn", "[taskbar-dimmer] location-change WinEvent hook install failed; fullscreen detection stays timer-based for in-place resize".into());
        }
        if statechange_hook.0.is_null() {
            emit_runtime("warn", "[taskbar-dimmer] state-change WinEvent hook install failed; style-only fullscreen flips stay undetected in event mode".into());
        }
        if menupopup_hook.0.is_null() {
            emit_runtime("warn", "[taskbar-dimmer] menu popup WinEvent hook install failed; overlay popup-hiding stays timer-based".into());
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
            if !menupopup_hook.0.is_null() {
                let _ = UnhookWinEvent(menupopup_hook);
            }
        }
        // A menu could have been open when the hooks came down; clear the
        // flag so the repair's FindWindowW scan alone decides its z-order.
        MENU_POPUP_OPEN.store(false, Ordering::Relaxed);
        *MENU_POPUP_SINCE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        ELEVATION_WINEVENT_HOOK.store(0, Ordering::Relaxed);
        ELEVATION_WINEVENT_THREAD_ID.store(0, Ordering::Relaxed);
    }

    fn ensure_elevation_win_event() {
        let threads = ELEVATION_WINEVENT_THREAD.get_or_init(|| Mutex::new(None));
        let mut guard = threads.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return;
        }
        let handle = match std::thread::Builder::new()
            .name("taskbar-dimmer-elevation".into())
            .spawn(elevation_win_event_thread_loop)
        {
            Ok(handle) => handle,
            Err(error) => {
                log::error!("[taskbar-dimmer] could not start elevation WinEvent thread: {error}");
                return;
            }
        };
        *guard = Some(handle);
    }

    fn stop_elevation_win_event() {
        let thread_exists = ELEVATION_WINEVENT_THREAD
            .get()
            .map(|threads| threads.lock().unwrap_or_else(|e| e.into_inner()).is_some())
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
        MENU_POPUP_OPEN.store(false, Ordering::Relaxed);
        *MENU_POPUP_SINCE.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
        let verbose_only = level != "warn" && level != "error" && level != "flicker";
        if verbose_only && !VERBOSE_LOGGING.load(Ordering::Relaxed) {
            return;
        }
        match level {
            "warn" => log::warn!("{message}"),
            "error" => log::error!("{message}"),
            _ => log::info!("{message}"),
        }
    }

    // Temporary flicker diagnostics (remove with the counters above): log
    // every fade reversal and every overlay hide/re-show with the running
    // total, so a flicker storm is visible as a burst of incrementing
    // numbers. Uses the "flicker" level which always passes through the
    // verbose gate (the whole point is seeing these without enabling
    // verbose diagnostics).
    fn log_flicker_event(kind: &str, hwnd: isize, detail: &str) {
        let count = match kind {
            "reversal" => FLICKER_REVERSAL_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
            "hide" => FLICKER_HIDE_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
            "reshow" => FLICKER_RESHOW_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
            _ => return,
        };
        // Keep the counters available for diagnostics, but do not emit the
        // high-frequency flicker trace during normal use.
        if crate::detailed_logging_enabled() {
            emit_runtime(
                "flicker",
                format!(
                    "[taskbar-dimmer] flicker_debug kind={} count={} hwnd=0x{:x} {}",
                    kind, count, hwnd, detail,
                ),
            );
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
            // The overlay must never hide the pointer over the taskbar.
            // DefWindowProc only applies the class cursor for HTCLIENT hit
            // tests, and this window returns HTTRANSPARENT, so the class
            // cursor never takes effect and the pointer over the overlay
            // just inherits whatever was last set system-wide. After a
            // streaming session (Sunshine) that can be a hidden cursor,
            // which is exactly the invisible pointer over the dimmed
            // taskbar. Setting the arrow here guarantees the pointer is
            // always visible over the taskbar while the dimmer is on.
            WM_SETCURSOR => {
                // Count every delivery: a live cursor problem with a counter
                // that never rises proves the overlay never receives this
                // message (HTTRANSPARENT routes the mouse away), so the arrow
                // being forced here is irrelevant and the culprit is the
                // window that actually owns the pointer (the taskbar).
                WM_SETCURSOR_COUNT.fetch_add(1, Ordering::Relaxed);
                unsafe {
                    let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
                    LAST_SET_CURSOR_HANDLE.store(arrow.0 as isize, Ordering::Relaxed);
                    let _ = SetCursor(Some(arrow));
                }
                if VERBOSE_LOGGING.load(Ordering::Relaxed) {
                    emit_runtime(
                        "info",
                        format!(
                            "[taskbar-dimmer] overlay received WM_SETCURSOR (count={})",
                            WM_SETCURSOR_COUNT.load(Ordering::Relaxed),
                        ),
                    );
                }
                LRESULT(1)
            }
            WM_DIMMER_TICK => {
                // Tick messages are posted by a background thread so the
                // overlay never owns a WM_TIMER. This avoids SetTimer's
                // message-pump interference during window drags, background
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
                // The repair must never re-show a hidden overlay. Its
                // SetWindowPos carries SWP_SHOWWINDOW (historically how the
                // overlay returned after fullscreen), and re-showing it while
                // a fullscreen app covers the monitor is exactly what keeps
                // the taskbar visible above the game: the shell treats the
                // game as not-fullscreen when a topmost window floats over
                // it, so the taskbar stays up for the whole session until an
                // alt-tab forces a fresh evaluation. Visibility is owned by
                // the fade logic: update_fade's self-heal re-shows the
                // overlay within a tick whenever it should be visible, so a
                // hidden overlay only ever waits one tick, never a repair.
                if !unsafe { IsWindowVisible(hwnd).as_bool() } {
                    return LRESULT(0);
                }
                // Z-order repair is deferred out of the animation tick to
                // prevent SetWindowPos latency from starving the fade timer.
                // The animation tick signals this request via PostMessageW
                // and returns immediately, keeping the WM_TIMER callback fast.
                let hwnd_raw = hwnd.0 as isize;
                // Check the live foreground window in addition to the cached
                // flag: the cached value can lag the real state mid-
                // transition, and re-showing the overlay over a fullscreen
                // game is exactly what keeps the taskbar visible above it.
                // The chain is cheap at the repair cadence (throttled to
                // 250ms and only when a repair is actually requested). Gated
                // by the fullscreen detection toggle like the tick path.
                let fullscreen_now = FULLSCREEN_DETECTION.load(Ordering::Relaxed)
                    && fullscreen_app_active_for_monitor(Some(hwnd), None);
                let mut fades = fades_map().lock().unwrap_or_else(|e| e.into_inner());
                if let Some(fade) = fades.get_mut(&hwnd_raw) {
                    // Skip the repair while the overlay is hidden for
                    // fullscreen (or the app's own fullscreen strip):
                    // maintain_overlay_zorder's SetWindowPos uses
                    // SWP_SHOWWINDOW, which would re-show a hidden overlay
                    // and flash it back on screen. A repair that was already
                    // queued when fullscreen was detected must not execute.
                    // The restore after exit is covered by the self-heal in
                    // update_fade, which posts a fresh repair only after
                    // fullscreen_active is false again.
                    if SUSPENDED.load(Ordering::Relaxed) {
                        return LRESULT(0);
                    }
                    if (fade.fullscreen_active || fullscreen_now)
                        && FORCE_REPAIR_COUNT.load(Ordering::Acquire) == 0
                    {
                        // Skip the repair while the overlay is hidden for
                        // fullscreen, unless we're inside the post-transition
                        // recovery window (Show Desktop / minimize). During
                        // recovery, fullscreen_active can be stale, a
                        // previously queued tick hasn't re-evaluated it yet,
                        // and blocking the repair here would leave the
                        // overlay behind the taskbar until the user triggers
                        // another shell event.
                        return LRESULT(0);
                    }
                    if let Some((raw, rect)) = fade.cached_taskbar {
                        let zorder_started = Instant::now();
                        // Copy the cached taskbar out and release the FADES
                        // lock before the SetWindowPos repair, matching the
                        // old code's discipline of never holding the fade
                        // lock across window calls.
                        drop(fades);
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
            // The overlay sits over the taskbar and must never swallow the
            // cursor. A NULL class cursor would hide the pointer over the
            // window, and although DefWindowProc only applies the class
            // cursor for HTCLIENT hit tests (this window returns
            // HTTRANSPARENT), IDC_ARROW is still the correct class cursor
            // for any HTCLIENT path, a system stock cursor with no ownership
            // to manage. The active guarantee is the WM_SETCURSOR arm in
            // overlay_window_proc, which forces the arrow regardless of the
            // hit-test result.
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
            ..Default::default()
        };
        unsafe {
            // A second registration simply means this process already created
            // its overlay class; the existing class is still valid.
            let _ = RegisterClassW(&class);
        }
    }

    fn overlays_map() -> &'static Mutex<HashMap<isize, OverlayInfo>> {
        OVERLAYS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn fades_map() -> &'static Mutex<HashMap<isize, FadeState>> {
        FADES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    // True when any overlay's dimmer is on (the master toggle is global, but
    // the per-overlay fade states carry the enabled flag).
    fn any_fade_enabled() -> bool {
        fades_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|f| f.enabled)
    }

    fn new_fade_state() -> FadeState {
        FadeState {
            enabled: false,
            base_alpha: 0.0,
            fade_in_duration_ms: DEFAULT_EASE_DURATION_MS,
            fade_out_duration_ms: DEFAULT_EASE_DURATION_MS,
            hover: false,
            hover_event: true,
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
            last_fullscreen_foreground: 0,
            last_fullscreen_covers: false,
            last_fullscreen_check: Instant::now(),
            cached_taskbar: None,
            last_taskbar_cache_refresh: Instant::now(),
            last_zorder_repair: Instant::now(),
            last_applied_alpha: None,
            last_fade_tick: None,
            fade_tick_count: 0,
            max_fade_tick_gap_ms: 0.0,
            max_alpha_jump: 0.0,
            max_layered_update_us: 0,
            slow_layered_update_count: 0,
        }
    }

    unsafe extern "system" fn enum_taskbar_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let list = &mut *(lparam.0 as *mut Vec<(HWND, RECT)>);
        let mut buf = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut buf);
        if len > 0 {
            let class = String::from_utf16_lossy(&buf[..len as usize]);
            if class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd" {
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    list.push((hwnd, rect));
                }
            }
        }
        BOOL(1)
    }

    // Every monitor taskbar: the primary Shell_TrayWnd plus one
    // Shell_SecondaryTrayWnd per secondary monitor (only present when
    // "show taskbar on all displays" is on). Each gets its own overlay.
    fn taskbars() -> Vec<(HWND, RECT)> {
        let mut list: Vec<(HWND, RECT)> = Vec::new();
        unsafe {
            let _ = EnumWindows(
                Some(enum_taskbar_proc),
                LPARAM(&mut list as *mut Vec<(HWND, RECT)> as isize),
            );
        }
        list
    }

    // Shell_TrayWnd can be momentarily missing right after a tray restart
    // while the shell settles. apply() and available() must not turn that
    // transient hiccup into a hard failure, the frontend would then report
    // the overlay unavailable and mis-handle the toggle as off, so retry the
    // lookup briefly before giving up. The happy path still returns on the
    // first attempt with zero added latency.
    fn taskbars_retry(attempts: usize, retry_delay: Duration) -> Vec<(HWND, RECT)> {
        for attempt in 0..attempts {
            let list = taskbars();
            if !list.is_empty() {
                return list;
            }
            if attempt + 1 < attempts {
                std::thread::sleep(retry_delay);
            }
        }
        Vec::new()
    }

    // Stable per-monitor key for a taskbar window: resolve the taskbar's
    // HMONITOR to the monitors table and use the EDID id (sig fallback), so
    // per-monitor profiles survive resolution changes just like the other
    // per-monitor features. Falls back to a rect-based key when the monitor
    // can't be matched.
    fn monitor_key_for_taskbar(taskbar: HWND, rect: RECT) -> String {
        unsafe {
            let hmon = MonitorFromWindow(taskbar, MONITOR_DEFAULTTONEAREST);
            if !hmon.0.is_null() {
                let want = hmon.0 as usize as u32;
                for m in crate::monitors::all_monitors() {
                    if m.hmonitor == want {
                        // The profile identity (EDID id, disambiguated for
                        // identical panels) so each taskbar resolves its own
                        // profile.
                        return m.profile_id.clone();
                    }
                }
            }
        }
        format!(
            "{:x}|{:x}|{:x}|{:x}",
            rect.left, rect.top, rect.right, rect.bottom
        )
    }

    fn rects_intersect(a: RECT, b: RECT) -> bool {
        a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
    }

    // Re-point this overlay at the fresh taskbar HWND after Explorer
    // recreated Shell_TrayWnd (new handle) following a display change. The
    // stored HWND goes stale, GetWindowRect on it fails forever, and with
    // cached_taskbar None both hover detection and z-order repair stop
    // running, leaving the overlay stuck behind the recreated taskbar until
    // the next apply(). Match by monitor: each monitor has exactly one
    // taskbar, so the overlay's own HMONITOR identifies its replacement.
    // Throttled via LAST_TASKBAR_REKEY so a taskbar that is genuinely
    // missing (Explorer restart) doesn't spin EnumWindows at tick cadence.
    fn rekey_taskbar_for_overlay(
        hwnd: HWND,
        hwnd_raw: isize,
        now: Instant,
    ) -> Option<(isize, RECT)> {
        {
            let mut last = LAST_TASKBAR_REKEY.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(prev) = *last {
                if (now.duration_since(prev).as_millis() as u64) < TASKBAR_REKEY_INTERVAL_MS {
                    return None;
                }
            }
            *last = Some(now);
        }
        let overlay_monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        let mut replacement = None;
        for (taskbar, rect) in taskbars() {
            let taskbar_monitor = unsafe { MonitorFromWindow(taskbar, MONITOR_DEFAULTTONEAREST) };
            if taskbar_monitor == overlay_monitor {
                replacement = Some((taskbar.0 as isize, rect));
                break;
            }
        }
        let (new_raw, new_rect) = replacement?;
        {
            let mut overlays = overlays_map().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(info) = overlays.get_mut(&hwnd_raw) {
                info.taskbar = new_raw;
                info.rect = new_rect;
            }
        }
        if VERBOSE_LOGGING.load(Ordering::Relaxed) {
            emit_runtime(
                "info",
                format!(
                    "[taskbar-dimmer] taskbar rekeyed stale hwnd -> 0x{:x} rect={}",
                    new_raw,
                    rect_text(Some(new_rect)),
                ),
            );
        }
        Some((new_raw, new_rect))
    }

    // The "Keep display mode?" flyout (flyout_window.rs) is also an
    // always-on-top popup, shown briefly right after a display mode change.
    // HWND_TOPMOST does not merely keep a window "on top", SetWindowPos with
    // that handle inserts the window at the very FRONT of the whole topmost
    // band, ahead of every other topmost window. This overlay's z-order
    // repair runs on a continuous ~250ms timer (ZORDER_REPAIR_INTERVAL_MS)
    // independent of the flyout's lifecycle, so a repair tick landing while
    // the flyout is visible pushes the flyout behind this overlay, the
    // flyout shows natively (per its own logs) but is never actually seen,
    // hidden behind this fully opaque black overlay. flyout_window.rs
    // publishes its HWND here whenever it is shown/hidden so this repair can
    // insert itself just behind the flyout instead of in front of everything.
    pub static FLYOUT_HWND: AtomicIsize = AtomicIsize::new(0);

    pub fn set_flyout_hwnd(raw: isize) {
        FLYOUT_HWND.store(raw, Ordering::Release);
    }

    fn active_flyout_window() -> Option<HWND> {
        let raw = FLYOUT_HWND.load(Ordering::Acquire);
        if raw == 0 {
            return None;
        }
        let hwnd = HWND(raw as *mut c_void);
        unsafe { IsWindowVisible(hwnd).as_bool() }.then_some(hwnd)
    }

    pub fn available() -> bool {
        // Same transient-miss retry as before: a single snapshot can miss the
        // taskbar right after a tray restart, and the frontend latches a
        // false negative as "overlay unavailable" for the whole session.
        !taskbars_retry(4, Duration::from_millis(150)).is_empty()
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
    fn fullscreen_app_active_for_monitor(
        overlay: Option<HWND>,
        target_monitor: Option<u32>,
    ) -> bool {
        fullscreen_sample(overlay, target_monitor)
            .map(|sample| sample.active)
            .unwrap_or(false)
    }

    // Result of one fullscreen detection pass. Kept as a struct instead of a
    // bare bool so the verbose diagnostic can report exactly which part of
    // the chain decided the result; only the active flag is read by the
    // callers, the remaining fields are diagnostics.
    struct FullscreenSample {
        active: bool,
        covers: bool,
        has_chrome: bool,
        is_maximized: bool,
        frame_rect: RECT,
        monitor_rect: RECT,
    }

    fn fullscreen_sample(
        overlay: Option<HWND>,
        target_monitor: Option<u32>,
    ) -> Option<FullscreenSample> {
        unsafe {
            let foreground = GetForegroundWindow();
            if foreground.0.is_null() || overlay.map(|hwnd| foreground == hwnd).unwrap_or(false) {
                return None;
            }

            let mut process_id = 0u32;
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                foreground,
                Some(&mut process_id),
            );
            if process_id == 0 || IsIconic(foreground).as_bool() {
                return None;
            }

            // Fullscreen suppression is monitor-local. The foreground window
            // may be fullscreen on a secondary display while this overlay is
            // attached to another taskbar; in that case this overlay must stay
            // visible. Do not reject our own process here: the calibration
            // strip can be the fullscreen foreground window too, and the same
            // geometry/style checks below distinguish it from the normal app
            // window.
            let foreground_monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
            if foreground_monitor.0.is_null() {
                return None;
            }
            if let Some(target_monitor) = target_monitor {
                if foreground_monitor.0 as usize as u32 != target_monitor {
                    return None;
                }
            } else if let Some(overlay) = overlay {
                let overlay_monitor = MonitorFromWindow(overlay, MONITOR_DEFAULTTONEAREST);
                if overlay_monitor.0.is_null() || foreground_monitor != overlay_monitor {
                    return None;
                }
            }

            // The desktop itself ("Progman" / "WorkerW") is a top-level window
            // that covers the entire monitor with no window chrome, so it would
            // be misclassified as a fullscreen app whenever the user clicks the
            // desktop, hiding the overlay until the foreground changes back to
            // a real window. Exclude it explicitly.
            if let Some(class) = window_class_name(foreground) {
                if class == "Progman" || class == "WorkerW" {
                    return None;
                }
            }

            let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
            if monitor.0.is_null() {
                return None;
            }
            let mut monitor_info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut monitor_info).as_bool() {
                return None;
            }
            let monitor_rect = monitor_info.rcMonitor;

            // Visible bounds. Prefer DWM's extended frame bounds: they exclude
            // the invisible resize border, so a borderless game resized to the
            // exact monitor size compares cleanly even when GetWindowRect
            // reports the hidden border (which can differ by a few pixels).
            // Falls back to GetWindowRect when DWM cannot report (composition
            // disabled, remote session).
            let mut frame_rect = RECT::default();
            let frame_ok = DwmGetWindowAttribute(
                foreground,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut frame_rect as *mut RECT as *mut c_void,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_ok()
                && frame_rect.right > frame_rect.left
                && frame_rect.bottom > frame_rect.top;
            if !frame_ok {
                if GetWindowRect(foreground, &mut frame_rect).is_err() {
                    return None;
                }
            }

            // Tolerates windows a few pixels short of the monitor (DPI
            // rounding at 125%/150% scaling is routinely off by a pixel or
            // two) and windows that overhang it (invisible borders, shadow).
            let tolerance = FULLSCREEN_RECT_TOLERANCE;
            let covers = frame_rect.left <= monitor_rect.left + tolerance
                && frame_rect.top <= monitor_rect.top + tolerance
                && frame_rect.right >= monitor_rect.right - tolerance
                && frame_rect.bottom >= monitor_rect.bottom - tolerance;

            // Most foreground windows never cover the monitor, so skip the
            // style read entirely unless the frame covers it (or verbose
            // diagnostics want the full picture).
            if !covers && !VERBOSE_LOGGING.load(Ordering::Relaxed) {
                return Some(FullscreenSample {
                    active: false,
                    covers,
                    has_chrome: false,
                    is_maximized: false,
                    frame_rect,
                    monitor_rect,
                });
            }

            // The classification is the original rule: a chrome-less window
            // covering the monitor is fullscreen, and a window that keeps its
            // caption/frame styles only counts when it sits exactly on the
            // monitor bounds unmaximized. The tolerance above only relaxes
            // the coverage gate for chrome-less windows; the exact-monitor
            // branch stays strict, so maximized and near-maximized windows
            // are never reclassified.
            let style = GetWindowLongPtrW(foreground, GWL_STYLE) as u32;
            let has_chrome = (style & (WS_CAPTION.0 | WS_THICKFRAME.0)) != 0;
            let exact_monitor = frame_rect.left == monitor_rect.left
                && frame_rect.top == monitor_rect.top
                && frame_rect.right == monitor_rect.right
                && frame_rect.bottom == monitor_rect.bottom;
            let is_maximized = (style & WS_MAXIMIZE.0) != 0;
            let active = covers && (!has_chrome || (exact_monitor && !is_maximized));
            Some(FullscreenSample {
                active,
                covers,
                has_chrome,
                is_maximized,
                frame_rect,
                monitor_rect,
            })
        }
    }

    // Hides every overlay whose monitor is currently covered by a fullscreen
    // foreground window. Evaluated live from the actual foreground window,
    // not from the cached per-overlay flag, which can lag the real state
    // during a transition. Runs synchronously from the WinEvent callback on
    // the hook thread, so the overlay is out of the way before the shell
    // decides whether the new foreground window is truly fullscreen. A
    // borderless fullscreen game's taskbar only hides when the game is the
    // topmost window at that decision moment; an overlay still floating
    // above it (the posted tick reaches the overlay's message loop a moment
    // later) makes the shell keep the taskbar visible over the game for the
    // whole session. Gated by the fullscreen detection toggle like the tick
    // path, so the Debug-tab A/B switch keeps its meaning.
    fn hide_overlays_for_fullscreen_foreground() {
        if !FULLSCREEN_DETECTION.load(Ordering::Relaxed) {
            return;
        }
        let hwnds: Vec<isize> = overlays_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        for raw in hwnds {
            let hwnd = HWND(raw as *mut c_void);
            if fullscreen_app_active_for_monitor(Some(hwnd), None) {
                unsafe {
                    if IsWindowVisible(hwnd).as_bool() {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        log_flicker_event("hide", raw, "fullscreen (event callback)");
                        if VERBOSE_LOGGING.load(Ordering::Relaxed) {
                            emit_runtime(
                                "info",
                                "[taskbar-dimmer] fullscreen_hide synchronous (event callback)"
                                    .into(),
                            );
                        }
                    }
                }
            }
        }
    }

    pub fn fullscreen_active_on_monitor(hmonitor: u32) -> bool {
        FULLSCREEN_DETECTION.load(Ordering::Relaxed)
            && fullscreen_app_active_for_monitor(None, Some(hmonitor))
    }

    // Hover hit-test with hysteresis. The rect is expanded by
    // HOVER_HYSTERESIS_MARGIN while the overlay is currently hovered and
    // strict otherwise, so the cursor must commit past the boundary before
    // the state flips. Pass the overlay's current hover state so a cursor
    // parked right at the taskbar edge (GetCursorPos jitters a pixel or two
    // across the boundary) doesn't reverse the fade on every tick.
    fn hover_hit_test(
        rect: RECT,
        point: windows::Win32::Foundation::POINT,
        currently_hovered: bool,
    ) -> bool {
        if currently_hovered {
            point.x >= rect.left - HOVER_HYSTERESIS_MARGIN
                && point.x < rect.right + HOVER_HYSTERESIS_MARGIN
                && point.y >= rect.top - HOVER_HYSTERESIS_MARGIN
                && point.y < rect.bottom + HOVER_HYSTERESIS_MARGIN
        } else {
            point.x >= rect.left
                && point.x < rect.right
                && point.y >= rect.top
                && point.y < rect.bottom
        }
    }

    fn cursor_over_rect(
        rect: RECT,
        currently_hovered: bool,
    ) -> (bool, windows::Win32::Foundation::POINT) {
        let mut point = windows::Win32::Foundation::POINT::default();
        unsafe {
            if GetCursorPos(&mut point).is_err() {
                return (currently_hovered, point);
            }
        }
        (hover_hit_test(rect, point, currently_hovered), point)
    }

    // True when Windows reports the cursor is not being drawn at all
    // (GetCursorInfo without CURSOR_SHOWING). After a Sunshine stream ends
    // the cursor can be left in exactly this state over the taskbar: the
    // stream hid it, and nothing has re-shown it, because Explorer only
    // restores its arrow over empty taskbar space on a click, not on hover.
    fn cursor_is_hidden() -> bool {
        unsafe {
            let mut info = CURSORINFO {
                cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                flags: CURSORINFO_FLAGS(0),
                hCursor: HCURSOR(std::ptr::null_mut()),
                ptScreenPos: windows::Win32::Foundation::POINT::default(),
            };
            GetCursorInfo(&mut info).is_ok() && info.flags != CURSOR_SHOWING
        }
    }

    // Force the arrow back over the dimmed taskbar. The overlay cannot do
    // this from WM_SETCURSOR: it is click-through (HTTRANSPARENT plus
    // WS_EX_TRANSPARENT), so that message is routed to the window beneath
    // (the taskbar) and the overlay's WM_SETCURSOR arm never fires, which is
    // exactly what WM_SETCURSOR_COUNT staying at zero proved during the bug.
    // Instead, re-show the cursor from here whenever the pointer sits inside
    // a dimmed taskbar and Windows reports the cursor hidden, mirroring what
    // Chrome's hover and Explorer's own click handling do elsewhere: a
    // SetCursor with a real handle draws the cursor again (the display count
    // is not negative, otherwise those paths could not re-show it either).
    // Only fires when the cursor is hidden, so a legitimately visible cursor
    // (resize handles at the taskbar edge, a custom shell cursor) is never
    // overridden. Returns true when the arrow was set. Callers run this on
    // mouse events (mouse_hook_proc) and on the hover poll (update_fade),
    // never while holding the FADES lock (this function takes it itself).
    fn force_arrow_over_dimmed_taskbar(point: windows::Win32::Foundation::POINT) -> bool {
        if !cursor_is_hidden() {
            return false;
        }
        let mut hit_dimmed_taskbar = false;
        {
            let fades = fades_map().lock().unwrap_or_else(|e| e.into_inner());
            for (raw, fade) in fades.iter() {
                if !fade.enabled {
                    continue;
                }
                let Some((_, rect)) = fade.cached_taskbar else {
                    continue;
                };
                // Only overlays actually covering their taskbar: a hidden
                // overlay (fullscreen or the app's own fullscreen strip)
                // must not force anything.
                if !unsafe { IsWindowVisible(HWND(*raw as *mut _)).as_bool() } {
                    continue;
                }
                // Strict containment (no hysteresis): the pointer must be
                // genuinely inside the taskbar rect.
                if hover_hit_test(rect, point, false) {
                    hit_dimmed_taskbar = true;
                    break;
                }
            }
        }
        if !hit_dimmed_taskbar {
            return false;
        }
        unsafe {
            let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
            if arrow.0.is_null() {
                return false;
            }
            LAST_SET_CURSOR_HANDLE.store(arrow.0 as isize, Ordering::Relaxed);
            CURSOR_RESHOW_COUNT.fetch_add(1, Ordering::Relaxed);
            let _ = SetCursor(Some(arrow));
        }
        true
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
        unsafe {
            // Detect popup menus before changing ownership. If Explorer is
            // While a system menu is open, pause repairs entirely.
            // The GetTopWindow walk below catches visible menus when they
            // exist; this flag covers the brief window where MENUPOPUPSTART
            // has fired but the menu window isn't in the z-order yet.
            if MENU_POPUP_OPEN.load(Ordering::Relaxed) {
                return;
            }

            // No popup is active. Now restore the taskbar owner if Explorer
            // supplied a new taskbar HWND.
            if GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT) != taskbar.0 as isize {
                SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, taskbar.0 as isize);
            }

            // The mode-change flyout is the only other always-on-top popup we

            // must stay behind. It is a stable topmost window, so inserting
            // directly below it is safe (no demotion, no cascade). When it is
            // not visible, claim the front of the topmost band.
            let insert_after = active_flyout_window().or(Some(HWND_TOPMOST)); // Walk the z-order from the top. Custom menus created with
                                                                              // WS_EX_TOPMOST can end up anywhere in the topmost band, often
                                                                              // behind the overlay, which was pushed to the front during
                                                                              // creation. Find the highest topmost window that overlaps the
                                                                              // taskbar (excluding the overlay, taskbar, and flyout) and
                                                                              // insert the overlay directly behind it. This way any menu ,
                                                                              // #32768 or custom class, naturally stays above.
            let mut walk = GetTopWindow(None);
            while let Ok(w) = walk {
                if !w.0.is_null() && w != hwnd && w != taskbar && active_flyout_window() != Some(w)
                {
                    let exstyle = GetWindowLongPtrW(w, GWL_EXSTYLE) as u32;
                    // Z-order is arranged with all topmost windows first,
                    // then non-topmost below. Once we hit a non-topmost
                    // window we've passed the entire topmost band, stop.
                    if (exstyle & WS_EX_TOPMOST.0) == 0 {
                        break;
                    }
                    if (exstyle & WS_EX_TRANSPARENT.0) != 0 {
                        walk = GetWindow(w, GW_HWNDNEXT);
                        continue;
                    }
                    let mut w_rect = RECT::default();
                    if GetWindowRect(w, &mut w_rect).is_ok() && rects_intersect(w_rect, rect) {
                        let _ = SetWindowPos(
                            hwnd,
                            Some(w),
                            rect.left,
                            rect.top,
                            rect.right - rect.left,
                            rect.bottom - rect.top,
                            SWP_SHOWWINDOW | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
                        );
                        return;
                    }
                }
                walk = GetWindow(w, GW_HWNDNEXT);
            }

            // Skip the SetWindowPos when the overlay is already where we want
            // it: visible, and either directly below the flyout or in the
            // topmost band (the topmost style plus the taskbar ownership
            // guarantees it sits above the taskbar). GetWindow /
            // GetWindowLongPtrW only READ the z-order, no DWM reflow, no
            // shell notifications, so steady-state repairs become nearly
            // free instead of re-triggering the window-manager churn (and the
            // slow_zorder_update warnings) that a bare SetWindowPos every
            // 250ms caused.
            // During a shell transition (Show Desktop etc.) Explorer can
            // reorder the topmost band so the overlay ends up behind the
            // taskbar while still looking "in place" (WS_EX_TOPMOST set).
            // Replicate the old unconditional repair for a few ticks after
            // a foreground/minimize event, but only when no native popup
            // menu is tracked.
            let forced = FORCE_REPAIR_COUNT.load(Ordering::Acquire) > 0
                && !MENU_POPUP_OPEN.load(Ordering::Relaxed);
            if forced {
                FORCE_REPAIR_COUNT.fetch_sub(1, Ordering::Release);
            }
            // The z-order can be perfect while the overlay still sits at the
            // OLD taskbar geometry, after a resolution/refresh-rate change
            // (or a taskbar edge move) the cached rect updates to the new
            // monitor bounds but the overlay only follows when a repair
            // actually repositions it. The in_place skip below must therefore
            // ALSO verify the overlay's current rect matches the cached
            // taskbar rect; otherwise the SetWindowPos that would move it to
            // the new resolution is skipped on every repair and the overlay
            // stays glued to the old position. GetWindowRect is a cheap read,
            // so steady-state repairs (rect unchanged) remain a no-op.
            let mut overlay_rect = RECT::default();
            let geometry_matches = GetWindowRect(hwnd, &mut overlay_rect).is_ok()
                && overlay_rect.left == rect.left
                && overlay_rect.top == rect.top
                && overlay_rect.right == rect.right
                && overlay_rect.bottom == rect.bottom;
            // The TOPMOST style flag alone cannot prove the overlay is above
            // the taskbar: Explorer can reinsert the taskbar ahead of the
            // overlay (display sleep/wake, shell reflow) while both keep the
            // flag, so the old check looked in place while the overlay was
            // fully occluded. Walk upward from the overlay; finding the
            // taskbar above it means the overlay lost its position.
            let above_taskbar = {
                let mut above = true;
                let mut cur = GetWindow(hwnd, GW_HWNDPREV);
                for _ in 0..64 {
                    let Ok(w) = cur else { break };
                    if w.0.is_null() {
                        break;
                    }
                    if w == taskbar {
                        above = false;
                        break;
                    }
                    cur = GetWindow(w, GW_HWNDPREV);
                }
                above
            };
            let in_place = geometry_matches
                && IsWindowVisible(hwnd).as_bool()
                && !forced
                && above_taskbar
                && match insert_after {
                    Some(target) if target == HWND_TOPMOST => {
                        (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0) != 0
                    }
                    Some(target) => GetWindow(hwnd, GW_HWNDPREV) == Ok(target),
                    None => false,
                };
            if in_place {
                return;
            }

            if VERBOSE_LOGGING.load(Ordering::Relaxed) {
                // The fullscreen path hides the overlay with SW_HIDE; nothing
                // else re-shows it except this SetWindowPos(SWP_SHOWWINDOW),
                // so a hidden→visible transition here marks the restore.
                if !IsWindowVisible(hwnd).as_bool() {
                    log_flicker_event("reshow", hwnd.0 as isize, "zorder repair");
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
                SWP_SHOWWINDOW | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
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
        let mut guard = threads.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return;
        }
        TICK_THREAD_SHUTDOWN.store(false, Ordering::Relaxed);
        TICK_INTERVAL_MS.store(POLL_INTERVAL_MS.load(Ordering::Relaxed), Ordering::Relaxed);
        let handle = match std::thread::Builder::new()
            .name("taskbar-dimmer-tick".into())
            .spawn(tick_thread_loop)
        {
            Ok(handle) => handle,
            Err(error) => {
                log::error!("[taskbar-dimmer] could not start tick thread: {error}");
                return;
            }
        };
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
            // to the notify; a fade that starts between iterations can never
            // be missed (which would otherwise keep the thread sleeping the
            // stale idle interval and subtract it from the fade duration).
            let guard = TICK_WAKE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
                .unwrap_or_else(|e| e.into_inner());
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
        // Tick every overlay; each clears TICK_POSTED when its own
        // WM_DIMMER_TICK is processed, which still coalesces bursts.
        let hwnds: Vec<isize> = overlays_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        if hwnds.is_empty() {
            TICK_POSTED.store(false, Ordering::Release);
            return;
        }
        let mut any_posted = false;
        for raw in hwnds {
            let result = unsafe {
                PostMessageW(
                    Some(HWND(raw as *mut c_void)),
                    WM_DIMMER_TICK,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
            any_posted |= result.is_ok();
        }
        if !any_posted {
            TICK_POSTED.store(false, Ordering::Release);
        }
    }

    // When the foreground/minimize hook fires, request several forced
    // repairs. This is lightweight: just bumps a counter; the regular 250ms
    // repair cycle consumes it. 5 repairs × 250ms = 1250ms recovery window ,
    // enough for the shell to fully settle after Show Desktop.
    fn request_forced_repairs() {
        FORCE_REPAIR_COUNT.store(5, Ordering::Release);
    }

    // Immediate z-order repair request that bypasses the 250ms repair
    // throttle. Used by the WinEvent hook on menu popup open/close so repair
    // pauses or resumes in the same frame instead of up to 250ms later.
    // Idempotent, the repair handler re-checks the actual state before acting.
    fn post_dimmer_repair() {
        let hwnds: Vec<isize> = overlays_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        for raw in hwnds {
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(raw as *mut c_void)),
                    WM_DIMMER_REPAIR_ZORDER,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        }
    }

    // React to a Windows display-change broadcast (WM_DISPLAYCHANGE /
    // WM_DEVICECHANGE, wired from lib.rs). During a resolution/DPI change the
    // taskbar moves to its new geometry, and the periodic cache cadence (1s
    // in poll mode, the 5s catch-all in event mode) would otherwise leave the
    // overlay floating at the old position for a visible beat. Force an
    // immediate refresh and reposition, and keep refreshing through the
    // settle window so the overlay tracks the taskbar to its final position.
    pub fn on_display_change() {
        TASKBAR_CACHE_FORCE_REFRESH.store(true, Ordering::Relaxed);
        FULLSCREEN_RECHECK_REQUESTED.store(true, Ordering::Relaxed);
        *DISPLAY_CHANGE_SINCE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        // Explorer can reorder the topmost band after a display change; the
        // forced-repair window re-asserts the overlay's position and z-order
        // a few times at the 250ms cadence, covering a late shell reflow.
        request_forced_repairs();
        post_dimmer_tick();
        post_dimmer_repair();
    }

    // WH_MOUSE_LL callback. The system delivers every mouse event to this
    // procedure on the installing thread, which is what makes hover response
    // instant. The hover hit-test itself uses GetCursorPos rather than
    // MSLLHOOKSTRUCT.pt: the raw point can sit one or more pixels beyond the
    // screen edge while the cursor rests against the left / bottom / right
    // boundary (the taskbar's screen-edge sides), which would fail the rect
    // test and make the overlay re-appear there. GetCursorPos always clamps
    // to the virtual screen, the same source as the polling fallback, so
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
                let mut point = windows::Win32::Foundation::POINT::default();
                let point_ok = GetCursorPos(&mut point).is_ok();
                // Per-overlay hover: only event-mode overlays consume the
                // hook; poll-mode overlays get their hover from the tick.
                let changed = {
                    let mut fades = fades_map().lock().unwrap_or_else(|e| e.into_inner());
                    let mut changed = false;
                    for (_, fade) in fades.iter_mut() {
                        if !fade.hover_event {
                            continue;
                        }
                        let hovered = match fade.cached_taskbar {
                            Some((_, rect)) => hover_hit_test(rect, point, fade.hover),
                            None => false,
                        };
                        if fade.hover != hovered {
                            fade.hover = hovered;
                            changed = true;
                        }
                    }
                    changed
                };
                if changed {
                    post_dimmer_tick();
                }
                // A mouse event just landed over the dimmed taskbar. If the
                // streaming session left the cursor hidden, force the arrow
                // back immediately so hovering the taskbar reveals it without
                // requiring a click. No-op while the cursor is visible.
                if point_ok {
                    force_arrow_over_dimmed_taskbar(point);
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
                // chain), the tick thread's GetCursorPos fallback still
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
        let _lifecycle_guard = MOUSE_HOOK_LIFECYCLE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !MOUSE_HOOK_TOGGLE.load(Ordering::Relaxed) {
            return;
        }
        let threads = MOUSE_HOOK_THREAD.get_or_init(|| Mutex::new(None));
        let mut guard = threads.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return;
        }
        let handle = match std::thread::Builder::new()
            .name("taskbar-dimmer-mouse-hook".into())
            .spawn(mouse_hook_thread_loop)
        {
            Ok(handle) => handle,
            Err(error) => {
                log::error!("[taskbar-dimmer] could not start mouse hook thread: {error}");
                return;
            }
        };
        *guard = Some(handle);
    }

    fn stop_mouse_hook() {
        let _lifecycle_guard = MOUSE_HOOK_LIFECYCLE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        stop_mouse_hook_unlocked();
    }

    fn stop_mouse_hook_unlocked() {
        // If no hook thread was ever spawned (dimmer never enabled) there is
        // nothing to stop, bail immediately so disable/shutdown paths with
        // the dimmer off don't pay the registration-wait below.
        let thread_exists = MOUSE_HOOK_THREAD
            .get()
            .map(|threads| threads.lock().unwrap_or_else(|e| e.into_inner()).is_some())
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
        let hwnd_raw = hwnd.0 as isize;
        // This overlay's own taskbar window (copied before the FADES lock so
        // the cache refresh below can re-fetch its rect without nesting a
        // second lock).
        let mut overlay_taskbar = overlays_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&hwnd_raw)
            .map(|info| info.taskbar);
        let now = Instant::now();
        // Self-heal a stale taskbar HWND: after a display change Explorer can
        // recreate Shell_TrayWnd with a new handle, and GetWindowRect on the old
        // handle fails forever. With cached_taskbar None, hover detection and
        // z-order repair both stop (the overlay ends up stuck behind the
        // recreated taskbar) until the next apply() happens to re-enumerate.
        // Re-point this overlay at the fresh taskbar on its monitor before the
        // cache refresh below runs. This runs with no lock held, so it can never
        // invert the OVERLAYS -> FADES lock order.
        if let Some(raw) = overlay_taskbar {
            let mut r = RECT::default();
            let resolvable = unsafe { GetWindowRect(HWND(raw as *mut c_void), &mut r) }.is_ok();
            if !resolvable {
                overlay_taskbar = rekey_taskbar_for_overlay(hwnd, hwnd_raw, now)
                    .map(|(raw, _)| raw)
                    .or(Some(raw));
            }
        }

        // Fullscreen suppression is decoupled from the animation tick: the
        // expensive detection chain result is cached, so the fade timer can
        // stay fast. The detection source follows the hover mode setting:
        //  - Event mode: the WinEvent hook is the sole detector, rechecks
        //    run only on events (foreground switch, move/size, minimize/
        //    restore, location change), no timer polling at all.
        //  - Poll mode: the timer cadence below is the only path.
        //  - Event mode with a failed WinEvent install: the timer takes
        //    over so detection never silently stops.
        // When the detection toggle is off the overlay never hides for
        // external fullscreen (but SUSPENDED, the app's own monitor-
        // fullscreen strip, still takes effect immediately).
        let fullscreen = if SUSPENDED.load(Ordering::Relaxed) {
            // `SUSPENDED` is requested by the app's own monitor-fullscreen
            // calibration mode. Resolve it through the same monitor-local
            // detector instead of hiding every overlay globally.
            fullscreen_app_active_for_monitor(Some(hwnd), None)
        } else {
            let mut fades = fades_map().lock().unwrap_or_else(|e| e.into_inner());
            let Some(fade) = fades.get_mut(&hwnd_raw) else {
                return;
            };
            // Event mode: the WinEvent hook is the primary detector, but a
            // slow catch-all keeps the check alive even there so a borderless
            // transition that slips past every event self-heals within a
            // second. Poll mode keeps the faster cadence. The cadence only
            // gates when the expensive Win32 chain runs; every tick reads
            // the cached result.
            let event_driven = PREFER_EVENT_HOVER.load(Ordering::Relaxed)
                && ELEVATION_WINEVENT_HOOK.load(Ordering::Acquire) != 0;
            let recheck_requested =
                event_driven && FULLSCREEN_RECHECK_REQUESTED.swap(false, Ordering::Relaxed);
            let fullscreen_cadence_ms = if event_driven {
                FULLSCREEN_CATCHALL_MS
            } else {
                FULLSCREEN_CHECK_INTERVAL_MS
            };
            let timer_due = now.duration_since(fade.last_fullscreen_check).as_millis() as u64
                >= fullscreen_cadence_ms;
            // Event mode's catch-all only re-runs the expensive chain when
            // something might have changed: the foreground switched since the
            // last check, or the last checked window covered the monitor (a
            // fullscreen transition in progress). A settled desktop with a
            // normal foreground window costs nothing between foreground
            // switches. Poll mode keeps checking unconditionally, that is its
            // whole contract.
            let foreground_raw = unsafe { GetForegroundWindow() }.0 as isize;
            let catchall_relevant = !event_driven
                || foreground_raw != fade.last_fullscreen_foreground
                || fade.last_fullscreen_covers;
            if recheck_requested || (timer_due && catchall_relevant) {
                // since_last_ms shows the gap between consecutive rechecks ,
                // during a slow multi-step transition it grows to the full
                // transition tail before the active flag finally drops.
                let check_gap_ms =
                    now.duration_since(fade.last_fullscreen_check).as_secs_f64() * 1000.0;
                fade.last_fullscreen_check = now;
                let prev_active = fade.fullscreen_active;
                // End both borrows so the guard is released before the
                // expensive Win32 detection chain (same discipline as the
                // original single-overlay code), then reacquire to write the
                // result back.
                let _ = fade;
                drop(fades);
                let sample = if FULLSCREEN_DETECTION.load(Ordering::Relaxed) {
                    fullscreen_sample(Some(hwnd), None)
                } else {
                    None
                };
                let active = sample.as_ref().map(|s| s.active).unwrap_or(false);
                if let Some(f) = fades_map()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&hwnd_raw)
                {
                    f.last_fullscreen_foreground = foreground_raw;
                    f.last_fullscreen_covers = sample.as_ref().map(|s| s.covers).unwrap_or(false);
                    f.fullscreen_active = active;
                }
                if VERBOSE_LOGGING.load(Ordering::Relaxed) {
                    // fg_class helps confirm the detection source, e.g. the
                    // desktop (WorkerW/Progman) must never count as fullscreen.
                    let fg_class =
                        window_class_name(unsafe { GetForegroundWindow() }).unwrap_or_default();
                    match &sample {
                        Some(s) => emit_runtime("info", format!(
                            "[taskbar-dimmer] fullscreen_check trigger={} active={} prev={} since_last_ms={:.1} fg_class={} covers={} chrome={} maximized={} frame={} monitor={}",
                            if recheck_requested { "event" } else { "timer" },
                            active, prev_active, check_gap_ms, fg_class,
                            s.covers, s.has_chrome, s.is_maximized,
                            rect_text(Some(s.frame_rect)), rect_text(Some(s.monitor_rect)),
                        )),
                        None => emit_runtime("info", format!(
                            "[taskbar-dimmer] fullscreen_check trigger={} active={} prev={} since_last_ms={:.1} fg_class={} rejected",
                            if recheck_requested { "event" } else { "timer" },
                            active, prev_active, check_gap_ms, fg_class,
                        )),
                    }
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
                    log_flicker_event("hide", hwnd_raw, "fullscreen (tick)");
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
        let mut force_cursor_arrow = false;
        let mut alpha_to_apply = None;
        let mut transition_done = false;

        {
            let mut fades = fades_map().lock().unwrap_or_else(|e| e.into_inner());
            let Some(fade) = fades.get_mut(&hwnd_raw) else {
                return;
            };
            if let Some(previous) = fade.last_poll_tick {
                let gap_ms = now.duration_since(previous).as_secs_f64() * 1000.0;
                let can_report = fade
                    .last_poll_gap_diagnostic
                    .map(|last| now.duration_since(last) >= Duration::from_secs(1))
                    .unwrap_or(true);
                // The tick thread sleeps the FULL interval between ticks, so a
                // healthy gap equals the cadence (the poll interval when idle,
                // 16 ms during a fade) plus a few ms of timer/message overhead,
                // not zero. The old fixed 60 ms floor fired on every tick of
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
                // The display-change notification forces a refresh in both
                // modes; the WinEvent flag below only ever fires in event
                // mode (poll mode has no hook to set it).
                let force_recheck = TASKBAR_CACHE_FORCE_REFRESH.swap(false, Ordering::Relaxed);
                let cache_recheck_requested = force_recheck
                    || (event_driven_cache
                        && TASKBAR_CACHE_RECHECK_REQUESTED.swap(false, Ordering::Relaxed));
                let cache_interval = if event_driven_cache {
                    TASKBAR_CACHE_CATCHALL_MS
                } else {
                    TASKBAR_CACHE_REFRESH_MS
                };
                // Right after a display change the taskbar can still be
                // settling; refresh on every tick for the settle window so
                // the overlay tracks it to its final geometry instead of
                // sampling once and then waiting out the cadence.
                let settling = DISPLAY_CHANGE_SINCE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .map(|since| {
                        (now.duration_since(since).as_millis() as u64) < DISPLAY_CHANGE_SETTLE_MS
                    })
                    .unwrap_or(false);
                let refresh_cache = fade.cached_taskbar.is_none()
                    || settling
                    || cache_recheck_requested
                    || now
                        .duration_since(fade.last_taskbar_cache_refresh)
                        .as_millis() as u64
                        >= cache_interval;
                if refresh_cache {
                    let trigger = if fade.cached_taskbar.is_none() {
                        "initial"
                    } else if force_recheck {
                        "display"
                    } else if cache_recheck_requested {
                        "event"
                    } else if settling {
                        "settle"
                    } else if event_driven_cache {
                        "catchall"
                    } else {
                        "timer"
                    };
                    fade.last_taskbar_cache_refresh = now;
                    let previous = fade.cached_taskbar;
                    // Per-overlay refresh: re-fetch this overlay's OWN
                    // taskbar's current rect (the taskbar HWND is stable; only
                    // its geometry moves). The mouse hook hit-tests this same
                    // cached rect, so the two detectors stay in sync.
                    fade.cached_taskbar = overlay_taskbar.and_then(|raw| {
                        let mut r = RECT::default();
                        if unsafe { GetWindowRect(HWND(raw as *mut c_void), &mut r) }.is_ok() {
                            Some((raw, r))
                        } else {
                            None
                        }
                    });
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
                // Show Desktop can leave the low-level mouse hook's cached
                // hover state at the taskbar button that was clicked. During
                // the short, event-triggered shell-recovery window, refresh
                // the hit-test directly so the overlay's alpha follows the
                // cursor even if Windows emits no further mouse event. This
                // is bounded transition recovery, not a normal idle poll.
                // Poll-mode overlays (or the explicit Debug toggles, or an
                // elevated foreground the hook can't see) always poll on the
                // tick; event-mode overlays use the hook's cached hover.
                let should_poll = CURSOR_POLLING.load(Ordering::Relaxed)
                    || (hook_active && elevated)
                    || !fade.hover_event
                    // Event mode with the mouse hook expected but not
                    // actually installed (install failed, or the hook thread
                    // has not registered yet): fall back to the cursor poll
                    // so hover detection never silently stops, matching the
                    // promise on mouse_hook_thread_loop. Gated on
                    // MOUSE_HOOK_TOGGLE so the Debug-tab A/B switch (hook
                    // deliberately off) still stands as an explicit test
                    // configuration.
                    || (fade.hover_event
                        && MOUSE_HOOK_TOGGLE.load(Ordering::Relaxed)
                        && MOUSE_HOOK.load(Ordering::Acquire) == 0);
                if should_poll {
                    let (is_hovered, point) = cursor_over_rect(rect, fade.hover);
                    hovered = is_hovered;
                    cursor = point;
                    cursor_polled = true;
                    // Keep the overlay's hover cache in sync so an immediate
                    // hook poke after this tick never sees a stale value.
                    fade.hover = hovered;
                    // Poll tick found the pointer over this overlay's dimmed
                    // taskbar; the arrow re-show runs after the lock drops.
                    if fade.enabled && hovered {
                        force_cursor_arrow = true;
                    }
                } else if hook_active {
                    // Cursor polling off (low-CPU mode): the hook is the sole
                    // hover detector.
                    hovered = fade.hover;
                }
            }

            // Self-heal overlay visibility. The z-order repair that re-shows
            // the overlay is throttled to 250ms and rides the periodic tick,
            // so after a fullscreen exit (or an external hide) it can leave
            // the overlay hidden for up to ~500ms, the visible flicker when
            // toggling fullscreen quickly. Flag a direct re-show whenever the
            // overlay is hidden but the dimmer is on and the cursor is not
            // over the taskbar (i.e. it should be showing at base alpha); the            // ShowWindow itself runs after the lock is dropped. Context menus
            // no longer hide the overlay, so they do not need to gate this
            // recovery path; an external hide or Show Desktop transition can
            // therefore recover on the next tick without requiring a click.
            // Do not require !hovered here: Show Desktop leaves the cursor
            // over the taskbar button, and that was the exact case where the
            // old condition prevented recovery until another click/move.
            let overlay_visible = unsafe { IsWindowVisible(hwnd).as_bool() };
            if fade.enabled && !overlay_visible {
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
                    log_flicker_event(
                        "reversal",
                        hwnd_raw,
                        &format!("alpha {:.0} -> {:.0}", fade.current_alpha, new_target),
                    );
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
            // interval or the notify, never a stale long sleep that would
            // eat into the fade duration.
            {
                let guard = TICK_WAKE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
                if TICK_INTERVAL_MS.swap(desired_interval, Ordering::Relaxed) != desired_interval {
                    TICK_WAKE_CONDVAR.notify_all();
                }
                drop(guard);
            }

            // Z-order repairs run during the recovery window after a shell
            // transition (Show Desktop, minimize/restore), plus a slow
            // catch-all so silent reorderings self-heal with no event at
            // all. The steady-state repair is a no-op via in_place during
            // normal operation, but SetWindowPos can still race with
            // popup-menu creation, so the fast cadence stays gated behind
            // FORCE_REPAIR_COUNT while the catch-all uses its own slower
            // interval. Both paths respect the popup/fullscreen pauses in
            // the repair handler.
            let since_repair_ms = now.duration_since(fade.last_zorder_repair).as_millis() as u64;
            if (FORCE_REPAIR_COUNT.load(Ordering::Acquire) > 0
                && since_repair_ms >= ZORDER_REPAIR_INTERVAL_MS)
                || since_repair_ms >= ZORDER_CATCHALL_MS
            {
                should_repair_zorder = true;
            }
            if should_repair_zorder {
                fade.last_zorder_repair = now;
            }
        }
        // The cursor poll found the pointer over this overlay's dimmed
        // taskbar. Re-show the arrow if Windows reports it hidden, outside
        // the FADES lock (force_arrow_over_dimmed_taskbar takes it itself).
        if force_cursor_arrow {
            force_arrow_over_dimmed_taskbar(cursor);
        }
        // Re-show the overlay outside the lock, matching the codebase pattern
        // of not holding FADE_STATE across window calls (ShowWindow can send
        // WM_SHOWWINDOW / WM_WINDOWPOSCHANGED synchronously).
        if should_reshow_overlay {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            log_flicker_event("reshow", hwnd_raw, "self-heal");
            if VERBOSE_LOGGING.load(Ordering::Relaxed) {
                emit_runtime("info", "[taskbar-dimmer] overlay_reshown self-heal".into());
            }
        }
        let cached_taskbar_for_log = fades_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&hwnd_raw)
            .and_then(|fade| fade.cached_taskbar);

        if let Some(alpha) = alpha_to_apply {
            let should_apply = fades_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&hwnd_raw)
                .map(|fade| fade.last_applied_alpha != Some(alpha))
                .unwrap_or(false);
            if should_apply {
                let started = Instant::now();
                let result =
                    unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA) };
                let duration = started.elapsed().as_micros();
                {
                    let mut fades = fades_map().lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(fade) = fades.get_mut(&hwnd_raw) {
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
            let summary = {
                let mut fades = fades_map().lock().unwrap_or_else(|e| e.into_inner());
                if let Some(fade) = fades.get_mut(&hwnd_raw) {
                    if fade.fade_tick_count > 0 {
                        let summary = transition_summary(&fade, "complete", now);
                        fade.fade_tick_count = 0;
                        fade.last_fade_tick = None;
                        fade.max_fade_tick_gap_ms = 0.0;
                        fade.max_alpha_jump = 0.0;
                        fade.max_layered_update_us = 0;
                        fade.slow_layered_update_count = 0;
                        Some(summary)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(summary) = summary {
                emit_runtime("info", summary);
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

    // Create one overlay window (the class is registered once). Overlay
    // windows are created lazily per taskbar by apply(); this only creates a
    // single window so the startup path keeps a handle to prove the class
    // works, and the per-taskbar overlays reuse it for the primary taskbar.
    fn create_overlay_window() -> Result<isize, String> {
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
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        Ok(hwnd.0 as isize)
    }

    // Startup hook: register the overlay window class. The overlay windows
    // themselves are created lazily per taskbar in apply().
    pub fn create(_app: &AppHandle) -> Result<isize, String> {
        register_class();
        Ok(0)
    }

    pub fn shutdown() {
        stop_tick_thread();
        stop_mouse_hook();
        stop_elevation_win_event();
    }

    // Maps a user-facing opacity (0..1) to an 8-bit alpha value with a 1:1
    // top half: 100%→255, 99%→254, …, 50%→205, every integer percentage
    // from 50 to 100 maps to its own distinct alpha level, so no two slider
    // positions repeat a value. Below 50% there are more percentages than
    // remaining alpha levels (0-50% → 0-205), so the curve skips levels to
    // stay strictly increasing without repeats.
    fn opacity_to_alpha(opacity: f64) -> f64 {
        let pct = (opacity.clamp(0.0, 1.0) * 100.0).clamp(0.0, 100.0);
        if pct >= 50.0 {
            255.0 - (100.0 - pct)
        } else {
            pct / 50.0 * 205.0
        }
    }

    // Detection Method is global, not per monitor: the mouse hook either
    // installs or it doesn't, and the tick cadence is a single value. This
    // syncs the runtime statics from the global config (called on startup
    // and whenever the Settings-tab control changes). Event mode uses the
    // mouse hook as the hover detector and stands the cursor poll down;
    // poll mode uses the cursor poll and no hook. The Debug tab can still
    // override either live (set_detection), a later mode/interval change
    // just re-syncs these defaults.
    pub fn sync_detection(mode: &str, poll_ms: u32) {
        let event = mode == "event";
        let poll_ms = poll_ms.clamp(16, 200);
        PREFER_EVENT_HOVER.store(event, Ordering::Relaxed);
        POLL_INTERVAL_MS.store(poll_ms, Ordering::Relaxed);
        CURSOR_POLLING.store(!event, Ordering::Relaxed);
        MOUSE_HOOK_TOGGLE.store(event, Ordering::Relaxed);
        // Wake the tick thread so a poll-interval change takes effect
        // immediately instead of waiting out the previous cadence.
        let guard = TICK_WAKE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        TICK_INTERVAL_MS.store(poll_ms, Ordering::Relaxed);
        TICK_WAKE_CONDVAR.notify_all();
        drop(guard);
    }

    pub fn apply(immediate: bool) -> Result<(), String> {
        // Same transient-miss retry as before: right after a tray restart
        // the taskbars can be missing, and the frontend would otherwise treat
        // the toggle as off and never persist the setting. Absorb the hiccup.
        let taskbars = taskbars_retry(3, Duration::from_millis(100));
        if taskbars.is_empty() {
            return Err("Windows taskbar was not found".to_string());
        }
        // Ensure one overlay per taskbar, then apply each with its monitor's
        // profile. Overlays for taskbars that disappeared are dropped.
        let mut to_apply: Vec<(HWND, HWND, RECT)> = Vec::new();
        {
            let mut overlays = overlays_map().lock().unwrap_or_else(|e| e.into_inner());
            for (taskbar, rect) in &taskbars {
                let existing = overlays
                    .iter()
                    .find(|(_, info)| info.taskbar == taskbar.0 as isize)
                    .map(|(h, _)| *h);
                let hwnd = match existing {
                    Some(h) => {
                        if let Some(info) = overlays.get_mut(&h) {
                            // monitor_key_for_taskbar enumerates the whole
                            // monitor table (Win32 + CCD + SetupAPI), which is
                            // far too expensive to run on every slider tick
                            // (apply runs once per input event). Resolve it
                            // only when the overlay is first created or its
                            // taskbar moved to another monitor (rect change);
                            // a slider drag keeps the same rect, so the
                            // stored key stays valid and the hot path skips
                            // the enumeration entirely.
                            let moved = info.rect != *rect;
                            info.rect = *rect;
                            if moved {
                                info.monitor_key = monitor_key_for_taskbar(*taskbar, *rect);
                            }
                        }
                        h
                    }
                    None => {
                        let raw = create_overlay_window()?;
                        fades_map()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(raw, new_fade_state());
                        let key = monitor_key_for_taskbar(*taskbar, *rect);
                        overlays.insert(
                            raw,
                            OverlayInfo {
                                taskbar: taskbar.0 as isize,
                                rect: *rect,
                                monitor_key: key,
                            },
                        );
                        raw
                    }
                };
                to_apply.push((HWND(hwnd as *mut c_void), *taskbar, *rect));
            }
            overlays.retain(|h, _| to_apply.iter().any(|(hw, _, _)| hw.0 as isize == *h));
        }
        let mut any_enabled = false;
        for (hwnd, taskbar, rect) in to_apply {
            any_enabled |= apply_overlay(hwnd, taskbar, rect, immediate)?;
        }
        // Thread lifecycle is global, not per overlay: the tick, mouse hook,
        // and elevation WinEvent threads are shared across every overlay, so
        // start them when any overlay is on and stop them only when all are
        // off. Deciding this once here (instead of inside apply_overlay)
        // keeps a mixed-state apply from starting and stopping them
        // repeatedly.
        if any_enabled {
            ensure_tick_thread();
            if PREFER_EVENT_HOVER.load(Ordering::Relaxed) {
                ensure_elevation_win_event();
                ensure_mouse_hook();
            }
        } else {
            stop_tick_thread();
            stop_mouse_hook();
            stop_elevation_win_event();
        }
        Ok(())
    }

    // Apply the dimmer state to ONE overlay, reading that overlay's monitor
    // profile from PROFILES. The old single-overlay apply body, generalized.
    fn apply_overlay(
        hwnd: HWND,
        taskbar: HWND,
        rect: RECT,
        immediate: bool,
    ) -> Result<bool, String> {
        let hwnd_raw = hwnd.0 as isize;
        // This overlay's per-monitor profile (opacity, fades, and the
        // per-monitor on/off state).
        let profile = {
            let overlays = overlays_map().lock().unwrap_or_else(|e| e.into_inner());
            let key = overlays.get(&hwnd_raw).map(|i| i.monitor_key.clone());
            key.as_deref()
                .and_then(|k| {
                    super::profiles_map()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(k)
                        .cloned()
                })
                .unwrap_or_default()
        };
        let enabled = profile.enabled;
        let alpha = opacity_to_alpha(profile.opacity);
        let hovered = enabled && cursor_over_rect(rect, false).0;

        let mut immediate_alpha = None;
        let fades = fades_map();
        {
            let mut fades_guard = fades.lock().unwrap_or_else(|e| e.into_inner());
            let fade = fades_guard
                .get_mut(&hwnd_raw)
                .ok_or_else(|| "overlay fade state missing".to_string())?;
            fade.enabled = enabled;
            fade.base_alpha = alpha;
            // Detection method is global (set in sync_detection), not per
            // monitor, so every overlay shares the same hover mode.
            fade.hover_event = PREFER_EVENT_HOVER.load(Ordering::Relaxed);
            // Seed the overlay's hover cache so it doesn't fire a spurious
            // poke for a state that predates the hook's installation.
            fade.hover = hovered;
            fade.fade_in_duration_ms = profile
                .fade_in_duration_ms
                .clamp(MIN_EASE_DURATION_MS, MAX_EASE_DURATION_MS);
            fade.fade_out_duration_ms = profile
                .fade_out_duration_ms
                .clamp(MIN_EASE_DURATION_MS, MAX_EASE_DURATION_MS);
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
                fade.cached_taskbar = Some((taskbar.0 as isize, rect));
                fade.last_taskbar_cache_refresh = Instant::now();
            }
        } // Thread lifecycle (tick, hooks) is managed once per apply() call
          // based on whether any overlay is on, since the threads are global.

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

            // Only reposition / re-show when the overlay isn't already
            // correctly on screen. During a slider drag apply() runs once per
            // input tick and only the alpha changed; re-inserting the window
            // at the front of the topmost band and re-showing it every tick
            // makes the layered window flicker (each z-order touch triggers a
            // DWM re-composite). The periodic z-order repair still handles
            // structural changes (taskbar geometry, flyout, menus), and the
            // !enabled path below still force-hides.
            let mut already_placed = false;
            if enabled {
                let visible = IsWindowVisible(hwnd).as_bool();
                let mut current = RECT::default();
                let geometry_ok = GetWindowRect(hwnd, &mut current).is_ok();
                already_placed = visible
                    && geometry_ok
                    && current.left == rect.left
                    && current.top == rect.top
                    && current.right == rect.right
                    && current.bottom == rect.bottom;
            }
            if !already_placed {
                // Context menus no longer change overlay visibility. They only
                // pause the repair path, so enabling the dimmer while a menu
                // is already open still shows the overlay normally.
                let visibility = if enabled {
                    SWP_SHOWWINDOW
                } else {
                    SWP_HIDEWINDOW
                };
                // Same flyout-priority reasoning as maintain_overlay_zorder:
                // don't blindly claim the front of the topmost band if the
                // mode-change flyout is currently showing. An active native
                // menu is handled specially: apply() may be called while a
                // menu is open, so keep the move/size/show operation but
                // explicitly preserve its current z-order and let the
                // menu-safe repair path handle ordering.
                let popup_active = MENU_POPUP_OPEN.load(Ordering::Relaxed);
                let insert_after = active_flyout_window().unwrap_or(HWND_TOPMOST);
                let zorder_flags = if popup_active {
                    SWP_NOZORDER
                } else {
                    SET_WINDOW_POS_FLAGS(0)
                };
                SetWindowPos(
                    hwnd,
                    Some(insert_after),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    visibility | zorder_flags | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
                )
                .map_err(|e| format!("could not position taskbar overlay: {e}"))?;
                if enabled {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                } else {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
            }
        }
        // Apply can run while a native menu is already open. Run the shared
        // repair once after positioning so an overlapping #32768 popup is
        // promoted above the overlay immediately, instead of waiting for the
        // next tick or requiring another input event.
        maintain_overlay_zorder(hwnd, Some((taskbar, rect)));
        if enabled {
            update_fade(hwnd);
        }
        Ok(enabled)
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    pub fn available() -> bool {
        false
    }
    pub fn set_verbose_logging(_enabled: bool) {}
    pub fn set_detection(_name: &str, _enabled: bool) {}
    pub fn sync_detection(_mode: &str, _poll_ms: u32) {}
    pub fn set_suspended(_enabled: bool) {}
    pub fn set_flyout_hwnd(_raw: isize) {}
    pub fn on_display_change() {}
    pub fn fullscreen_active_on_monitor(_hmonitor: u32) -> bool {
        false
    }
    pub fn create(_app: &AppHandle) -> Result<isize, String> {
        Err("Taskbar dimmer is only available on Windows".into())
    }
    pub fn apply(_immediate: bool) -> Result<(), String> {
        Err("Taskbar dimmer is only available on Windows".into())
    }
}

// The per-monitor profile map lives at crate level so the Tauri commands and
// the platform module share it. Key = the monitor's profile identity
// (EDID id, disambiguated for identical panels), resolved the same way
// monitor_key_for_taskbar does.
fn resolve_profile_key(monitor_key: Option<&str>) -> String {
    if let Some(key) = monitor_key {
        let key = key.trim();
        if !key.is_empty() {
            // Callers (the main window and the tray) send the monitor's
            // current profile identity; pass it through. Topology re-keys
            // are handled by rekey_profiles on display changes, so the hot
            // slider paths never pay for a monitor enumeration.
            return key.to_string();
        }
    }
    // No key supplied (e.g. the tray menu, which has no monitor selector):
    // fall back to the primary monitor's profile identity so the tray's
    // global controls keep operating on the primary taskbar.
    for m in crate::monitors::all_monitors() {
        if m.primary {
            return m.profile_id;
        }
    }
    String::new()
}

// Re-key the shared profile map to the current monitor identities after a
// display-topology change (see monitors::rekey_profile_map).
pub(crate) fn rekey_profiles() {
    if let Ok(mut profiles) = profiles_map().lock() {
        crate::monitors::rekey_profile_map(&mut profiles);
    }
}

#[tauri::command]
pub fn set_taskbar_dimmer(
    monitor_key: Option<String>,
    enabled: bool,
    opacity: f64,
    fade_in_duration_ms: u64,
    fade_out_duration_ms: u64,
    immediate: bool,
) -> Result<(), String> {
    let fade_in_duration_ms = fade_in_duration_ms.clamp(0, 1000);
    let fade_out_duration_ms = fade_out_duration_ms.clamp(0, 1000);
    let key = resolve_profile_key(monitor_key.as_deref());
    if key.is_empty() {
        return Err("Could not resolve the target monitor".to_string());
    }
    {
        let mut profiles = profiles_map().lock().unwrap_or_else(|e| e.into_inner());
        let entry = profiles.entry(key).or_default();
        // Enabled is per monitor: this toggle turns on/off the target
        // monitor's own overlay, not every taskbar.
        entry.enabled = enabled;
        entry.opacity = opacity.clamp(0.0, 1.0);
        entry.fade_in_duration_ms = fade_in_duration_ms;
        entry.fade_out_duration_ms = fade_out_duration_ms;
    }
    platform::apply(immediate)
}

// Global convenience toggle: turn every monitor's taskbar dimmer on or off
// at once, while keeping each monitor's opacity/fade settings intact. Used by
// the nav switch and the hotkey, so the app keeps a one-switch-all way to
// silence the dimmers even though the per-monitor on/off is now per display.
#[tauri::command]
pub fn set_taskbar_dimmer_enabled_for_all(enabled: bool, immediate: bool) -> Result<(), String> {
    let connected: Vec<String> = {
        let mut profiles = profiles_map().lock().unwrap_or_else(|e| e.into_inner());
        // Toggle the persisted profiles for current monitors (and guard
        // against an empty map by seeding nothing; apply() will still work
        // off the overlays' resolved keys next time a real edit happens).
        let keys: Vec<String> = profiles.keys().cloned().collect();
        if keys.is_empty() {
            // No profile entries yet, so seed one per connected monitor from
            // the defaults so the per-monitor enable actually sticks.
            let monitors = crate::monitors::all_monitors();
            for m in &monitors {
                profiles.entry(m.profile_id.clone()).or_default().enabled = enabled;
            }
            monitors.into_iter().map(|m| m.profile_id).collect()
        } else {
            for key in &keys {
                if let Some(entry) = profiles.get_mut(key) {
                    entry.enabled = enabled;
                }
            }
            keys
        }
    };
    let _ = connected;
    platform::apply(immediate)
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

// Startup seed for the global Detection Method setting (hover mode + poll
// interval), called from lib.rs after the persisted config is loaded so the
// hook and tick cadence match before any command re-applies them.
pub(crate) fn sync_detection(mode: &str, poll_ms: u32) {
    platform::sync_detection(mode, poll_ms);
}

#[tauri::command]
pub fn set_taskbar_dimmer_hover_mode(mode: String) -> Result<(), String> {
    // Detection Method is global: keep the current poll interval, swap the
    // mode, then apply so the overlay/tick behavior follows immediately.
    platform::sync_detection(
        &mode,
        platform::POLL_INTERVAL_MS.load(std::sync::atomic::Ordering::Relaxed),
    );
    platform::apply(false)
}

#[tauri::command]
pub fn set_taskbar_dimmer_poll_interval(ms: u32) -> Result<(), String> {
    // UI bounds: the frontend slider clamps 16..200, so keep the backend
    // clamp identical instead of allowing a hidden 2000 ms ceiling.
    let ms = ms.clamp(16, 200);
    // Keep the current detection mode, swap the interval, then apply so the
    // tick cadence changes immediately.
    let mode = if platform::PREFER_EVENT_HOVER.load(std::sync::atomic::Ordering::Relaxed) {
        "event"
    } else {
        "poll"
    };
    platform::sync_detection(mode, ms);
    platform::apply(false)
}

#[tauri::command]
pub fn get_taskbar_dimmer_status(
    state: State<TaskbarDimmerState>,
    monitor_key: Option<String>,
) -> TaskbarDimmerStatus {
    let key = resolve_profile_key(monitor_key.as_deref());
    let profile = profiles_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
        .unwrap_or_default();
    TaskbarDimmerStatus {
        enabled: profile.enabled,
        opacity: profile.opacity,
        fade_in_duration_ms: profile.fade_in_duration_ms,
        fade_out_duration_ms: profile.fade_out_duration_ms,
        // Detection method is global, not per monitor.
        hover_detection: if platform::PREFER_EVENT_HOVER.load(std::sync::atomic::Ordering::Relaxed)
        {
            "event".to_string()
        } else {
            "poll".to_string()
        },
        poll_interval_ms: platform::POLL_INTERVAL_MS.load(std::sync::atomic::Ordering::Relaxed),
        available: platform::available()
            && state
                .overlay
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some(),
    }
}

pub fn create_overlay(app: &AppHandle, state: &TaskbarDimmerState) -> Result<(), String> {
    let hwnd = platform::create(app)?;
    *state.overlay.lock().unwrap_or_else(|e| e.into_inner()) = Some(hwnd);
    Ok(())
}

pub fn shutdown() {
    platform::shutdown();
}

// Called by flyout_window.rs right after the flyout is shown (with its HWND)
// and right before/after it is hidden (with 0), so the dimmer overlay's
// z-order repair never fights the flyout for the front of the topmost band.
pub fn set_flyout_hwnd(raw: isize) {
    platform::set_flyout_hwnd(raw);
}

// Called by lib.rs's display-change notification so a resolution/DPI change
// repositions the overlay immediately instead of waiting out the periodic
// taskbar-cache cadence.
pub fn on_display_change() {
    platform::on_display_change();
}

pub(crate) fn fullscreen_active_on_monitor(hmonitor: u32) -> bool {
    platform::fullscreen_active_on_monitor(hmonitor)
}
