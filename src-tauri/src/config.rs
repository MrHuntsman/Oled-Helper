// Simple JSON config file persisted in the app's config dir (same appdata
// root as the log files, e.g. %APPDATA%/OledHelper on Windows). Currently
// stores Black Crush and Taskbar Dimmer settings so they survive restarts.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Condvar, Mutex, OnceLock,
};
#[cfg(not(target_os = "windows"))]
use tauri::Manager;
use tauri::{AppHandle, Emitter};
#[cfg(target_os = "windows")]
use windows::core::PCWSTR;
#[cfg(target_os = "windows")]
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

// A user-saved display mode (status-bar favorites menu): resolution + refresh
// rate in one combo, e.g. "3840x2160 @ 144". Applies through the same safe
// set_resolution path as the resolution dropdown (with preferred_hz).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct FavoriteDisplayMode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}

// A monitor's render knob pair (Text tab): faux boldness and fringe
// damping, all 0..=100. The LCD filter knob was removed; its stale field
// in old configs is ignored by serde (as are the removed crosstalk /
// gamma / filter-width knobs).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct MonitorKnobs {
    #[serde(default)]
    pub boldness: u32,
    #[serde(default)]
    pub damping: u32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct AppConfig {
    #[serde(default)]
    pub black_lift: f64,
    // New configs store display-scoped keys in the form
    // `monitor-id|WIDTHxHEIGHT|HZ`. Bare numeric keys are retained only for
    // frontend migration from the previous per-Hz format.
    #[serde(default)]
    pub black_lift_profiles: HashMap<String, f64>,
    #[serde(default = "default_true")]
    pub black_enabled: bool,
    // Hide the calibration strip interface (labels, controls), right-click
    // menu on the strip. The info tooltip and fullscreen slider only hide in
    // fullscreen modes; the rest of the interface hides everywhere.
    #[serde(default)]
    pub hide_calibration_interface: bool,
    // Small gap between cells on the Black strip (right-click menu toggle).
    #[serde(default = "default_true")]
    pub black_strip_gap_enabled: bool,
    // Whether the injected Explorer font hook is active (Text rendering tab
    // toggle). Off by default: Explorer's text is left at system ClearType
    // and the hook DLL is not injected unless the user enables it.
    #[serde(default)]
    pub explorer_hook_enabled: bool,
    // Text-tab hook scope: whether the swash detour applies to every GUI
    // process (all apps, the default) or only the shell/Control Panel
    // hosts. Many GDI apps (7-Zip, WinRAR, ...) force ClearType per-font and
    // ignore the registry grayscale setting, and the hook is the only layer
    // that reaches them; games are unaffected (they don't draw GDI text, and
    // anti-cheat blocks the injection anyway).
    #[serde(default = "default_true")]
    pub explorer_hook_all_apps: bool,
    // Escape hatch for the all-apps scope: exe names (with or without the
    // .exe suffix, case-insensitive) excluded from the detour.
    #[serde(default)]
    pub explorer_hook_blocklist: Vec<String>,
    // Per-monitor rendering for the Explorer hook (Text tab): master flag plus
    // per-monitor mode assignments keyed by the monitor's stable signature
    // ("WxH|physWxphysH|dpiXxdpiY") → mode id string ("rgb", "bgr", "custom",
    // ...). Absent assignments = use the global mode.
    #[serde(default)]
    pub per_monitor_modes: HashMap<String, String>,
    // Per-monitor render knob triplets keyed by the monitor's stable
    // signature. Absent entries = no adjustment (the monitor's text is left
    // at the system defaults).
    #[serde(default)]
    pub per_monitor_knobs: HashMap<String, MonitorKnobs>,
    // Per-monitor custom subpixel centers (per-channel (x, y) fractions of a
    // pixel) for the "custom" render mode, keyed by the monitor's stable
    // signature. Absent entries = use the mode's default layout. Legacy flat
    // [r, g, b] configs are upgraded to y = 0 by the deserializer.
    #[serde(default, deserialize_with = "deserialize_subpixel_offsets")]
    pub per_monitor_custom: HashMap<String, [[f32; 2]; 3]>,
    // The monitor selected in the app's per-monitor selector: its stable
    // signature, or empty when the selector is still loading. Restored so
    // the selector keeps its position across restarts.
    #[serde(default)]
    pub active_monitor_sig: String,
    // Power tab screen protector: seconds of idle before the display fades
    // and stays dimmed until input (10-180 seconds). Enabled separately via
    // the Screen Dimmer title-section toggle.
    #[serde(default = "default_screen_protector_seconds")]
    pub screen_protector_seconds: u32,
    // Whether the screen dimmer is enabled. Off by default; configs written
    // before this flag existed are migrated (a non-zero timeout meant on).
    #[serde(default)]
    pub screen_protector_enabled: bool,
    // Screen Dimmer strength and transition durations. The field keeps its
    // historical opacity name for config compatibility (0.0 = no dim,
    // 1.0 = black); durations are ms.
    #[serde(default = "default_screen_protector_opacity")]
    pub screen_protector_opacity: f64,
    #[serde(default = "default_screen_protector_fade_in_duration")]
    pub screen_protector_fade_in_duration_ms: u64,
    #[serde(default = "default_screen_protector_fade_out_duration")]
    pub screen_protector_fade_out_duration_ms: u64,
    // Whether the Screen Dimmer's fade-in/fade-out sliders are chained (one
    // drag drives both). Persisted so the link state survives restarts.
    #[serde(default = "default_true")]
    pub screen_protector_fade_linked: bool,
    // Whether a fullscreen app on a monitor suppresses the dim on that
    // monitor (per-monitor fullscreen detection). On by default so games and
    // fullscreen video stay bright; toggling it off lets the dim run over
    // fullscreen content too.
    #[serde(default = "default_true")]
    pub screen_protector_fullscreen_detection: bool,
    // Whether the app relaunches itself elevated at startup (Settings tab
    // "Always open as administrator"). On by user choice only; a UAC prompt
    // appears before any window when set and the process isn't elevated.
    #[serde(default)]
    pub always_elevated: bool,
    // High Nits strip range: the grid is a fixed 3×7 (21 cells) with no
    // zoom; the user picks Min and Max directly (independently of each
    // other) and Step (nits between neighboring cells) is derived on the
    // frontend as (max − min) / 20. high_nits_max is optional so older
    // config files (from before Max was a stored field, when only min/step
    // were saved and max was derived) still parse; the frontend derives max
    // from min/step as a migration fallback when this is absent.
    #[serde(default = "default_high_nits_min")]
    pub high_nits_min: u32,
    #[serde(default)]
    pub high_nits_max: Option<u32>,
    #[serde(default = "default_high_nits_step")]
    pub high_nits_step: u32,
    #[serde(default)]
    pub taskbar_dimmer_enabled: bool,
    #[serde(default = "default_dimmer_opacity")]
    pub taskbar_dimmer_opacity: f64,
    // alias handles config files written before fade-in/out were split into
    // separate durations, so existing users don't silently lose their setting.
    // Both fields share the same legacy key on purpose: the old single
    // duration applied symmetrically to fade-in and fade-out, so a pre-split
    // config should seed both new fields from it. Serde's alias only binds
    // the first field that claims it per input key when deserializing a
    // struct from a map, so the JSON key would only reach whichever field is
    // listed first, to migrate old configs correctly, the fade-out field
    // reads the legacy key by hand in `migrate_legacy_ease_duration` below
    // instead of relying on a second #[serde(alias)] here.
    #[serde(
        default = "default_dimmer_ease_duration",
        alias = "taskbar_dimmer_ease_duration_ms"
    )]
    pub taskbar_dimmer_fade_in_duration_ms: u64,
    #[serde(default = "default_dimmer_ease_duration")]
    pub taskbar_dimmer_fade_out_duration_ms: u64,
    // Whether the Taskbar Dimmer's fade-in/fade-out sliders are chained.
    #[serde(default = "default_true")]
    pub taskbar_dimmer_fade_linked: bool,
    #[serde(default = "default_hover_detection")]
    pub taskbar_dimmer_hover_detection: String,
    // Whether the one-time "still running in the tray" balloon has been shown
    // already, so it only appears on the first close-to-tray ever.
    #[serde(default)]
    pub tray_notice_shown: bool,
    #[serde(default = "default_poll_interval")]
    pub taskbar_dimmer_poll_interval_ms: u64,
    #[serde(default = "default_resolution_sort")]
    pub display_menu_resolution_sort: String,
    #[serde(default = "default_resolution_group_by")]
    pub display_menu_resolution_group_by: String,
    #[serde(default = "default_true")]
    pub display_menu_show_tags: bool,
    #[serde(default = "default_refresh_rate_sort")]
    pub display_menu_refresh_rate_sort: String,
    // Global hotkey bindings: action name → combo string ("Ctrl+Alt+F9" or
    // "None" for unbound). Registered via RegisterHotKey on startup and when
    // the user changes a binding in the Hotkeys tab.
    #[serde(default)]
    pub hotkeys: HashMap<String, String>,
    // User-saved resolution@hz combos shown in the status-bar favorites menu.
    #[serde(default)]
    pub favorite_display_modes: Vec<FavoriteDisplayMode>,
}

