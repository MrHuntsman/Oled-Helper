// hotkeys.rs — global hotkey bindings for the app's main features.
//
// Bindings are stored as strings in the config ("Ctrl+Alt+F9", or "None" for
// unbound) and registered with RegisterHotKey on the main window. WM_HOTKEY
// arrives through the window subclass installed in lib.rs and is dispatched
// here as a "hotkey-pressed" event that the frontend turns into the actual
// action (toggle black crush, step the level, flip the dimmer, toggle HDR, ...).
//
// Increase/decrease bindings are registered WITHOUT MOD_NOREPEAT so holding
// the key auto-repeats WM_HOTKEY, giving hold-to-repeat for free.

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};

// Hotkey IDs (WM_HOTKEY wparam). Stable per action — not persisted anywhere,
// but kept distinct so multiple bindings never collide on one slot.
pub const HK_TOGGLE_DIMMER: i32 = 1;
pub const HK_TOGGLE_CRUSH: i32 = 2;
pub const HK_DECREASE: i32 = 4;
pub const HK_INCREASE: i32 = 5;
pub const HK_TOGGLE_HDR: i32 = 8;

// action name → (hotkey id, auto-repeat). Auto-repeat bindings are registered
// without MOD_NOREPEAT so the OS keeps sending WM_HOTKEY while the key is held.
pub const ACTIONS: [(&str, i32, bool); 5] = [
    ("toggle_taskbar_dimmer", HK_TOGGLE_DIMMER, false),
    ("toggle_black_crush", HK_TOGGLE_CRUSH, false),
    ("decrease_black_crush", HK_DECREASE, true),
    ("increase_black_crush", HK_INCREASE, true),
    ("toggle_hdr", HK_TOGGLE_HDR, false),
];

static APP: OnceLock<AppHandle> = OnceLock::new();
static MAIN_HWND: Mutex<Option<isize>> = Mutex::new(None);
static BINDINGS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

// ── parse_hotkey ──────────────────────────────────────────────────────────────
// Parses "Ctrl+Alt+F9" → (modifiers, vk). Returns None for "None", empty, or
// unrecognised keys. The key names match exactly what the frontend capture UI
// produces (see TabHotkeys.svelte) so a captured combo always round-trips.

pub fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut mods: u32 = 0;
    let mut key_part = s;
    loop {
        if let Some(rest) = key_part.strip_prefix("Win+") {
            mods |= MOD_WIN.0;
            key_part = rest;
        } else if let Some(rest) = key_part.strip_prefix("Ctrl+") {
            mods |= MOD_CONTROL.0;
            key_part = rest;
        } else if let Some(rest) = key_part.strip_prefix("Alt+") {
            mods |= MOD_ALT.0;
            key_part = rest;
        } else if let Some(rest) = key_part.strip_prefix("Shift+") {
            mods |= MOD_SHIFT.0;
            key_part = rest;
        } else {
            break;
        }
    }

    let vk = key_table()
        .binary_search_by_key(&key_part, |&(name, _)| name)
        .ok()
        .map(|i| key_table()[i].1)?;
    Some((mods, vk))
}

