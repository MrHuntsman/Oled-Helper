//! explorer-hook.dll — injected into explorer.exe to override its font
//! antialiasing. The Windows shell hard-codes ClearType and ignores the
//! per-user font-smoothing registry values, so the only way to change its
//! text is to hook its font creation and rewrite the quality flag.
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

use std::sync::OnceLock;

use retour::GenericDetour;
use windows::core::{s, w, BOOL, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, CLEARTYPE_NATURAL_QUALITY, CLEARTYPE_QUALITY, ENUMLOGFONTEXDVW, FONT_QUALITY,
    HFONT, LOGFONTW, NONANTIALIASED_QUALITY,
};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE};
use windows::Win32::System::SystemServices::{
    DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, DLL_THREAD_ATTACH, DLL_THREAD_DETACH,
};

// Shared memory layout (12 bytes): the app writes the mode at offset 0; the
// DLL writes its own state back at offsets 4 and 8 so the app can verify the
// hook is really installed in the CURRENT explorer.exe instead of trusting
// its own injection bookkeeping.
const STATE_MODE: usize = 0;
const STATE_STATUS: usize = 4;
const STATE_PID: usize = 8;
const STATE_SIZE: usize = 12;
// Status bits written by the DLL at attach.
const STATUS_HOOK_W_INSTALLED: u32 = 1;
const STATUS_HOOK_EX_INSTALLED: u32 = 2;

type CreateFontIndirectWFn = unsafe extern "system" fn(*const LOGFONTW) -> HFONT;
type CreateFontIndirectExWFn = unsafe extern "system" fn(*const ENUMLOGFONTEXDVW) -> HFONT;

// Shared mode (0 = passthrough, 1 = grayscale, 2 = crisp) + the installed
// detours. Both are set once at attach time and live for the process
// lifetime. STATE stores the mapped view's raw pointer as a usize (raw
// pointers are not Send/Sync, so we carry the address in an integer type
// instead).
static STATE: OnceLock<usize> = OnceLock::new();
static DETOUR: OnceLock<GenericDetour<CreateFontIndirectWFn>> = OnceLock::new();
static DETOUR_EX: OnceLock<GenericDetour<CreateFontIndirectExWFn>> = OnceLock::new();

unsafe extern "system" fn hook_create_font_indirect_w(lplf: *const LOGFONTW) -> HFONT {
    let Some(detour) = DETOUR.get() else {
        return HFONT::default();
    };
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

// CreateFontIndirectExW takes an ENUMLOGFONTEXDVW whose first member is the
// LOGFONT, so the requested quality sits at the same offset as in
// CreateFontIndirectW. A few shell font paths use this variant, so without
// this detour they would keep ClearType fringing.
unsafe extern "system" fn hook_create_font_indirect_ex_w(lplf: *const ENUMLOGFONTEXDVW) -> HFONT {
    let Some(detour) = DETOUR_EX.get() else {
        return HFONT::default();
    };
    if lplf.is_null() {
        return detour.call(lplf);
    }
    let mode = state_mode();
    let quality = (*(lplf as *const LOGFONTW)).lfQuality;
    let Some(rewritten) = rewritten_quality(mode, quality) else {
        return detour.call(lplf);
    };
    // Copy the (large) struct so the caller's is never mutated, flip the
    // embedded quality, and call the original with the copy.
    let mut copy = *lplf;
    copy.elfEnumLogfontEx.elfLogFont.lfQuality = rewritten;
    detour.call(&copy)
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
// generation instead of trusting its own injection bookkeeping. Called from
// DllMain at attach, so by the time LoadLibraryW returns in the injector the
// report is already visible.
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

fn install_detour() {
    let mut status = 0;
    if install_font_detour() {
        status |= STATUS_HOOK_W_INSTALLED;
    }
    if install_font_ex_detour() {
        status |= STATUS_HOOK_EX_INSTALLED;
    }
    // Report what actually got installed so the app can tell "hooked" from
    // "DLL loaded but detour failed" — the status write happens while
    // LoadLibraryW is still inside DllMain, so inject() sees it on return.
    report_state(status);
}

fn install_font_detour() -> bool {
    if DETOUR.get().is_some() {
        return true;
    }
    let Some(original) = find_create_font_indirect_w() else {
        debug("[explorer-hook] CreateFontIndirectW not found; no hook installed");
        return false;
    };
    // SAFETY: the detour is created once, enabled, and kept alive in DETOUR
    // for the lifetime of the process.
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
    // SAFETY: the detour is created once, enabled, and kept alive in DETOUR_EX
    // for the lifetime of the process.
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

#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _hinst: windows::Win32::Foundation::HINSTANCE,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            map_state();
            install_detour();
            BOOL(1)
        }
        DLL_PROCESS_DETACH | DLL_THREAD_ATTACH | DLL_THREAD_DETACH => BOOL(1),
        _ => BOOL(1),
    }
}
