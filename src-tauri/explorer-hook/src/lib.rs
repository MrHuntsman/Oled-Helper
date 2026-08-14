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
//! converted too (smoothing off means no smoothing at all). The mode is
//! re-read on every CreateFontIndirectW call, so the app can flip it live
//! without unloading the DLL.
//!
//! The DLL also watches FontSmoothingGamma in the registry and re-asserts
//! the system smoothing state when it changes, so gdi32 re-reads the new
//! gamma and the app's gamma slider controls the edge darkness of Explorer's
//! grayscale text live (gamma has no SPI action of its own).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use retour::GenericDetour;
use windows::core::{s, w, BOOL, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, CLEARTYPE_NATURAL_QUALITY, CLEARTYPE_QUALITY, HFONT, LOGFONTW,
    NONANTIALIASED_QUALITY,
};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, FILE_MAP_READ};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    REG_DWORD, REG_VALUE_TYPE,
};
use windows::Win32::System::SystemServices::{
    DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, DLL_THREAD_ATTACH, DLL_THREAD_DETACH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_SETFONTSMOOTHING, SPIF_UPDATEINIFILE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

type CreateFontIndirectWFn = unsafe extern "system" fn(*const LOGFONTW) -> HFONT;

// Shared mode (0 = passthrough, 1 = grayscale, 2 = crisp) + the installed
// detour. Both are set once at attach time and live for the process
// lifetime. STATE stores the mapped view's raw pointer as a usize (raw
// pointers are not Send/Sync, so we carry the address in an integer type
// instead).
static STATE: OnceLock<usize> = OnceLock::new();
static DETOUR: OnceLock<GenericDetour<CreateFontIndirectWFn>> = OnceLock::new();

// Gamma watch state (see maybe_flush_gamma). u32::MAX marks "never read".
static LAST_GAMMA: AtomicU32 = AtomicU32::new(u32::MAX);
static LAST_GAMMA_CHECK_MS: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn hook_create_font_indirect_w(lplf: *const LOGFONTW) -> HFONT {
    let Some(detour) = DETOUR.get() else {
        return HFONT::default();
    };
    maybe_flush_gamma();
    let mode = state_mode();
    if mode == 0 {
        return detour.call(lplf);
    }
    let mut lf = *lplf;
    // Only neutralize the fringing cases: fonts that explicitly asked for
    // ClearType. Fonts that already asked for grayscale or non-antialiased
    // rendering keep their intent, and DEFAULT_QUALITY fonts follow the
    // system smoothing state (which the app already sets per mode).
    let is_cleartype = lf.lfQuality == CLEARTYPE_QUALITY
        || u32::from(lf.lfQuality.0) == CLEARTYPE_NATURAL_QUALITY;
    // Crisp mode also converts explicit grayscale requests — smoothing off
    // means no smoothing at all — but leaves non-antialiased fonts alone.
    let needs_rewrite = is_cleartype || (mode == 2 && lf.lfQuality == ANTIALIASED_QUALITY);
    if needs_rewrite {
        lf.lfQuality = if mode == 2 {
            NONANTIALIASED_QUALITY
        } else {
            ANTIALIASED_QUALITY
        };
    }
    detour.call(&lf)
}

fn state_mode() -> u32 {
    // SAFETY: STATE is written once in map_state; the mapped view stays valid
    // for the lifetime of the process.
    unsafe { STATE.get().map(|addr| *( *addr as *const u32)).unwrap_or(0) }
}

fn tick_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_reg_dword(subkey: &str, value: &str) -> Option<u32> {
    // SAFETY: standard HKCU registry reads with bounded buffers.
    unsafe {
        let mut key = HKEY::default();
        let open = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(subkey).as_ptr()),
            Some(0),
            KEY_QUERY_VALUE,
            &mut key,
        );
        if open.is_err() || key.is_invalid() {
            return None;
        }
        let mut data: u32 = 0;
        let mut size: u32 = 4;
        let mut value_type = REG_VALUE_TYPE::default();
        let query = RegQueryValueExW(
            key,
            PCWSTR(wide(value).as_ptr()),
            None,
            Some(&mut value_type),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if query.is_err() || value_type != REG_DWORD {
            return None;
        }
        Some(data)
    }
}

fn read_reg_string(subkey: &str, value: &str) -> Option<String> {
    // SAFETY: standard HKCU registry reads with bounded buffers.
    unsafe {
        let mut key = HKEY::default();
        let open = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(subkey).as_ptr()),
            Some(0),
            KEY_QUERY_VALUE,
            &mut key,
        );
        if open.is_err() || key.is_invalid() {
            return None;
        }
        let mut required = 0u32;
        let mut value_type = REG_VALUE_TYPE::default();
        let size_query = RegQueryValueExW(
            key,
            PCWSTR(wide(value).as_ptr()),
            None,
            Some(&mut value_type),
            None,
            Some(&mut required),
        );
        if size_query.is_err() || required == 0 || required > 64 {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buf = vec![0u16; (required as usize).div_ceil(2)];
        let mut read = required;
        let query = RegQueryValueExW(
            key,
            PCWSTR(wide(value).as_ptr()),
            None,
            Some(&mut value_type),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut read),
        );
        let _ = RegCloseKey(key);
        if query.is_err() {
            return None;
        }
        Some(
            String::from_utf16_lossy(&buf)
                .trim_end_matches('\0')
                .to_string(),
        )
    }
}

// FontSmoothingGamma has no SPI action, so the app writes it to the registry
// directly. gdi32 caches the smoothing parameters per process, so a running
// explorer.exe would keep rendering with the old gamma until restarted. To
// make the app's gamma slider live for Explorer, watch the registry value
// (throttled — this runs on every font creation) and, when it changes,
// re-assert the current smoothing state via SPI so gdi32 re-reads it.
fn maybe_flush_gamma() {
    let now = tick_ms();
    let last_check = LAST_GAMMA_CHECK_MS.load(Ordering::Relaxed);
    if now.wrapping_sub(last_check) < 500 {
        return;
    }
    LAST_GAMMA_CHECK_MS.store(now, Ordering::Relaxed);
    let Some(gamma) = read_reg_dword(r"Control Panel\Desktop", "FontSmoothingGamma") else {
        return;
    };
    let previous = LAST_GAMMA.swap(gamma, Ordering::Relaxed);
    if previous == u32::MAX || previous == gamma {
        return; // first read or unchanged
    }
    let enabled = read_reg_string(r"Control Panel\Desktop", "FontSmoothing")
        .map(|v| v.trim() != "0")
        .unwrap_or(true);
    // SAFETY: re-asserting the current value is a no-op configurationally;
    // it only nudges gdi32 to invalidate its smoothing cache.
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETFONTSMOOTHING,
            if enabled { 2 } else { 0 },
            None,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(SPIF_UPDATEINIFILE.0),
        );
    }
    debug(&format!("[explorer-hook] gamma changed to {gamma}; flushed smoothing state"));
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
