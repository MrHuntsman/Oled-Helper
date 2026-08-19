//! font.rs, resolve a GDI LOGFONTW face name to font-file bytes for swash.
//!
//! GDI hands us an HFONT; there is no Win32 API to map that straight back to a
//! font file, so we go through the registry font table (the same lookup MacType
//! uses): `HKLM/HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts` maps a
//! display name like "Segoe UI (TrueType)" to a file name like "segoeui.ttf".

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, REG_VALUE_TYPE,
};

const FONTS_SUBKEY: &str = r"Software\Microsoft\Windows NT\CurrentVersion\Fonts";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// Read a REG_SZ value by name from the fonts subkey of one hive. Returns None
// for a missing key/value or any read failure (the caller just tries the next
// candidate/hive).
fn read_font_value(hkey: HKEY, value: &str) -> Option<String> {
    unsafe {
        let mut key = HKEY::default();
        let open = RegOpenKeyExW(
            hkey,
            PCWSTR(wide(FONTS_SUBKEY).as_ptr()),
            Some(0),
            KEY_QUERY_VALUE,
            &mut key,
        );
        if open != ERROR_SUCCESS || key.is_invalid() {
            return None;
        }
        let value_name = wide(value);
        let mut required = 0u32;
        let mut value_type = REG_VALUE_TYPE::default();
        let size_query = RegQueryValueExW(
            key,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut value_type),
            None,
            Some(&mut required),
        );
        if size_query != ERROR_SUCCESS || required == 0 || required > 1024 {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buf = vec![0u16; (required as usize).div_ceil(2)];
        let mut read = required;
        let query = RegQueryValueExW(
            key,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut value_type),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut read),
        );
        let _ = RegCloseKey(key);
        if query != ERROR_SUCCESS {
            return None;
        }
        Some(
            String::from_utf16_lossy(&buf)
                .trim_end_matches('\0')
                .to_string(),
        )
    }
}

// The registry display name for a face varies with weight/italic, e.g.
// "Segoe UI (TrueType)", "Arial Bold (TrueType)", "Arial Italic (OpenType)".
// Try every plausible spelling.
fn display_name_candidates(face: &str, weight: i32, italic: bool) -> Vec<String> {
    let suffix = match (weight >= 600, italic) {
        (false, false) => "",
        (true, false) => " Bold",
        (false, true) => " Italic",
        (true, true) => " Bold Italic",
    };
    vec![
        format!("{face}{suffix} (TrueType)"),
        format!("{face}{suffix} (OpenType)"),
        format!("{face}{suffix}"),
    ]
}

// Turn the registry value (a bare file name or an absolute path) into a full
// path. System font values are relative to the Windows fonts directory.
fn resolve_font_path(file: &str) -> Option<PathBuf> {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        return Some(path);
    }
    let windir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    Some(windir.join("Fonts").join(file))
}

// Resolve a face (unchecked, always index 0, TTC face selection comes later)
// to its file bytes.
fn resolve_font(face: &str, weight: i32, italic: bool) -> Option<Arc<[u8]>> {
    for name in display_name_candidates(face, weight, italic) {
        for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            if let Some(file) = read_font_value(hive, &name) {
                if let Some(path) = resolve_font_path(&file) {
                    if let Ok(data) = std::fs::read(&path) {
                        if !data.is_empty() {
                            return Some(Arc::from(data));
                        }
                    }
                }
            }
        }
    }
    None
}

// Per-process cache: resolution touches the registry and reads a font file, so
// it must not run on every text draw. Negative results are cached too so an
// unsupported face does not re-query the registry each draw.
static FONT_CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<[u8]>>>>> = OnceLock::new();

pub fn cached_font(face: &str, weight: i32, italic: bool) -> Option<Arc<[u8]>> {
    let key = format!("{face}|{weight}|{italic}");
    let cache = FONT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(entry) = cache.lock().unwrap().get(&key) {
        return entry.clone();
    }
    let resolved = resolve_font(face, weight, italic);
    cache
        .lock()
        .unwrap()
        .insert(key, resolved.clone());
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_common_system_fonts() {
        for face in ["Arial", "Segoe UI"] {
            let data = resolve_font(face, 400, false).expect("font resolves");
            assert!(!data.is_empty(), "{face} should have non-empty bytes");
        }
    }

    #[test]
    fn resolves_bold_variant() {
        // Arial Bold lives in its own file (arialbd.ttf) in the registry.
        let data = resolve_font("Arial", 700, false).expect("bold font resolves");
        assert!(!data.is_empty());
    }
}
