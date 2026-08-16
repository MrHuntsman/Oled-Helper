//! explorer-hook.dll — loaded into the shell and the classic Control Panel
//! host processes via SetWindowsHookExW to override their font antialiasing.
//! The Windows shell (and several system consoles) hard-code ClearType and
//! ignore the per-user font-smoothing registry values, so the only way to
//! change their text is to hook font creation and rewrite the quality flag.
//!
//! The app (OLED Helper) controls this via a session-local shared-memory
//! mode: 0 = pass through untouched, 1 = force grayscale
//! (ANTIALIASED_QUALITY), 2 = force no antialiasing / crisp
//! (NONANTIALIASED_QUALITY). Only explicit ClearType requests are rewritten
//! — fonts that already asked for grayscale or non-antialiased rendering
//! pass through untouched, and in crisp mode explicit grayscale requests are
//! converted too (smoothing off means no smoothing at all). Both
//! CreateFontIndirectW and CreateFontIndirectExW are hooked. Fonts that ask
//! for DEFAULT_QUALITY are intentionally left alone: they follow the system
//! smoothing state, which the app already sets per mode. The mode is
//! re-read on every font creation call, so the app can flip it live without
//! unloading the DLL.
//!
//! Crash safety (explorer.exe restarts itself if we crash it):
//! 1. The inline patch is applied while every OTHER thread of the process is
//!    suspended, so no thread can execute the function entry while the jump
//!    bytes are half-written (a plain copy_from_slice over the entry is not
//!    atomic and explorer creates fonts from many threads at once). Same for
//!    restoring the original bytes on DLL_PROCESS_DETACH — re-enabling after
//!    a toggle-off must find a pristine function, not a double patch.
//! 2. The detour is installed lazily from the first hook callback, NOT in
//!    DllMain — keeping DllMain free of VirtualAlloc / disassembly work that
//!    runs under the loader lock.
//! 3. The hook functions never return a NULL HFONT: before the detour is
//!    stored they call the original function pointer directly, so a font
//!    creation can never fail spuriously during the install window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use retour::GenericDetour;
use windows::core::{s, w, BOOL, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, CLEARTYPE_NATURAL_QUALITY, CLEARTYPE_QUALITY, ENUMLOGFONTEXDVW, FONT_QUALITY,
    HFONT, LOGFONTW, NONANTIALIASED_QUALITY,
};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleW, GetProcAddress,
};
use windows::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE};
use windows::Win32::System::SystemServices::{
    DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, DLL_THREAD_ATTACH, DLL_THREAD_DETACH,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
};
use windows::Win32::UI::WindowsAndMessaging::CallNextHookEx;

// Shared memory layout (12 bytes): the app writes the mode at offset 0; the
// DLL writes its own state back at offsets 4 and 8 so the app can verify the
// hook is really installed in the CURRENT explorer.exe instead of trusting
// that the module merely loaded.
const STATE_MODE: usize = 0;
const STATE_STATUS: usize = 4;
const STATE_PID: usize = 8;
const STATE_SIZE: usize = 12;
// Status bits written by the DLL once the detour is actually installed.
const STATUS_HOOK_W_INSTALLED: u32 = 1;
const STATUS_HOOK_EX_INSTALLED: u32 = 2;

type CreateFontIndirectWFn = unsafe extern "system" fn(*const LOGFONTW) -> HFONT;
type CreateFontIndirectExWFn = unsafe extern "system" fn(*const ENUMLOGFONTEXDVW) -> HFONT;

// Shared mode (0 = passthrough, 1 = grayscale, 2 = crisp) + the installed
// detours. Both are set once and live for the process lifetime. STATE stores
// the mapped view's raw pointer as a usize (raw pointers are not Send/Sync,
// so we carry the address in an integer type instead). ORIGINAL_* keep the
// pristine entry points so the hooks can fall back to a direct call before
// the detour is stored (never return NULL HFONT).
static STATE: OnceLock<usize> = OnceLock::new();
static DETOUR: OnceLock<GenericDetour<CreateFontIndirectWFn>> = OnceLock::new();
static DETOUR_EX: OnceLock<GenericDetour<CreateFontIndirectExWFn>> = OnceLock::new();
static ORIGINAL_W: OnceLock<CreateFontIndirectWFn> = OnceLock::new();
static ORIGINAL_EX: OnceLock<CreateFontIndirectExWFn> = OnceLock::new();
// True once the deferred detour install has been attempted; reset on failure
// so the next hook callback retries (the callback fires on every message, so
// a transient failure self-heals).
static INSTALL_ATTEMPTED: AtomicBool = AtomicBool::new(false);
// Cached in DllMain so the hook callback doesn't re-query the process image
// path on every window-creation event.
static IS_TARGET: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn hook_create_font_indirect_w(lplf: *const LOGFONTW) -> HFONT {
    match DETOUR.get() {
        Some(detour) => {
            if lplf.is_null() {
                return detour.call(lplf);
            }
            let mode = state_mode();
            let mut lf = *lplf;
            if let Some(quality) = rewritten_quality(mode, lf.lfQuality) {
                lf.lfQuality = quality;
            }
            detour.call(&lf)
        }
        // Detour not stored yet (install window): call the real function
        // directly instead of failing the font creation.
        None => ORIGINAL_W.get().map(|orig| orig(lplf)).unwrap_or_default(),
    }
}