// The complete key-name table. Sorted by lexicographic byte order so
// parse_hotkey can binary-search it; exposed as a function so the unit tests
// can assert the ordering property on the real table.
fn key_table() -> &'static [(&'static str, u32)] {
    &[
        ("'", 0xDE),
        (",", 0xBC),
        ("-", 0xBD),
        (".", 0xBE),
        ("/", 0xBF),
        ("0", 0x30),
        ("1", 0x31),
        ("2", 0x32),
        ("3", 0x33),
        ("4", 0x34),
        ("5", 0x35),
        ("6", 0x36),
        ("7", 0x37),
        ("8", 0x38),
        ("9", 0x39),
        (";", 0xBA),
        ("=", 0xBB),
        ("A", 0x41),
        ("B", 0x42),
        ("Backspace", 0x08),
        ("C", 0x43),
        ("CapsLk", 0x14),
        ("D", 0x44),
        ("Delete", 0x2E),
        ("Down", 0x28),
        ("E", 0x45),
        ("End", 0x23),
        ("Enter", 0x0D),
        ("Esc", 0x1B),
        ("F", 0x46),
        ("F1", 0x70),
        ("F10", 0x79),
        ("F11", 0x7A),
        ("F12", 0x7B),
        ("F13", 0x7C),
        ("F14", 0x7D),
        ("F15", 0x7E),
        ("F16", 0x7F),
        ("F17", 0x80),
        ("F18", 0x81),
        ("F19", 0x82),
        ("F2", 0x71),
        ("F20", 0x83),
        ("F21", 0x84),
        ("F22", 0x85),
        ("F23", 0x86),
        ("F24", 0x87),
        ("F3", 0x72),
        ("F4", 0x73),
        ("F5", 0x74),
        ("F6", 0x75),
        ("F7", 0x76),
        ("F8", 0x77),
        ("F9", 0x78),
        ("G", 0x47),
        ("H", 0x48),
        ("Home", 0x24),
        ("I", 0x49),
        ("Insert", 0x2D),
        ("J", 0x4A),
        ("K", 0x4B),
        ("L", 0x4C),
        ("Left", 0x25),
        ("M", 0x4D),
        ("N", 0x4E),
        ("Num*", 0x6A),
        ("Num+", 0x6B),
        ("Num-", 0x6D),
        ("Num.", 0x6E),
        ("Num/", 0x6F),
        ("Num0", 0x60),
        ("Num1", 0x61),
        ("Num2", 0x62),
        ("Num3", 0x63),
        ("Num4", 0x64),
        ("Num5", 0x65),
        ("Num6", 0x66),
        ("Num7", 0x67),
        ("Num8", 0x68),
        ("Num9", 0x69),
        ("O", 0x4F),
        ("P", 0x50),
        ("Pause", 0x13),
        ("PgDn", 0x22),
        ("PgUp", 0x21),
        ("Print", 0x2C),
        ("Q", 0x51),
        ("R", 0x52),
        ("Right", 0x27),
        ("S", 0x53),
        ("Space", 0x20),
        ("T", 0x54),
        ("Tab", 0x09),
        ("U", 0x55),
        ("Up", 0x26),
        ("V", 0x56),
        ("W", 0x57),
        ("X", 0x58),
        ("Y", 0x59),
        ("Z", 0x5A),
        ("[", 0xDB),
        ("\\", 0xDC),
        ("]", 0xDD),
        ("`", 0xC0),
    ]
}

// ── Registration ─────────────────────────────────────────────────────────────

pub fn init(app: AppHandle, hwnd: isize, config: &crate::config::AppConfig) {
    let _ = APP.set(app);
    if let Ok(mut guard) = MAIN_HWND.lock() {
        *guard = Some(hwnd);
    }
    if let Ok(mut bindings) = BINDINGS.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        *bindings = config.hotkeys.clone();
    }
    if let Err(errors) = register_all() {
        log::warn!("[hotkeys] some bindings failed to register: {errors}");
    }
}

pub fn current_bindings() -> HashMap<String, String> {
    BINDINGS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map(|b| b.clone())
        .unwrap_or_default()
}

fn main_hwnd() -> Option<HWND> {
    MAIN_HWND
        .lock()
        .ok()
        .and_then(|g| *g)
        .map(|raw| HWND(raw as *mut _))
}