/// Deserialize `per_monitor_custom`: accepts the current 2D shape
/// (sig → [[r_x, r_y], [g_x, g_y], [b_x, b_y]]) and upgrades the legacy flat
/// [r, g, b] shape to y = 0 so pre-2D configs keep their horizontal layout.
fn deserialize_subpixel_offsets<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, [[f32; 2]; 3]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum SubpixelOffsets {
        TwoD([[f32; 2]; 3]),
        Flat([f32; 3]),
    }

    let map = HashMap::<String, SubpixelOffsets>::deserialize(deserializer)?;
    Ok(map
        .into_iter()
        .map(|(sig, offsets)| {
            let offsets = match offsets {
                SubpixelOffsets::TwoD(offsets) => offsets,
                SubpixelOffsets::Flat([r, g, b]) => [[r, 0.0], [g, 0.0], [b, 0.0]],
            };
            (sig, offsets)
        })
        .collect())
}

fn default_true() -> bool {
    true
}
fn default_dimmer_opacity() -> f64 {
    0.95
}
fn default_screen_protector_seconds() -> u32 {
    30
}
fn default_screen_protector_opacity() -> f64 {
    0.8
}
fn default_screen_protector_fade_in_duration() -> u64 {
    1000
}
fn default_screen_protector_fade_out_duration() -> u64 {
    200
}
fn default_high_nits_min() -> u32 {
    100
}
fn default_high_nits_step() -> u32 {
    100
}
fn default_dimmer_ease_duration() -> u64 {
    300
}
fn default_hover_detection() -> String {
    "event".to_string()
}
fn default_poll_interval() -> u64 {
    100
}
fn default_resolution_sort() -> String {
    "desc".to_string()
}
fn default_resolution_group_by() -> String {
    "none".to_string()
}
fn default_refresh_rate_sort() -> String {
    "desc".to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            black_lift: 0.0,
            black_lift_profiles: HashMap::new(),
            black_enabled: true,
            hide_calibration_interface: false,
            black_strip_gap_enabled: true,
            explorer_hook_enabled: false,
            explorer_hook_all_apps: true,
            explorer_hook_blocklist: Vec::new(),
            per_monitor_modes: HashMap::new(),
            per_monitor_knobs: HashMap::new(),
            per_monitor_custom: HashMap::new(),
            active_monitor_sig: String::new(),
            screen_protector_seconds: default_screen_protector_seconds(),
            screen_protector_enabled: false,
            screen_protector_opacity: default_screen_protector_opacity(),
            screen_protector_fade_in_duration_ms: default_screen_protector_fade_in_duration(),
            screen_protector_fade_out_duration_ms: default_screen_protector_fade_out_duration(),
            screen_protector_fade_linked: true,
            screen_protector_fullscreen_detection: true,
            always_elevated: false,
            high_nits_min: default_high_nits_min(),
            high_nits_max: None,
            high_nits_step: default_high_nits_step(),
            taskbar_dimmer_enabled: false,
            taskbar_dimmer_opacity: default_dimmer_opacity(),
            taskbar_dimmer_fade_in_duration_ms: default_dimmer_ease_duration(),
            taskbar_dimmer_fade_out_duration_ms: default_dimmer_ease_duration(),
            taskbar_dimmer_fade_linked: true,
            taskbar_dimmer_hover_detection: default_hover_detection(),
            taskbar_dimmer_poll_interval_ms: default_poll_interval(),
            tray_notice_shown: false,
            display_menu_resolution_sort: default_resolution_sort(),
            display_menu_resolution_group_by: default_resolution_group_by(),
            display_menu_show_tags: true,
            display_menu_refresh_rate_sort: default_refresh_rate_sort(),
            hotkeys: HashMap::new(),
            favorite_display_modes: Vec::new(),
        }
    }
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = local_app_dir(app)?;
    fs::create_dir_all(&dir).map_err(|e| format!("could not create app data dir: {e}"))?;
    Ok(dir.join("config.json"))
}

