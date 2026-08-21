// input_monitor.rs, real keyboard/mouse input tracking for the Screen Dimmer.
//
// GetLastInputInfo is reset by gamepad/HID reports on some systems, so a
// connected-but-idle controller makes Windows report continuous "input" and
// the dimmer never fires. A WH_KEYBOARD_LL + WH_MOUSE_LL hook pair only
// receives genuine keyboard and mouse events, so the idle clock ignores
// controller chatter. The hooks are only installed while the Screen Dimmer is
// enabled; each event is a single atomic store, so the overhead is negligible.

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

/// Last genuine keyboard/mouse input tick, falling back to GetLastInputInfo
/// when the hooks are not installed (dimmer disabled, or install failed).
pub fn last_kb_mouse_input_ms() -> u32 {
    if HOOKS_ACTIVE.load(Ordering::Acquire) {
        return LAST_KB_MOUSE_INPUT.load(Ordering::Acquire);
    }
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