/// Unregisters every slot, then re-registers the current bindings. Returns an
/// error listing the combos that could not be registered (e.g. already in use
/// by another application or bound twice within this app).
pub fn register_all() -> Result<(), String> {
    let hwnd = main_hwnd().ok_or_else(|| "hotkey window is not ready".to_string())?;
    unsafe {
        for (_, id, _) in ACTIONS {
            let _ = UnregisterHotKey(Some(hwnd), id);
        }
    }
    let bindings = current_bindings();
    let mut errors: Vec<String> = Vec::new();
    for (action, id, repeat) in ACTIONS {
        let Some(combo) = bindings.get(action) else {
            continue;
        };
        let Some((mods, vk)) = parse_hotkey(combo) else {
            continue;
        };
        // "None" is stored as the unbound marker; parse_hotkey returns None for
        // it, so the loop above already skipped it — but guard anyway.
        let mods = if repeat { mods } else { mods | MOD_NOREPEAT.0 };
        unsafe {
            match RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(mods), vk) {
                Ok(()) => {}
                Err(e) => {
                    let code = e.code().0;
                    let detail = if (code & 0xFFFF) == 1409 {
                        // ERROR_HOTKEY_ALREADY_REGISTERED
                        "already in use by another application or action".to_string()
                    } else {
                        format!("{e}")
                    };
                    errors.push(format!("{action} ({combo}): {detail}"));
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

// ── Command: set / clear one binding ─────────────────────────────────────────

#[tauri::command]
pub fn set_hotkey(action: String, combo: String) -> Result<(), String> {
    if !ACTIONS.iter().any(|(name, _, _)| *name == action) {
        return Err(format!("unknown hotkey action '{action}'"));
    }
    let combo = combo.trim().to_string();
    if !combo.is_empty() && !combo.eq_ignore_ascii_case("none") && parse_hotkey(&combo).is_none() {
        return Err(format!("unrecognized key combination '{combo}'"));
    }
    let previous = {
        let mut bindings = BINDINGS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| "hotkey bindings state was poisoned".to_string())?;
        let previous = bindings.get(&action).cloned();
        if combo.is_empty() || combo.eq_ignore_ascii_case("none") {
            bindings.remove(&action);
        } else {
            bindings.insert(action.clone(), combo.clone());
        }
        previous
    };
    // Roll back the in-memory binding if registration fails so the UI never
    // shows a combo that isn't actually live.
    if let Err(errors) = register_all() {
        if let Ok(mut bindings) = BINDINGS.get_or_init(|| Mutex::new(HashMap::new())).lock() {
            match previous {
                Some(p) => {
                    bindings.insert(action, p);
                }
                None => {
                    bindings.remove(&action);
                }
            }
        }
        return Err(errors);
    }
    Ok(())
}

#[tauri::command]
pub fn reset_hotkeys() -> Result<(), String> {
    if let Ok(mut bindings) = BINDINGS.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        bindings.clear();
    }
    register_all()
}

// ── WM_HOTKEY dispatch (called from the window subclass on the main thread) ─

pub fn handle_hotkey(id: i32) {
    let Some((action, _, _)) = ACTIONS.iter().find(|(_, hk, _)| *hk == id) else {
        return;
    };
    emit("hotkey-pressed", action);
}

fn emit(event: &str, action: &str) {
    if let Some(app) = APP.get() {
        let _ = app.emit(event, action.to_string());
    }
}

pub fn shutdown() {
    if let Some(hwnd) = main_hwnd() {
        unsafe {
            for (_, id, _) in ACTIONS {
                let _ = UnregisterHotKey(Some(hwnd), id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifier_combos() {
        assert_eq!(
            parse_hotkey("Ctrl+Alt+F9"),
            Some((MOD_CONTROL.0 | MOD_ALT.0, 0x78))
        );
        assert_eq!(
            parse_hotkey("Win+Ctrl+Shift+Space"),
            Some((MOD_WIN.0 | MOD_CONTROL.0 | MOD_SHIFT.0, 0x20))
        );
        assert_eq!(parse_hotkey("F5"), Some((0, 0x74)));
        assert_eq!(parse_hotkey("Alt+A"), Some((MOD_ALT.0, 0x41)));
        assert_eq!(parse_hotkey("Ctrl+Num3"), Some((MOD_CONTROL.0, 0x63)));
    }

    #[test]
    fn rejects_invalid_or_unbound() {
        assert_eq!(parse_hotkey("None"), None);
        assert_eq!(parse_hotkey(""), None);
        assert_eq!(parse_hotkey("nOnE"), None);
        assert_eq!(parse_hotkey("Ctrl+NotAKey"), None);
        assert_eq!(parse_hotkey("Alt+F9+Ctrl"), None); // modifier after key
        assert_eq!(parse_hotkey("Ctrl++"), None);
    }
    #[test]
    fn key_table_is_sorted_for_binary_search() {
        // parse_hotkey uses binary_search_by_key, which requires the table to
        // be sorted — a regression here silently drops a key from the lookup.
        let entries: Vec<(&str, u32)> = key_table().to_vec();
        let mut sorted = entries.clone();
        sorted.sort_by_key(|(name, _)| *name);
        assert_eq!(entries, sorted, "KEY_TABLE must stay sorted");
        assert!(!entries.is_empty());
    }
}