#[cfg(target_os = "windows")]
fn local_app_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let localappdata =
        std::env::var("LOCALAPPDATA").map_err(|e| format!("LOCALAPPDATA not set: {e}"))?;
    Ok(PathBuf::from(localappdata).join(app.config().identifier.clone()))
}

#[cfg(not(target_os = "windows"))]
fn local_app_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))
}

#[tauri::command]
pub fn open_config_folder(app: AppHandle) -> Result<(), String> {
    let dir = local_app_dir(&app)?;
    fs::create_dir_all(&dir).map_err(|e| format!("could not create app data dir: {e}"))?;
    #[cfg(target_os = "windows")]
    {
        let path = dir
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        unsafe {
            windows::Win32::UI::Shell::ShellExecuteW(
                None,
                PCWSTR::null(),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
            );
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let status = std::process::Command::new("xdg-open")
            .arg(&dir)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("file manager exited with {status}"))
        }
    }
}

#[tauri::command]
pub fn load_config(app: AppHandle) -> AppConfig {
    let path = match config_path(&app) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("load_config: {e}");
            return AppConfig::default();
        }
    };
    match fs::read_to_string(&path) {
        Ok(text) => {
            let mut config = serde_json::from_str::<AppConfig>(&text).unwrap_or_else(|e| {
                log::warn!("load_config: failed to parse {path:?}: {e}");
                AppConfig::default()
            });
            migrate_legacy_ease_duration(&text, &mut config);
            migrate_legacy_screen_protector(&text, &mut config);
            migrate_screen_protector_enabled(&text, &mut config);
            config
        }
        Err(_) => AppConfig::default(),
    }
}

