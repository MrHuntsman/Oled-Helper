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
    #[serde(default = "default_dimmer_ease_duration")]
    pub taskbar_dimmer_ease_duration_ms: u64,
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
            taskbar_dimmer_ease_duration_ms: default_dimmer_ease_duration(),
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
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            log::warn!("load_config: failed to parse {path:?}: {e}");
            AppConfig::default()
        }),
        Err(_) => AppConfig::default(), // file doesn't exist yet — first run
    }
}

#[tauri::command]
pub fn save_config(app: AppHandle, config: AppConfig) -> Result<(), String> {
    let path = config_path(&app)?;
    let text = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("could not write {path:?}: {e}"))
}