// CreateFontIndirectExW takes an ENUMLOGFONTEXDVW whose first member is the
// LOGFONT, so the requested quality sits at the same offset as in
// CreateFontIndirectW. A few shell font paths use this variant, so without
// this detour they would keep ClearType fringing.
unsafe extern "system" fn hook_create_font_indirect_ex_w(lplf: *const ENUMLOGFONTEXDVW) -> HFONT {
    match DETOUR_EX.get() {
        Some(detour) => {
            if lplf.is_null() {
                return detour.call(lplf);
            }
            let mode = state_mode();
            let quality = (*(lplf as *const LOGFONTW)).lfQuality;
            let Some(rewritten) = rewritten_quality(mode, quality) else {
                return detour.call(lplf);
            };
            // Copy the (large) struct so the caller's is never mutated, flip
            // the embedded quality, and call the original with the copy.
            let mut copy = *lplf;
            copy.elfEnumLogfontEx.elfLogFont.lfQuality = rewritten;
            detour.call(&copy)
        }
        None => ORIGINAL_EX.get().map(|orig| orig(lplf)).unwrap_or_default(),
    }
}

// Map a requested quality to the forced one for the current mode, or None to
// pass through. Only explicit ClearType requests are rewritten (plus
// explicit grayscale in crisp mode). Fonts that asked for DEFAULT_QUALITY or
// non-antialiased rendering keep their intent — DEFAULT_QUALITY fonts follow
// the system smoothing state, which the app already sets per mode.
fn rewritten_quality(mode: u32, quality: FONT_QUALITY) -> Option<FONT_QUALITY> {
    if mode == 0 {
        return None;
    }
    let is_cleartype =
        quality == CLEARTYPE_QUALITY || u32::from(quality.0) == CLEARTYPE_NATURAL_QUALITY;
    // Crisp mode also converts explicit grayscale requests — smoothing off
    // means no smoothing at all — but leaves non-antialiased fonts alone.
    let needs_rewrite = is_cleartype || (mode == 2 && quality == ANTIALIASED_QUALITY);
    if needs_rewrite {
        Some(if mode == 2 {
            NONANTIALIASED_QUALITY
        } else {
            ANTIALIASED_QUALITY
        })
    } else {
        None
    }
}

fn state_mode() -> u32 {
    // SAFETY: STATE is written once in map_state; the mapped view stays valid
    // for the lifetime of the process.
    unsafe {
        STATE.get()
            .map(|addr| *((addr + STATE_MODE) as *const u32))
            .unwrap_or(0)
    }
}