fn migrate_screen_protector_enabled(raw_json: &str, config: &mut AppConfig) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_json) else {
        return;
    };
    let Some(obj) = value.as_object() else { return };
    if obj.contains_key("screen_protector_enabled") {
        return;
    }
    // Configs written before the enable toggle existed: a non-zero timeout
    // meant the protector was on.
    if config.screen_protector_seconds > 0 {
        config.screen_protector_enabled = true;
    }
}

fn migrate_legacy_screen_protector(raw_json: &str, config: &mut AppConfig) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_json) else {
        return;
    };
    let Some(obj) = value.as_object() else { return };
    if obj.contains_key("screen_protector_seconds") {
        return;
    }
    if let Some(minutes) = obj
        .get("screen_protector_minutes")
        .and_then(|v| v.as_u64())
    {
        config.screen_protector_seconds = minutes.saturating_mul(60).min(180) as u32;
    }
}

fn migrate_legacy_ease_duration(raw_json: &str, config: &mut AppConfig) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_json) else {
        return;
    };
    let Some(obj) = value.as_object() else { return };
    if obj.contains_key("taskbar_dimmer_fade_out_duration_ms") {
        return;
    }
    if let Some(legacy) = obj
        .get("taskbar_dimmer_ease_duration_ms")
        .and_then(|v| v.as_u64())
    {
        config.taskbar_dimmer_fade_out_duration_ms = legacy;
    }
}

