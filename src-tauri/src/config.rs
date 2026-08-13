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
    // Hide the calibration strip interface (labels, controls) ,  right-click
    // menu on the strip. The info tooltip and fullscreen slider only hide in
    // fullscreen modes; the rest of the interface hides everywhere.
    #[serde(default)]
    pub hide_calibration_interface: bool,
    // Small gap between cells on the Black strip (right-click menu toggle).
    #[serde(default = "default_true")]
    pub black_strip_gap_enabled: bool,
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
    // listed first ,  to migrate old configs correctly, the fade-out field
    // reads the legacy key by hand in `migrate_legacy_ease_duration` below
    // instead of relying on a second #[serde(alias)] here.
    #[serde(
        default = "default_dimmer_ease_duration",
        alias = "taskbar_dimmer_ease_duration_ms"
    )]
    pub taskbar_dimmer_fade_in_duration_ms: u64,
    #[serde(default = "default_dimmer_ease_duration")]
    pub taskbar_dimmer_fade_out_duration_ms: u64,
    #[serde(default = "default_hover_detection")]
    pub taskbar_dimmer_hover_detection: String,
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

fn default_true() -> bool {
    true
}
fn default_dimmer_opacity() -> f64 {
    0.95
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
            high_nits_min: default_high_nits_min(),
            high_nits_max: None,
            high_nits_step: default_high_nits_step(),
            taskbar_dimmer_enabled: false,
            taskbar_dimmer_opacity: default_dimmer_opacity(),
            taskbar_dimmer_fade_in_duration_ms: default_dimmer_ease_duration(),
            taskbar_dimmer_fade_out_duration_ms: default_dimmer_ease_duration(),
            taskbar_dimmer_hover_detection: default_hover_detection(),
            taskbar_dimmer_poll_interval_ms: default_poll_interval(),
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
            config
        }
        Err(_) => AppConfig::default(),
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
}
