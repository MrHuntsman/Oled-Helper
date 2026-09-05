// input_monitor.rs, real keyboard/mouse input tracking for the Screen Dimmer.
//
// GetLastInputInfo is reset by gamepad/HID reports on some systems, so a
// connected-but-idle controller makes Windows report continuous "input" and
// the dimmer never fires. A WH_KEYBOARD_LL + WH_MOUSE_LL hook pair only
// receives genuine keyboard and mouse events, so the idle clock ignores
// controller chatter. The hooks are only installed while the Screen Dimmer is
// enabled; each event is a single atomic store, so the overhead is negligible.
//
// The hooks have one blind spot: Windows filters low-level hook events by
// integrity level (UIPI), so once an elevated window (an admin app, a UAC
// prompt, an elevated game) takes the foreground, a non-elevated process
// stops receiving them entirely and this clock would freeze. While such a
// window is focused the clock falls back to GetLastInputInfo, which the
// system maintains itself and which is not subject to that filter, so the
// dimmer neither fires mid-use nor sticks once dimmed. The fallback is gated
// on an explicit foreground-elevation check rather than on GetLastInputInfo
// being fresher than the hook clock, because HID chatter keeps it
// permanently fresh on some systems (the reason the hooks exist); whenever
// the hooks can see input they stay authoritative.

#![allow(non_snake_case)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, MSG, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_QUIT,
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetTickCount64() -> u64;
}

// Tick count of the most recent genuine keyboard/mouse event seen by a hook.
static LAST_KB_MOUSE_INPUT: AtomicU32 = AtomicU32::new(0);
// True once both hooks are installed and running.
static HOOKS_ACTIVE: AtomicBool = AtomicBool::new(false);

// How long the hook clock must be silent before the foreground window is
// examined for elevation. While the user is actively typing in a normal
// window the hooks fire constantly and the check is never reached.
const HOOK_SILENCE_FALLBACK_MS: u32 = 1000;
// Throttle for the foreground-elevation token query (OpenProcess +
// GetTokenInformation). The watcher reads the idle clock every 16 ms tick;
// the check itself only reruns at this cadence.
const ELEVATION_RECHECK_MS: u32 = 300;

// Cached verdict that the current foreground window is elevated above this
// process, the condition under which the hooks are filtered and the clock
// falls back to GetLastInputInfo.
static FOREGROUND_ELEVATED: AtomicBool = AtomicBool::new(false);
// Tick count of the most recent foreground-elevation check.
static LAST_ELEVATION_CHECK: AtomicU32 = AtomicU32::new(0);
// Whether this process itself runs elevated. An elevated process's hooks are
// not filtered by UIPI, so the GetLastInputInfo fallback is never needed
// (and would reintroduce HID chatter on the systems affected by it). Cached:
// elevation cannot change during a process's lifetime.
static SELF_ELEVATED: OnceLock<bool> = OnceLock::new();

static HOOK_THREAD: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

// One procedure serves both hooks: they share the same (code, wparam, lparam)
// signature. Every delivered event is genuine keyboard/mouse activity.
unsafe extern "system" fn input_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 {
        LAST_KB_MOUSE_INPUT.store(GetTickCount64() as u32, Ordering::Release);
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn hook_thread_loop() {
    HOOK_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    // Seed the clock so idle starts counting from the moment the hooks go live
    // rather than from system boot (the fallback value of zero).
    LAST_KB_MOUSE_INPUT.store(unsafe { GetTickCount64() } as u32, Ordering::Release);

    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(input_hook_proc), None, 0) };
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(input_hook_proc), None, 0) };
    match (keyboard, mouse) {
        (Ok(keyboard), Ok(mouse)) => {
            HOOKS_ACTIVE.store(true, Ordering::Release);
            let mut msg = MSG::default();
            unsafe {
                // Low-level hooks only fire while the installing thread pumps
                // messages; WM_QUIT ends the loop on shutdown.
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
            }
            let _ = unsafe { UnhookWindowsHookEx(keyboard) };
            let _ = unsafe { UnhookWindowsHookEx(mouse) };
        }
        _ => {
            log::warn!("[input-monitor] keyboard/mouse hook install failed");
        }
    }
    HOOKS_ACTIVE.store(false, Ordering::Release);
    HOOK_THREAD_ID.store(0, Ordering::Relaxed);
}

