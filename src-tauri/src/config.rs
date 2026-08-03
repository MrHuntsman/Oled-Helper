// Simple JSON config file persisted in the app's config dir (same appdata
// root as the log files, e.g. %APPDATA%/OledHelper on Windows). Currently
// stores Black Crush and Taskbar Dimmer settings so they survive restarts.
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::AppHandle;
#[cfg(not(target_os = "windows"))]
use tauri::Manager;

#[derive(Serialize, Deserialize, Clone)]
pub struct AppConfig {
    #[serde(default)]
    pub black_lift: f64,
    #[serde(default = "default_true")]
    pub black_enabled: bool,
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
    // listed first — to migrate old configs correctly, the fade-out field
    // reads the legacy key by hand in `migrate_legacy_ease_duration` below
    // instead of relying on a second #[serde(alias)] here.
    #[serde(default = "default_dimmer_ease_duration", alias = "taskbar_dimmer_ease_duration_ms")]
    pub taskbar_dimmer_fade_in_duration_ms: u64,
    #[serde(default = "default_dimmer_ease_duration")]
    pub taskbar_dimmer_fade_out_duration_ms: u64,
    #[serde(default = "default_true")]
    pub tray_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_dimmer_opacity() -> f64 {
    0.95
}

fn default_dimmer_ease_duration() -> u64 {
    250
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            black_lift: 0.0,
            black_enabled: true,
            taskbar_dimmer_enabled: false,
            taskbar_dimmer_opacity: default_dimmer_opacity(),
            taskbar_dimmer_fade_in_duration_ms: default_dimmer_ease_duration(),
            taskbar_dimmer_fade_out_duration_ms: default_dimmer_ease_duration(),
            tray_enabled: true,
        }
    }
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    // app_data_dir() was observed resolving to %APPDATA% (Roaming) rather than
    // %LOCALAPPDATA% in this Tauri version/setup, despite docs suggesting
    // otherwise — so build the Local path explicitly instead of trusting it.
    let dir = local_app_dir(app)?;
    fs::create_dir_all(&dir).map_err(|e| format!("could not create app data dir: {e}"))?;
    Ok(dir.join("config.json"))
}

#[cfg(target_os = "windows")]
fn local_app_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let localappdata = std::env::var("LOCALAPPDATA").map_err(|e| format!("LOCALAPPDATA not set: {e}"))?;
    let identifier = app.config().identifier.clone();
    Ok(PathBuf::from(localappdata).join(identifier))
}

#[cfg(not(target_os = "windows"))]
fn local_app_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))
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
        Err(_) => AppConfig::default(), // file doesn't exist yet — first run
    }
}

// Pre-split configs stored one `taskbar_dimmer_ease_duration_ms` that applied
// to both fade-in and fade-out. serde's #[serde(alias)] only lets one field
// claim a given input key, so only fade_in_duration_ms picks it up
// automatically (see the field comment in AppConfig above); this fills in
// fade_out_duration_ms from the same legacy key by hand, but only when the
// config predates the split (no explicit fade-out key present) so it never
// clobbers a value the user has already set post-split.
fn migrate_legacy_ease_duration(raw_json: &str, config: &mut AppConfig) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_json) else { return };
    let Some(obj) = value.as_object() else { return };
    if obj.contains_key("taskbar_dimmer_fade_out_duration_ms") {
        return; // already on the split format
    }
    if let Some(legacy) = obj.get("taskbar_dimmer_ease_duration_ms").and_then(|v| v.as_u64()) {
        config.taskbar_dimmer_fade_out_duration_ms = legacy;
    }
}

#[tauri::command]
pub fn save_config(app: AppHandle, config: AppConfig) -> Result<(), String> {
    // Write on a background thread and return immediately. File I/O on the
    // IPC command thread would block every other command (including the
    // high-frequency gamma ramp updates) for the duration of the write — and
    // a slow disk/OneDrive can stall it for hundreds of milliseconds. The
    // write ordering is preserved by always spawning; duplicate stale writes
    // are harmless because the frontend debounces saves after the drag
    // settles, so the final write carries the latest config.
    let path = config_path(&app)?;
    let text = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        if let Err(e) = fs::write(&path, text) {
            log::warn!("save_config: could not write {:?}: {e}", path);
        }
    });
    Ok(())
}
