// hotkeys.rs, global hotkey bindings for the app's main features.
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
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
    MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

// Hotkey IDs (WM_HOTKEY wparam). Stable per action, not persisted anywhere,
// but kept distinct so multiple bindings never collide on one slot.
pub const HK_TOGGLE_DIMMER: i32 = 1;
pub const HK_TOGGLE_CRUSH: i32 = 2;
pub const HK_DECREASE: i32 = 4;
pub const HK_INCREASE: i32 = 5;
pub const HK_TOGGLE_HDR: i32 = 8;
pub const HK_HOLD_COMPARE: i32 = 16;
pub const HK_DECREASE_SDR_BRIGHTNESS: i32 = 32;
pub const HK_INCREASE_SDR_BRIGHTNESS: i32 = 33;
pub const HK_TURN_OFF_DISPLAY: i32 = 64;
pub const HK_TOGGLE_SCREEN_DIMMER: i32 = 128;

// action name → (hotkey id, auto-repeat). Auto-repeat bindings are registered
// without MOD_NOREPEAT so the OS keeps sending WM_HOTKEY while the key is held.
pub const ACTIONS: [(&str, i32, bool); 10] = [
    ("toggle_taskbar_dimmer", HK_TOGGLE_DIMMER, false),
    ("toggle_black_crush", HK_TOGGLE_CRUSH, false),
    ("decrease_black_crush", HK_DECREASE, true),
    ("increase_black_crush", HK_INCREASE, true),
    ("toggle_hdr", HK_TOGGLE_HDR, false),
    ("hold_compare", HK_HOLD_COMPARE, false),
    ("decrease_sdr_brightness", HK_DECREASE_SDR_BRIGHTNESS, true),
    ("increase_sdr_brightness", HK_INCREASE_SDR_BRIGHTNESS, true),
    ("turn_off_display", HK_TURN_OFF_DISPLAY, false),
    ("toggle_screen_dimmer", HK_TOGGLE_SCREEN_DIMMER, false),
];

// ── Favorite hotkeys (bound directly to each favorite) ───────────────
// Favorite resolution hotkeys are registered dynamically on demand (see
// DYNAMIC_IDS below), so any number of favorites can have a hotkey with no
// fixed cap. The action name embeds the favorite's stable id
// ("favorite_<favId>"), so a hotkey stays bound to a specific saved favorite
// even if the favorites list is reordered, and removing a favorite makes its
// action inert (the frontend prunes it). Ids only need to be unique and
// stable within a process lifetime: WM_HOTKEY dispatch reverse-looks up the
// id through the same map, so allocation order across restarts does not
// matter.
static DYNAMIC_IDS: OnceLock<Mutex<HashMap<String, i32>>> = OnceLock::new();
static NEXT_DYNAMIC_ID: AtomicI32 = AtomicI32::new(4096);

fn is_favorite_action(action: &str) -> bool {
    action.strip_prefix("favorite_").is_some_and(|id| !id.is_empty())
}

fn dynamic_id(action: &str) -> i32 {
    let map = DYNAMIC_IDS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = map.lock() {
        if let Some(id) = guard.get(action) {
            return *id;
        }
    }
    let id = NEXT_DYNAMIC_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut guard) = map.lock() {
        guard.insert(action.to_string(), id);
    }
    id
}

static APP: OnceLock<AppHandle> = OnceLock::new();
static MAIN_HWND: Mutex<Option<isize>> = Mutex::new(None);
static BINDINGS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
static CURRENT_LIFT: OnceLock<Arc<AtomicI32>> = OnceLock::new();
// The per-monitor lift table and the screen-dimmer brightness map, registered
// at startup (lib.rs) so hold-to-compare can flatten and restore EVERY
// monitor, not just the primary. Same Arcs as gamma::GammaState.
static PER_MONITOR_LIFTS: OnceLock<Arc<Mutex<HashMap<String, i32>>>> = OnceLock::new();
static SCREEN_DIM_BRIGHTNESS: OnceLock<Arc<Mutex<HashMap<String, u32>>>> = OnceLock::new();
// Gamma writes can originate from the IPC command thread or the main-window
// timer. Serialize them so a slider update cannot interleave with compare's
// temporary linear ramp or its restoration.
static GAMMA_WRITE_LOCK: Mutex<()> = Mutex::new(());
// Hold-to-compare is owned by the main window's message loop. RegisterHotKey
// detects the press; a short-lived timer checks the physical key state until
// the user releases any required key. No system-wide keyboard hook is needed.
static HOLD_CONFIG_VK: AtomicU32 = AtomicU32::new(0);
static HOLD_CONFIG_MODS: AtomicU32 = AtomicU32::new(0);
static HOLD_ACTIVE_VK: AtomicU32 = AtomicU32::new(0);
static HOLD_ACTIVE_MODS: AtomicU32 = AtomicU32::new(0);
static HOLD_STATE: AtomicU32 = AtomicU32::new(0);
const HOLD_IDLE: u32 = 0;
const HOLD_ACTIVE: u32 = 1;
const HOLD_RESTORING: u32 = 2;
// Private timer id; WM_TIMER wParam carries this value.
const HOLD_TIMER_ID: usize = 0x4F48;
const HOLD_TIMER_INTERVAL_MS: u32 = 40;

