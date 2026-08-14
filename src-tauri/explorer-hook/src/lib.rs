//! explorer-hook.dll — injected into explorer.exe to force grayscale font
//! antialiasing. The Windows shell hard-codes ClearType and ignores the
//! per-user font-smoothing registry values, so the only way to change its
//! text is to hook its font creation and rewrite the quality flag.
//!
//! The app (OLED Helper) controls this via a session-local shared-memory
//! flag: 0 = pass through untouched, 1 = force `ANTIALIASED_QUALITY`
//! (grayscale). The flag is re-read on every CreateFontIndirectW call, so the
//! app can flip it live without unloading the DLL.

use std::sync::OnceLock;

use retour::GenericDetour;
use windows::core::{s, w, BOOL, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Graphics::Gdi::{ANTIALIASED_QUALITY, HFONT, LOGFONTW};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, FILE_MAP_READ};
use windows::Win32::System::SystemServices::{
    DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, DLL_THREAD_ATTACH, DLL_THREAD_DETACH,
};

type CreateFontIndirectWFn = unsafe extern "system" fn(*const LOGFONTW) -> HFONT;

// Shared flag (0 = passthrough, 1 = force grayscale) + the installed detour.
// Both are set once at attach time and live for the process lifetime.
// STATE stores the mapped view's raw pointer as a usize (raw pointers are not
// Send/Sync, so we carry the address in an integer type instead).
static STATE: OnceLock<usize> = OnceLock::new();
static DETOUR: OnceLock<GenericDetour<CreateFontIndirectWFn>> = OnceLock::new();

unsafe extern "system" fn hook_create_font_indirect_w(lplf: *const LOGFONTW) -> HFONT {
    let Some(detour) = DETOUR.get() else {
        return HFONT::default();
    };
    if state_enabled() {
        let mut lf = *lplf;
        lf.lfQuality = ANTIALIASED_QUALITY;
        detour.call(&lf)
    } else {
        detour.call(lplf)
    }
}

fn state_enabled() -> bool {
    // SAFETY: STATE is written once in map_state; the mapped view stays valid
    // for the lifetime of the process.
    unsafe {
        STATE
            .get()
            .map(|addr| *( *addr as *const u32) != 0)
            .unwrap_or(false)
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
        let Ok(mapping) = OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) else {
            return;
        };
        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 4);
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

fn install_detour() {
    if DETOUR.get().is_some() {
        return;
    }
    let Some(original) = find_create_font_indirect_w() else {
        debug("[explorer-hook] CreateFontIndirectW not found; no hook installed");
        return;
    };
    // SAFETY: the detour is created once, enabled, and kept alive in DETOUR
    // for the lifetime of the process.
    unsafe {
        match GenericDetour::new(original, hook_create_font_indirect_w) {
            Ok(detour) => {
                if let Err(e) = detour.enable() {
                    debug(&format!("[explorer-hook] detour enable failed: {e}"));
                    return;
                }
                let _ = DETOUR.set(detour);
                debug("[explorer-hook] CreateFontIndirectW detour installed");
            }
            Err(e) => {
                debug(&format!("[explorer-hook] detour setup failed: {e}"));
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