// Write this process's PID and hook status back into the shared memory so the
// app can verify the hook is really live in the CURRENT explorer.exe
// generation instead of trusting that the module merely loaded.
fn report_state(status: u32) {
    let Some(addr) = STATE.get() else {
        return;
    };
    // SAFETY: the mapped view is valid for the process lifetime; both fields
    // are plain u32s at fixed offsets we own.
    unsafe {
        let pid = std::process::id();
        std::ptr::write_volatile((addr + STATE_PID) as *mut u32, pid);
        std::ptr::write_volatile((addr + STATE_STATUS) as *mut u32, status);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// The DLL is loaded into more than just explorer.exe: the app also
// LoadLibraryWs it to resolve the hook procedure before calling
// SetWindowsHookExW. Only the targeted shell/Control Panel processes need the
// font detour, so everything is gated on the host process being one of them.
fn process_name() -> Option<String> {
    // GetModuleFileNameW(None, ...) returns the image path of the process the
    // DLL is loaded in, which is exactly what we need to distinguish the
    // targeted hosts from every other process.
    unsafe {
        let mut buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut buf);
        if len == 0 || len as usize >= buf.len() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn process_base_name() -> String {
    process_name()
        .map(|name| {
            name.rsplit(['\\', '/'])
                .next()
                .unwrap_or("")
                .to_string()
        })
        .unwrap_or_default()
}

// The primary host: only Explorer writes its status report back into the
// shared memory (single slot), so the app verifies the primary target from it.
fn is_explorer() -> bool {
    process_base_name().eq_ignore_ascii_case("explorer.exe")
}

// Everything the app wants the font detour in: Explorer plus the classic
// Control Panel host processes and the MMC admin consoles. SystemProperties is
// a family of executables, so it is matched by prefix.
fn is_target() -> bool {
    let name = process_base_name();
    let lower = name.to_ascii_lowercase();
    lower == "explorer.exe"
        || lower == "control.exe"
        || lower == "rundll32.exe"
        || lower == "mmc.exe"
        || (lower.starts_with("systemproperties") && lower.ends_with(".exe"))
}

fn debug(msg: &str) {
    let w = wide(msg);
    unsafe { OutputDebugStringW(PCWSTR(w.as_ptr())) };
}

fn map_state() {
    // SAFETY: called once at DLL_PROCESS_ATTACH.
    unsafe {
        let name = wide(r"Local\OledHelperExplorerHookState");
        let Ok(mapping) = OpenFileMappingW(FILE_MAP_READ.0 | FILE_MAP_WRITE.0, false, PCWSTR(name.as_ptr()))
        else {
            return;
        };
        let view = MapViewOfFile(mapping, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, STATE_SIZE as usize);
        if !view.Value.is_null() {
            let _ = STATE.set(view.Value as usize);
        }
        let _ = CloseHandle(mapping);
    }
}

// ── Safe patching ──────────────────────────────────────────────────────
// The inline patch (retour) overwrites the function entry with a multi-byte
// jump that is NOT written atomically. CreateFontIndirectW is a hot function
// called from many explorer threads at once, so a thread executing the entry
// mid-write would run a half-written instruction and crash the shell (which
// Windows then auto-restarts). Suspending every other thread of the process
// while the bytes are written closes that window — the same technique
// MinHook uses. Best effort: threads we fail to open/suspend are left alone
// (rare; a tiny race window is far better than a crash, and the hook's
// direct-call fallback keeps font creation safe in between).

struct SuspendedThread(HANDLE);

impl Drop for SuspendedThread {
    fn drop(&mut self) {
        // SAFETY: handle came from OpenThread + SuspendThread above.
        unsafe {
            let _ = ResumeThread(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

fn suspend_other_threads() -> Vec<SuspendedThread> {
    let pid = std::process::id();
    let current = unsafe { GetCurrentThreadId() };
    let mut suspended = Vec::new();
    // SAFETY: CreateToolhelp32Snapshot with TH32CS_SNAPTHREAD + Thread32
    // enumeration are well-formed calls; the snapshot handle is closed below.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return suspended;
        };
        let mut entry = THREADENTRY32::default();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        if Thread32First(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32OwnerProcessID == pid && entry.th32ThreadID != current {
                    if let Ok(handle) = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) {
                        // SuspendThread returns (u32)-1 when the thread is
                        // already suspended or dying; skip those, keep the rest.
                        if SuspendThread(handle) != u32::MAX {
                            suspended.push(SuspendedThread(handle));
                        } else {
                            let _ = CloseHandle(handle);
                        }
                    }
                }
                if Thread32Next(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    suspended
}

fn find_create_font_indirect_w() -> Option<CreateFontIndirectWFn> {
    // SAFETY: reads export tables; both calls are well-formed.
    unsafe {
        let module = GetModuleHandleW(w!("gdi32.dll")).ok()?;
        let proc = GetProcAddress(module, s!("CreateFontIndirectW"))?;
        Some(std::mem::transmute(proc))
    }
}

fn find_create_font_indirect_ex_w() -> Option<CreateFontIndirectExWFn> {
    // SAFETY: reads export tables; both calls are well-formed.
    unsafe {
        let module = GetModuleHandleW(w!("gdi32.dll")).ok()?;
        let proc = GetProcAddress(module, s!("CreateFontIndirectExW"))?;
        Some(std::mem::transmute(proc))
    }
}

// Install both detours with every other thread of the process suspended, then
// report what actually got installed so the app can tell "hooked" from "DLL
// loaded but detour failed". Returns whether the primary (W) detour is live.
fn install_detour() -> bool {
    let suspended = suspend_other_threads();
    let mut status = 0;
    if install_font_detour() {
        status |= STATUS_HOOK_W_INSTALLED;
    }
    if install_font_ex_detour() {
        status |= STATUS_HOOK_EX_INSTALLED;
    }
    // Resume all suspended threads before touching shared memory.
    drop(suspended);
    // Only Explorer reports its pid+status back (the shared memory has a single
    // slot); every target still installs the detour and reads the mode.
    if is_explorer() {
        report_state(status);
    }
    status & STATUS_HOOK_W_INSTALLED != 0
}

fn install_font_detour() -> bool {
    if DETOUR.get().is_some() {
        return true;
    }
    let Some(original) = find_create_font_indirect_w() else {
        debug("[explorer-hook] CreateFontIndirectW not found; no hook installed");
        return false;
    };
    // Make the direct-call fallback available before the patch goes live.
    let _ = ORIGINAL_W.set(original);
    // SAFETY: the detour is created once, enabled, and kept alive in DETOUR
    // for the lifetime of the process. Other threads are suspended by the
    // caller, so the non-atomic patch write cannot race a mid-call thread.
    unsafe {
        match GenericDetour::new(original, hook_create_font_indirect_w) {
            Ok(detour) => {
                if let Err(e) = detour.enable() {
                    debug(&format!("[explorer-hook] detour enable failed: {e}"));
                    return false;
                }
                let _ = DETOUR.set(detour);
                debug("[explorer-hook] CreateFontIndirectW detour installed");
                true
            }
            Err(e) => {
                debug(&format!("[explorer-hook] detour setup failed: {e}"));
                false
            }
        }
    }
}

// CreateFontIndirectExW is optional — if gdi32 ever lacks the export, skip it
// rather than failing the whole hook.
fn install_font_ex_detour() -> bool {
    if DETOUR_EX.get().is_some() {
        return true;
    }
    let Some(original) = find_create_font_indirect_ex_w() else {
        debug("[explorer-hook] CreateFontIndirectExW not found; skipping");
        return false;
    };
    let _ = ORIGINAL_EX.set(original);
    // SAFETY: the detour is created once, enabled, and kept alive in DETOUR_EX
    // for the lifetime of the process. Other threads are suspended by the
    // caller (see install_font_detour).
    unsafe {
        match GenericDetour::new(original, hook_create_font_indirect_ex_w) {
            Ok(detour) => {
                if let Err(e) = detour.enable() {
                    debug(&format!("[explorer-hook] Ex detour enable failed: {e}"));
                    return false;
                }
                let _ = DETOUR_EX.set(detour);
                debug("[explorer-hook] CreateFontIndirectExW detour installed");
                true
            }
            Err(e) => {
                debug(&format!("[explorer-hook] Ex detour setup failed: {e}"));
                false
            }
        }
    }
}

// Restore the pristine function bytes on DLL_PROCESS_DETACH so a later
// re-enable patches a clean entry. Without this, gdi32 stays patched after
// the module unloads and a re-install would detour the old jump bytes,
// corrupting the chain. Other threads are suspended while the bytes are
// written back, same as the install side.
fn disable_detours() {
    let suspended = suspend_other_threads();
    // SAFETY: disable() restores the original prolog; the detours live for
    // the process lifetime and are only disabled here at detach.
    unsafe {
        if let Some(detour) = DETOUR.get() {
            let _ = detour.disable();
        }
        if let Some(detour) = DETOUR_EX.get() {
            let _ = detour.disable();
        }
    }
    drop(suspended);
}

// Exported so the app can hand it to SetWindowsHookExW. The hook only exists
// to make Windows load this DLL into explorer.exe through the documented hook
// mechanism (no CreateRemoteThread / VirtualAllocEx), which is why it does not
// trip the injection signatures that trip Windows Defender. The first call
// also runs the deferred detour install (out of DllMain / the loader lock);
// every callback is then forwarded straight through.
#[no_mangle]
pub unsafe extern "system" fn hook_callback(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Only target processes install the detour; every other host just forwards.
    // With a global WH_CBT hook this fires on window creation, before the
    // window's fonts are rasterized, so the fix reaches fast-starting applets.
    if IS_TARGET.load(Ordering::Acquire) && !INSTALL_ATTEMPTED.swap(true, Ordering::SeqCst) {
        // Runs on the hooked thread; fires on the first processed callback
        // after the DLL is loaded. On failure, reset so the next callback
        // retries — the callback fires on every window creation, so this
        // self-heals.
        if !install_detour() {
            INSTALL_ATTEMPTED.store(false, Ordering::SeqCst);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _hinst: windows::Win32::Foundation::HINSTANCE,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            // Keep DllMain minimal: cache the target decision once, then map
            // the shared state. The detour install is deferred to the first
            // hook callback so no heavy work (VirtualAlloc, disassembly,
            // patching) runs under the loader lock.
            let target = is_target();
            IS_TARGET.store(target, Ordering::Release);
            if target {
                map_state();
            }
            BOOL(1)
        }
        DLL_PROCESS_DETACH => {
            if IS_TARGET.load(Ordering::Acquire) {
                disable_detours();
            }
            BOOL(1)
        }
        DLL_THREAD_ATTACH | DLL_THREAD_DETACH => BOOL(1),
        _ => BOOL(1),
    }
}