pub fn set_current_lift(lift: Arc<AtomicI32>) {
    let _ = CURRENT_LIFT.set(lift);
}

pub fn set_per_monitor_lifts(per_monitor: Arc<Mutex<HashMap<String, i32>>>) {
    let _ = PER_MONITOR_LIFTS.set(per_monitor);
}

pub fn set_screen_dim_brightness(brightness: Arc<Mutex<HashMap<String, u32>>>) {
    let _ = SCREEN_DIM_BRIGHTNESS.set(brightness);
}

pub fn is_hold_compare_active() -> bool {
    HOLD_STATE.load(Ordering::Acquire) != HOLD_IDLE
}

pub fn gamma_write_guard() -> std::sync::MutexGuard<'static, ()> {
    GAMMA_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

// ── parse_hotkey ──────────────────────────────────────────────────────────────
// Parses "Ctrl+Alt+F9" → (modifiers, vk). Returns None for "None", empty, or
// unrecognised keys. The key names match exactly what the frontend capture UI
// produces (see TabHotkeys.svelte) so a captured combo always round-trips.

// Case-insensitive prefix strip for the modifier names ("Ctrl+", "ctrl+", ...).
// The prefixes are ASCII, so slicing at their length is always on a char
// boundary.
fn strip_modifier<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let end = prefix.len();
    if s.len() >= end && s[..end].eq_ignore_ascii_case(prefix) {
        Some(&s[end..])
    } else {
        None
    }
}

pub fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut mods: u32 = 0;
    let mut key_part = s;
    loop {
        if let Some(rest) = strip_modifier(key_part, "Win+") {
            mods |= MOD_WIN.0;
            key_part = rest;
        } else if let Some(rest) = strip_modifier(key_part, "Ctrl+") {
            mods |= MOD_CONTROL.0;
            key_part = rest;
        } else if let Some(rest) = strip_modifier(key_part, "Alt+") {
            mods |= MOD_ALT.0;
            key_part = rest;
        } else if let Some(rest) = strip_modifier(key_part, "Shift+") {
            mods |= MOD_SHIFT.0;
            key_part = rest;
        } else {
            break;
        }
    }

    // Match the key name case-insensitively (a hand-edited config may use
    // "esc"/"pgup" instead of the canonical "Esc"/"PgUp"). The table is small,
    // so a linear scan is both simple and fast.
    let vk = key_table()
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key_part))
        .map(|(_, vk)| *vk)?;
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
    sync_hold_binding();
    if let Err(errors) = register_all() {
        log::warn!("[hotkeys] some bindings failed to register: {errors}");
    }
    // Resident elevated key observer for hold-to-compare over elevated
    // (admin) windows. Starts silently: without the registered helper task
    // it logs once and hold detection falls back to the local key read.
    crate::key_observer::start();
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