pub fn start() {
    let threads = HOOK_THREAD.get_or_init(|| Mutex::new(None));
    let mut guard = match threads.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_some() {
        return;
    }
    match std::thread::Builder::new()
        .name("screen-dimmer-input-hook".into())
        .spawn(hook_thread_loop)
    {
        Ok(handle) => *guard = Some(handle),
        Err(error) => log::error!("[input-monitor] could not start hook thread: {error}"),
    }
}

pub fn stop() {
    let thread_exists = HOOK_THREAD
        .get()
        .map(|threads| {
            threads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
        })
        .unwrap_or(false);
    if !thread_exists {
        HOOKS_ACTIVE.store(false, Ordering::Release);
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    let mut id = HOOK_THREAD_ID.load(Ordering::Relaxed);
    while id == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));
        id = HOOK_THREAD_ID.load(Ordering::Relaxed);
    }
    if id != 0 {
        let _ = unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
    if let Some(threads) = HOOK_THREAD.get() {
        if let Ok(mut guard) = threads.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
    HOOKS_ACTIVE.store(false, Ordering::Release);
}

/// System-wide last-input tick from GetLastInputInfo, the fallback clock.
fn system_last_input_tick() -> u32 {
    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut lii).as_bool() {
            lii.dwTime
        } else {
            0
        }
    }
}

// Whether this process runs with an elevated token (cached, see SELF_ELEVATED).
fn self_elevated() -> bool {
    *SELF_ELEVATED.get_or_init(crate::process::process_is_elevated)
}

// Throttled foreground-elevation read (the token query itself lives in
// process.rs, shared with the other UIPI-aware input paths). While the
// verdict flips back from elevated to normal, the hook clock is re-seeded
// from GetLastInputInfo: the hooks slept through the whole elevated session,
// so without the seed the stale clock would count it all as idle and could
// fire the dim the moment the user returns to a normal window.
fn elevated_foreground_now(now: u32) -> bool {
    if now.wrapping_sub(LAST_ELEVATION_CHECK.load(Ordering::Acquire)) < ELEVATION_RECHECK_MS {
        return FOREGROUND_ELEVATED.load(Ordering::Acquire);
    }
    LAST_ELEVATION_CHECK.store(now, Ordering::Release);
    let elevated = crate::process::foreground_process_is_elevated();
    let was_elevated = FOREGROUND_ELEVATED.swap(elevated, Ordering::AcqRel);
    if was_elevated && !elevated {
        LAST_KB_MOUSE_INPUT.store(system_last_input_tick(), Ordering::Release);
    }
    elevated
}

/// Last genuine keyboard/mouse input tick. Falls back to GetLastInputInfo
/// when the hooks are not installed (dimmer disabled, or install failed), and
/// while an elevated foreground window is filtering the hooks (see the
/// module comment): after the hook clock has been silent for a second the
/// foreground is examined (throttled) and the system clock is used only when
/// it is elevated, so HID chatter that keeps GetLastInputInfo fresh can
/// never stall the dim while the hooks can see input.
pub fn last_kb_mouse_input_ms() -> u32 {
    if !HOOKS_ACTIVE.load(Ordering::Acquire) {
        return system_last_input_tick();
    }
    let hook_tick = LAST_KB_MOUSE_INPUT.load(Ordering::Acquire);
    let now = unsafe { GetTickCount64() } as u32;
    if now.wrapping_sub(hook_tick) >= HOOK_SILENCE_FALLBACK_MS
        && !self_elevated()
        && elevated_foreground_now(now)
    {
        return system_last_input_tick();
    }
    hook_tick
}