// Pre-Tauri startup probe used by run() so the Screen Dimmer can request
// elevation BEFORE any window exists (no open-then-restart flash). Mirrors
// local_app_dir's %LOCALAPPDATA%/<identifier> path without needing an
// AppHandle; the identifier is baked in from the embedded tauri.conf.json.
// Returns true when the persisted config enables the dimmer (including the
// legacy pre-toggle shape where a non-zero timeout meant on), so the caller
// can decide whether a UAC relaunch is warranted.
pub fn screen_protector_enabled_on_disk() -> bool {
    read_config_raw(|obj| {
        // The explicit toggle wins whenever it is present, including an
        // explicit false. (Falling through to the legacy heuristic on a
        // present-but-false toggle re-enabled elevation on every launch for
        // users who turned the dimmer off but kept a non-zero timeout.)
        if let Some(value) = obj.get("screen_protector_enabled") {
            return value.as_bool().unwrap_or(false);
        }
        // Legacy pre-toggle configs (no enabled key at all) used a non-zero
        // timeout to mean on.
        obj.get("screen_protector_seconds")
            .and_then(|v| v.as_u64())
            .map_or(false, |seconds| seconds > 0)
    })
}

// "Always open as administrator" startup probe: whether the Settings tab's
// flag is set in the on-disk config, checked before any window exists.
pub fn always_elevated_on_disk() -> bool {
    read_config_bool("always_elevated")
}

fn read_config_bool(key: &str) -> bool {
    read_config_raw(|obj| obj.get(key).and_then(|v| v.as_bool()).unwrap_or(false))
}

// Resolves %LOCALAPPDATA%/<identifier>/config.json (identifier baked in from
// the embedded tauri.conf.json) and hands the parsed object to `f`. Any
// missing/unreadable/invalid config yields false via the caller's fallback.
fn read_config_raw(f: impl FnOnce(&serde_json::Map<String, serde_json::Value>) -> bool) -> bool {
    #[derive(serde::Deserialize)]
    struct Conf {
        identifier: String,
    }
    let Ok(conf) = serde_json::from_str::<Conf>(include_str!("../tauri.conf.json")) else {
        return false;
    };
    let Ok(localappdata) = std::env::var("LOCALAPPDATA") else {
        return false;
    };
    let path = PathBuf::from(localappdata)
        .join(conf.identifier)
        .join("config.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let Some(obj) = value.as_object() else {
        return false;
    };
    f(obj)
}

// Pre-Tauri companion to screen_protector_enabled_on_disk(): when the startup
// UAC relaunch is declined, the Screen Dimmer is switched off on disk so the
// app never runs in the silent half-state (dimming on, media detection off).
// Writes the explicit false flag even for legacy configs whose non-zero
// timeout implied "on", so the migrate-on-load logic cannot flip it back.
// Returns true when the config was changed (was enabled and is now disabled).
pub fn screen_protector_disable_on_disk() -> bool {
    #[derive(serde::Deserialize)]
    struct Conf {
        identifier: String,
    }
    let Ok(conf) = serde_json::from_str::<Conf>(include_str!("../tauri.conf.json")) else {
        return false;
    };
    let Ok(localappdata) = std::env::var("LOCALAPPDATA") else {
        return false;
    };
    let path = PathBuf::from(localappdata)
        .join(conf.identifier)
        .join("config.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let was_enabled = obj
        .get("screen_protector_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| {
            obj.get("screen_protector_seconds")
                .and_then(|v| v.as_u64())
                .map_or(false, |seconds| seconds > 0)
        });
    if !was_enabled {
        return false;
    }
    obj.insert(
        "screen_protector_enabled".to_string(),
        serde_json::Value::Bool(false),
    );
    let Ok(out_text) = serde_json::to_string_pretty(&value) else {
        return false;
    };
    let temp = path.with_extension("json.tmp");
    if fs::write(&temp, out_text).is_err() {
        return false;
    }
    if replace_config_file(&temp, &path).is_err() {
        let _ = fs::remove_file(&temp);
        return false;
    }
    true
}

struct PendingConfigWrite {
    path: PathBuf,
    text: String,
    version: u64,
}
static CONFIG_WRITE_VERSION: AtomicU64 = AtomicU64::new(0);
static CONFIG_WRITE_STOP: AtomicBool = AtomicBool::new(false);
static CONFIG_WRITE_QUEUE: OnceLock<(Mutex<Option<PendingConfigWrite>>, Condvar)> = OnceLock::new();
static CONFIG_WRITE_WORKER: OnceLock<Result<Mutex<Option<std::thread::JoinHandle<()>>>, String>> =
    OnceLock::new();
static CONFIG_APP: OnceLock<AppHandle> = OnceLock::new();