/// Unregisters every slot (static and dynamic), then re-registers the current
/// bindings. Returns an error listing the combos that could not be registered
/// (e.g. already in use by another application or bound twice within this app).
pub fn register_all() -> Result<(), String> {
    let hwnd = main_hwnd().ok_or_else(|| "hotkey window is not ready".to_string())?;
    unsafe {
        for (_, id, _) in ACTIONS {
            let _ = UnregisterHotKey(Some(hwnd), id);
        }
        // Also release any dynamically allocated favorite-slot ids.
        if let Some(map) = DYNAMIC_IDS.get() {
            if let Ok(guard) = map.lock() {
                for (_, id) in guard.iter() {
                    let _ = UnregisterHotKey(Some(hwnd), *id);
                }
            }
        }
    }
    let bindings = current_bindings();
    let mut errors: Vec<String> = Vec::new();
    for (action, id, repeat) in ACTIONS {
        if let Some(combo) = bindings.get(action) {
            register_combo(hwnd, action, combo, id, repeat, &mut errors);
        }
    }
    // Favorite slots beyond the static pool register through dynamically
    // allocated ids, so any number of favorites can be bound.
    for (action, combo) in &bindings {
        if is_favorite_action(action) && !ACTIONS.iter().any(|(name, _, _)| name == action) {
            let id = dynamic_id(action);
            register_combo(hwnd, action, combo, id, false, &mut errors);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn register_combo(
    hwnd: HWND,
    action: &str,
    combo: &str,
    id: i32,
    repeat: bool,
    errors: &mut Vec<String>,
) {
    let Some((mods, vk)) = parse_hotkey(combo) else {
        // "None"/empty is the unbound marker and is skipped silently; only a
        // real, non-empty combo that fails to parse is surfaced as an error
        // (e.g. a hand-edited config entry).
        let trimmed = combo.trim();
        if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("none") {
            errors.push(format!("{action}: unrecognized key combination '{combo}'"));
        }
        return;
    };
    // "None" is stored as the unbound marker; parse_hotkey returns None for
    // it, so the branch above already skipped it, but guard anyway.
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

// ── Command: set / clear one binding ─────────────────────────────────────────

#[tauri::command]
pub fn set_hotkey(action: String, combo: String) -> Result<(), String> {
    let known = ACTIONS.iter().any(|(name, _, _)| *name == action) || is_favorite_action(&action);
    if !known {
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
    // Keep an already-active gesture bound to its snapshotted key/modifiers.
    // The timer will restore it on physical release even if this setting is
    // edited while the combo is held; do not touch the main-window timer or
    // gamma from this IPC command thread.
    sync_hold_binding();
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
        sync_hold_binding();
        let _ = register_all();
        return Err(errors);
    }
    Ok(())
}

#[tauri::command]
pub fn reset_hotkeys() -> Result<(), String> {
    if let Ok(mut bindings) = BINDINGS.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        bindings.clear();
    }
    // An active gesture keeps its key/modifier snapshot and is released by
    // the existing main-window timer; reset must not perform gamma work from
    // the IPC command thread.
    sync_hold_binding();
    register_all()
}

// ── WM_HOTKEY dispatch (called from the window subclass on the main thread) ─

pub fn handle_hotkey(id: i32) {
    let Some((action, _, _)) = ACTIONS.iter().find(|(_, hk, _)| *hk == id) else {
        // Favorite-slot actions beyond the static pool live in the dynamic id
        // map; reverse-look up the action name and dispatch normally.
        let action = DYNAMIC_IDS
            .get()
            .and_then(|map| map.lock().ok())
            .and_then(|guard| guard.iter().find(|(_, v)| **v == id).map(|(k, _)| k.clone()));
        let Some(action) = action else {
            return;
        };
        emit("hotkey-pressed", &action);
        return;
    };
    if id == HK_HOLD_COMPARE {
        apply_hold_gamma();
    } else {
        emit("hotkey-pressed", action);
    }
}

pub fn handle_timer(timer_id: usize) {
    if timer_id == HOLD_TIMER_ID {
        poll_hold_release();
    }
}

fn sync_hold_binding() {
    let combo = current_bindings().get("hold_compare").cloned();
    let Some(combo) = combo else {
        HOLD_CONFIG_VK.store(0, Ordering::Release);
        HOLD_CONFIG_MODS.store(0, Ordering::Release);
        return;
    };
    let Some((mods, vk)) = parse_hotkey(&combo) else {
        HOLD_CONFIG_VK.store(0, Ordering::Release);
        HOLD_CONFIG_MODS.store(0, Ordering::Release);
        return;
    };
    HOLD_CONFIG_MODS.store(mods, Ordering::Release);
    HOLD_CONFIG_VK.store(vk, Ordering::Release);
}

fn stop_hold_timer() {
    if let Some(hwnd) = main_hwnd() {
        unsafe {
            let _ = KillTimer(Some(hwnd), HOLD_TIMER_ID);
        }
    }
}

fn finish_hold_restore() -> bool {
    if crate::vendor_info::nvidia_reference_mode_enabled() {
        log::warn!("[hotkeys] skipped hold-to-compare gamma restore because NVIDIA reference mode is enabled");
        HOLD_ACTIVE_VK.store(0, Ordering::Release);
        HOLD_ACTIVE_MODS.store(0, Ordering::Release);
        HOLD_STATE.store(HOLD_IDLE, Ordering::Release);
        return true;
    }
    // Re-apply the CURRENT per-monitor lifts, the primary fallback, and any
    // screen-dimmer dimming to every monitor. apply_screen_gamma_ramps takes
    // the gamma write lock itself, so no guard is held here; the non-idle
    // HOLD_STATE keeps a new hold from interleaving with the restore. Each
    // device write is best-effort, the safety thread re-applies any failure
    // on its next tick.
    let (Some(desired), Some(per_monitor), Some(brightness)) = (
        CURRENT_LIFT.get(),
        PER_MONITOR_LIFTS.get(),
        SCREEN_DIM_BRIGHTNESS.get(),
    ) else {
        HOLD_ACTIVE_VK.store(0, Ordering::Release);
        HOLD_ACTIVE_MODS.store(0, Ordering::Release);
        HOLD_STATE.store(HOLD_IDLE, Ordering::Release);
        return true;
    };
    let brightness = brightness
        .lock()
        .map(|map| map.clone())
        .unwrap_or_default();
    crate::gamma::apply_screen_gamma_ramps(
        desired.as_ref(),
        per_monitor.as_ref(),
        &brightness,
        false,
    );
    HOLD_ACTIVE_VK.store(0, Ordering::Release);
    HOLD_ACTIVE_MODS.store(0, Ordering::Release);
    HOLD_STATE.store(HOLD_IDLE, Ordering::Release);
    true
}

fn restore_hold_gamma() {
    if HOLD_STATE
        .compare_exchange(
            HOLD_ACTIVE,
            HOLD_RESTORING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }
    // Keep the timer stopped before the gamma write. The state remains
    // non-idle until the write completes, so the safety worker cannot race a
    // new hold or overwrite the restored ramp in the middle of this operation.
    stop_hold_timer();
    let _ = finish_hold_restore();
}

fn apply_hold_gamma() {
    if crate::vendor_info::nvidia_reference_mode_enabled() {
        log::warn!(
            "[hotkeys] hold-to-compare unavailable because NVIDIA reference mode is enabled"
        );
        return;
    }
    let Some(desired) = CURRENT_LIFT.get() else {
        return;
    };
    let Some(per_monitor) = PER_MONITOR_LIFTS.get() else {
        return;
    };
    {
        let _gamma_guard = GAMMA_WRITE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if HOLD_STATE
            .compare_exchange(HOLD_IDLE, HOLD_ACTIVE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // Compare is only meaningful while at least one monitor has a lift;
        // with everything at 0 the linear ramp equals the current state, so a
        // no-op hold would still rewrite every ramp on release.
        let global_lift = desired.load(Ordering::Acquire);
        let any_monitor_lift = per_monitor
            .lock()
            .map(|map| map.values().any(|value| *value != 0))
            .unwrap_or(false);
        if global_lift == 0 && !any_monitor_lift {
            HOLD_STATE.store(HOLD_IDLE, Ordering::Release);
            return;
        }
        HOLD_ACTIVE_VK.store(HOLD_CONFIG_VK.load(Ordering::Acquire), Ordering::Release);
        HOLD_ACTIVE_MODS.store(HOLD_CONFIG_MODS.load(Ordering::Acquire), Ordering::Release);
        // Flatten EVERY monitor to the neutral ramp while held: the global
        // fallback covers the primary via GetDC(None), each device gets its
        // own CreateDCW ramp so secondary monitors compare too.
        let neutral = crate::gamma_ramp::build_linear_ramp();
        let mut applied = unsafe { crate::gamma_ramp::set_display_ramp(&neutral) };
        for monitor in crate::monitors::all_monitors() {
            if monitor.device_name.is_empty() {
                continue;
            }
            if !unsafe { crate::gamma_ramp::set_monitor_ramp(&monitor.device_name, &neutral) } {
                applied = false;
            }
        }
        if !applied {
            log::warn!(
                "[hotkeys] hold-to-compare could not apply the linear gamma ramp to every monitor"
            );
            HOLD_ACTIVE_VK.store(0, Ordering::Release);
            HOLD_ACTIVE_MODS.store(0, Ordering::Release);
            HOLD_STATE.store(HOLD_IDLE, Ordering::Release);
            return;
        }
    }

    // SetTimer is deliberately outside the gamma lock. This keeps the lock
    // order consistent with restoration (KillTimer first, gamma write second)
    // and avoids a future cross-thread deadlock.
    let timer_failed = match main_hwnd() {
        Some(hwnd) => unsafe {
            SetTimer(Some(hwnd), HOLD_TIMER_ID, HOLD_TIMER_INTERVAL_MS, None) == 0
        },
        None => true,
    };
    if timer_failed {
        log::warn!("[hotkeys] hold-to-compare timer could not be started");
        restore_hold_gamma();
    }
}

pub fn recover_hold_restore() {
    if HOLD_STATE.load(Ordering::Acquire) == HOLD_RESTORING {
        let _ = finish_hold_restore();
    }
}

pub fn poll_hold_release() {
    if HOLD_STATE.load(Ordering::Acquire) != HOLD_ACTIVE {
        stop_hold_timer();
        return;
    }
    let vk = HOLD_ACTIVE_VK.load(Ordering::Acquire);
    let mods = HOLD_ACTIVE_MODS.load(Ordering::Acquire);
    if vk == 0 {
        restore_hold_gamma();
        return;
    }
    // Prefer the elevated key observer when it is connected: it reads the
    // physical key state even while an elevated window (admin app or game)
    // holds the foreground, where this process's GetAsyncKeyState is
    // filtered by UIPI. When no observer answers, fall back to the local
    // read, which is exact over normal windows and degrades to a quick
    // flash over elevated ones.
    if let Some(held) = crate::key_observer::combo_held(vk, mods) {
        if !held {
            restore_hold_gamma();
        }
        return;
    }
    if !key_is_down(vk) || !modifiers_are_down(mods) {
        restore_hold_gamma();
    }
}

fn key_is_down(vk: u32) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

// Physical state of one RegisterHotKey-style combo. Used by the elevated
// key observer child, whose GetAsyncKeyState reads are not filtered by UIPI
// (both it and an elevated game run at High integrity).
pub(crate) fn combo_is_down(vk: u32, mods: u32) -> bool {
    key_is_down(vk) && modifiers_are_down(mods)
}

fn modifiers_are_down(mods: u32) -> bool {
    let control_down = key_is_down(0xA2) || key_is_down(0xA3) || key_is_down(0x11);
    let alt_down = key_is_down(0xA4) || key_is_down(0xA5) || key_is_down(0x12);
    let shift_down = key_is_down(0xA0) || key_is_down(0xA1) || key_is_down(0x10);
    let win_down = key_is_down(0x5B) || key_is_down(0x5C);
    (mods & MOD_CONTROL.0 == 0 || control_down)
        && (mods & MOD_ALT.0 == 0 || alt_down)
        && (mods & MOD_SHIFT.0 == 0 || shift_down)
        && (mods & MOD_WIN.0 == 0 || win_down)
}

fn emit(event: &str, action: &str) {
    if let Some(app) = APP.get() {
        let _ = app.emit(event, action.to_string());
    }
}

pub fn shutdown() {
    crate::key_observer::shutdown();
    if let Some(hwnd) = main_hwnd() {
        unsafe {
            for (_, id, _) in ACTIONS {
                let _ = UnregisterHotKey(Some(hwnd), id);
            }
            if let Some(map) = DYNAMIC_IDS.get() {
                if let Ok(guard) = map.lock() {
                    for (_, id) in guard.iter() {
                        let _ = UnregisterHotKey(Some(hwnd), *id);
                    }
                }
            }
        }
    }
    stop_hold_timer();
    match HOLD_STATE.load(Ordering::Acquire) {
        HOLD_ACTIVE => restore_hold_gamma(),
        HOLD_RESTORING => {
            let _ = finish_hold_restore();
        }
        _ => {}
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
    fn key_table_has_unique_names() {
        // parse_hotkey now matches case-insensitively via a linear scan, so a
        // duplicate name (or two names that differ only by case) would be
        // ambiguous and the first match would silently win.
        let names: Vec<&str> = key_table().iter().map(|(name, _)| *name).collect();
        let mut lowered: Vec<String> = names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let unique = lowered.len();
        lowered.sort();
        lowered.dedup();
        assert_eq!(unique, lowered.len(), "key names must be unique (case-insensitively)");
        assert!(!names.is_empty());
    }
    #[test]
    fn parses_modifiers_case_insensitively() {
        assert_eq!(
            parse_hotkey("ctrl+alt+f9"),
            Some((MOD_CONTROL.0 | MOD_ALT.0, 0x78))
        );
        assert_eq!(
            parse_hotkey("ctrl+SHIFT+space"),
            Some((MOD_CONTROL.0 | MOD_SHIFT.0, 0x20))
        );
        assert_eq!(parse_hotkey("CTRL+pgup"), Some((MOD_CONTROL.0, 0x21)));
    }
}
