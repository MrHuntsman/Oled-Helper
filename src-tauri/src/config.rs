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
use tauri::AppHandle;
#[cfg(target_os = "windows")]
use windows::core::PCWSTR;
#[cfg(target_os = "windows")]
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
#[cfg(not(target_os = "windows"))]
use tauri::Manager;

#[derive(Serialize, Deserialize, Clone)]
pub struct AppConfig {
    #[serde(default)]
    pub black_lift: f64,
    // New configs store one display-unit black level per refresh rate. An
    // empty/missing map is valid for legacy configs and new refresh rates
    // default to zero in the frontend.
    #[serde(default)]
    pub black_lift_profiles: HashMap<String, f64>,
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
            black_lift_profiles: HashMap::new(),
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

struct PendingConfigWrite {
    path: PathBuf,
    text: String,
    version: u64,
}

static CONFIG_WRITE_VERSION: AtomicU64 = AtomicU64::new(0);
static CONFIG_WRITE_STOP: AtomicBool = AtomicBool::new(false);
static CONFIG_WRITE_QUEUE: OnceLock<(Mutex<Option<PendingConfigWrite>>, Condvar)> = OnceLock::new();
static CONFIG_WRITE_WORKER: OnceLock<Result<Mutex<Option<std::thread::JoinHandle<()>>>, String>> = OnceLock::new();

fn replace_config_file(temp_path: &Path, path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let temp = temp_path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect::<Vec<_>>();
        let target = path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect::<Vec<_>>();
        unsafe {
            MoveFileExW(
                PCWSTR(temp.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            ).map_err(|e| format!("could not replace {path:?}: {e}"))?;
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        fs::rename(temp_path, path).map_err(|e| format!("could not replace {path:?}: {e}"))
    }
}

fn config_write_worker() {
    let Some((queue, wake)) = CONFIG_WRITE_QUEUE.get() else { return; };
    loop {
        let pending = {
            let Ok(mut guard) = queue.lock() else { return; };
            while guard.is_none() && !CONFIG_WRITE_STOP.load(Ordering::Acquire) {
                let Ok(next) = wake.wait(guard) else { return; };
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
        let Some(pending) = pending else { break; };

        // A newer snapshot may have arrived while this write was waiting for
        // the worker. Skip stale snapshots so rapid slider/mode changes cannot
        // restore an older profile map after the latest save.
        if pending.version != CONFIG_WRITE_VERSION.load(Ordering::Acquire) {
            continue;
        }

        let temp_path = pending.path.with_extension("json.tmp");
        if let Err(e) = fs::write(&temp_path, &pending.text) {
            log::warn!("save_config: could not write {:?}: {e}", temp_path);
            continue;
        }
        if let Err(e) = replace_config_file(&temp_path, &pending.path) {
            let _ = fs::remove_file(&temp_path);
            log::warn!("save_config: {e}");
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

#[tauri::command]
pub fn save_config(app: AppHandle, config: AppConfig) -> Result<(), String> {
    // Write on a background thread and return immediately. File I/O on the
    // IPC command thread would block every other command (including the
    // high-frequency gamma ramp updates) for the duration of the write — and
    // a slow disk/OneDrive can stall it for hundreds of milliseconds.
    let path = config_path(&app)?;
    let text = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    let version = CONFIG_WRITE_VERSION.fetch_add(1, Ordering::AcqRel) + 1;
    let (queue, wake) = CONFIG_WRITE_QUEUE.get_or_init(|| (Mutex::new(None), Condvar::new()));
    ensure_config_worker()?;
    if let Ok(mut pending) = queue.lock() {
        *pending = Some(PendingConfigWrite { path, text, version });
        wake.notify_one();
    } else {
        return Err("config write queue was poisoned".into());
    }
    Ok(())
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