fn emit_config_write_failed(message: &str) {
    if let Some(app) = CONFIG_APP.get() {
        let _ = app.emit("config-write-failed", message.to_string());
    }
}
fn replace_config_file(temp_path: &Path, path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let temp = temp_path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let target = path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        unsafe {
            MoveFileExW(
                PCWSTR(temp.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
            .map_err(|e| format!("could not replace {path:?}: {e}"))?;
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        fs::rename(temp_path, path).map_err(|e| format!("could not replace {path:?}: {e}"))
    }
}
fn config_write_worker() {
    let Some((queue, wake)) = CONFIG_WRITE_QUEUE.get() else {
        return;
    };
    loop {
        let pending = {
            let Ok(mut guard) = queue.lock() else {
                return;
            };
            while guard.is_none() && !CONFIG_WRITE_STOP.load(Ordering::Acquire) {
                let Ok(next) = wake.wait(guard) else {
                    return;
                };
                guard = next;
            }
            if let Some(pending) = guard.take() {
                Some(pending)
            } else if CONFIG_WRITE_STOP.load(Ordering::Acquire) {
                None
            } else {
                continue;
            }
        };
        let Some(pending) = pending else {
            break;
        };
        if pending.version != CONFIG_WRITE_VERSION.load(Ordering::Acquire) {
            continue;
        }
        let temp_path = pending.path.with_extension("json.tmp");
        if pending.version != CONFIG_WRITE_VERSION.load(Ordering::Acquire) {
            continue;
        }
        if let Err(e) = fs::write(&temp_path, &pending.text) {
            log::warn!("save_config: could not write {:?}: {e}", temp_path);
            emit_config_write_failed(&format!("could not write {:?}: {e}", temp_path));
            continue;
        }
        if let Err(e) = replace_config_file(&temp_path, &pending.path) {
            let _ = fs::remove_file(&temp_path);
            log::warn!("save_config: {e}");
            emit_config_write_failed(&format!("could not replace {:?}: {e}", pending.path));
        }
    }
}
fn ensure_config_worker() -> Result<(), String> {
    let worker = CONFIG_WRITE_WORKER.get_or_init(|| {
        std::thread::Builder::new()
            .name("config-write-worker".into())
            .spawn(config_write_worker)
            .map(Some)
            .map(Mutex::new)
            .map_err(|e| format!("could not start config write worker: {e}"))
    });
    match worker {
        Ok(_) => Ok(()),
        Err(error) => Err(error.clone()),
    }
}
fn enqueue_config_write(app: &AppHandle, config: &AppConfig) -> Result<(), String> {
    let _ = CONFIG_APP.set(app.clone());
    let path = config_path(app)?;
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    let version = CONFIG_WRITE_VERSION.fetch_add(1, Ordering::AcqRel) + 1;
    let (queue, wake) = CONFIG_WRITE_QUEUE.get_or_init(|| (Mutex::new(None), Condvar::new()));
    ensure_config_worker()?;
    if let Ok(mut pending) = queue.lock() {
        *pending = Some(PendingConfigWrite {
            path,
            text,
            version,
        });
        wake.notify_one();
    } else {
        return Err("config write queue was poisoned".into());
    }
    Ok(())
}

#[tauri::command]
pub fn save_config(app: AppHandle, config: AppConfig) -> Result<(), String> {
    enqueue_config_write(&app, &config)
}
#[tauri::command]
pub fn reset_config(app: AppHandle) -> Result<AppConfig, String> {
    let defaults = AppConfig::default();
    enqueue_config_write(&app, &defaults)?;
    Ok(defaults)
}
pub fn shutdown() {
    CONFIG_WRITE_STOP.store(true, Ordering::Release);
    if let Some((_, wake)) = CONFIG_WRITE_QUEUE.get() {
        wake.notify_all();
    }
    if let Some(Ok(worker)) = CONFIG_WRITE_WORKER.get() {
        if let Ok(mut worker) = worker.lock() {
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample_config() -> AppConfig {
        AppConfig {
            taskbar_dimmer_fade_in_duration_ms: 100,
            taskbar_dimmer_fade_out_duration_ms: 200,
            ..AppConfig::default()
        }
    }
    #[test]
    fn legacy_ease_duration_seeds_fade_out() {
        let raw = r#"{"taskbar_dimmer_ease_duration_ms": 400}"#;
        let mut config = sample_config();
        migrate_legacy_ease_duration(raw, &mut config);
        assert_eq!(config.taskbar_dimmer_fade_out_duration_ms, 400);
    }
    #[test]
    fn split_format_is_not_overwritten() {
        let raw = r#"{"taskbar_dimmer_ease_duration_ms": 400, "taskbar_dimmer_fade_out_duration_ms": 350}"#;
        let mut config = sample_config();
        migrate_legacy_ease_duration(raw, &mut config);
        assert_eq!(config.taskbar_dimmer_fade_out_duration_ms, 200);
    }
    #[test]
    fn missing_legacy_key_is_noop() {
        let raw = r#"{"black_lift": 0.5}"#;
        let mut config = sample_config();
        migrate_legacy_ease_duration(raw, &mut config);
        assert_eq!(config.taskbar_dimmer_fade_out_duration_ms, 200);
    }
    #[test]
    fn invalid_json_is_noop() {
        let mut config = sample_config();
        migrate_legacy_ease_duration("not json", &mut config);
        assert_eq!(config.taskbar_dimmer_fade_out_duration_ms, 200);
    }
    #[test]
    fn display_menu_prefs_round_trip() {
        let config = AppConfig {
            display_menu_resolution_sort: "asc".into(),
            display_menu_resolution_group_by: "aspectRatio".into(),
            display_menu_show_tags: false,
            display_menu_refresh_rate_sort: "asc".into(),
            ..AppConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.display_menu_resolution_sort, "asc");
        assert_eq!(parsed.display_menu_resolution_group_by, "aspectRatio");
        assert!(!parsed.display_menu_show_tags);
        assert_eq!(parsed.display_menu_refresh_rate_sort, "asc");
    }
    #[test]
    fn display_menu_prefs_default_when_missing() {
        let parsed: AppConfig = serde_json::from_str(r#"{"black_lift": 0.5}"#).unwrap();
        assert_eq!(parsed.display_menu_resolution_sort, "desc");
        assert_eq!(parsed.display_menu_resolution_group_by, "none");
        assert!(parsed.display_menu_show_tags);
        assert_eq!(parsed.display_menu_refresh_rate_sort, "desc");
        assert!(!parsed.hide_calibration_interface);
    }
    #[test]
    fn hide_calibration_interface_round_trip() {
        let config = AppConfig {
            hide_calibration_interface: true,
            ..AppConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.hide_calibration_interface);
    }
    #[test]
    fn high_nits_range_round_trip() {
        let config = AppConfig {
            high_nits_min: 200,
            high_nits_max: Some(1200),
            high_nits_step: 50,
            ..AppConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.high_nits_min, 200);
        assert_eq!(parsed.high_nits_max, Some(1200));
        assert_eq!(parsed.high_nits_step, 50);
    }
    #[test]
    fn high_nits_range_defaults_when_missing() {
        let parsed: AppConfig = serde_json::from_str(r#"{"black_lift": 0.5}"#).unwrap();
        assert_eq!(parsed.high_nits_min, 100);
        assert_eq!(parsed.high_nits_max, None);
        assert_eq!(parsed.high_nits_step, 100);
    }
    #[test]
    fn legacy_flat_subpixel_offsets_upgrade_to_y_zero() {
        let parsed: AppConfig =
            serde_json::from_str(r#"{"per_monitor_custom": {"sig": [0.1, 0.2, 0.3]}}"#).unwrap();
        assert_eq!(
            parsed.per_monitor_custom["sig"],
            [[0.1, 0.0], [0.2, 0.0], [0.3, 0.0]]
        );
    }
    #[test]
    fn subpixel_offsets_2d_round_trip() {
        let config = AppConfig {
            per_monitor_custom: HashMap::from([(
                "sig".to_string(),
                [[-0.375, 0.25], [-0.125, 0.0], [0.375, -0.25]],
            )]),
            ..AppConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.per_monitor_custom["sig"],
            [[-0.375, 0.25], [-0.125, 0.0], [0.375, -0.25]]
        );
    }
}
