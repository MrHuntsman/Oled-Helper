//! External gamma watchdog.
//!
//! Gamma ramps applied with SetDeviceGammaRamp persist in the display driver
//! after the process that set them exits. Graceful shutdown resets them in
//! `shutdown_gamma_worker`, but a hard kill (Task Manager, crash, power loss)
//! skips that path and leaves a lifted or dimmed ramp on screen until the app
//! is relaunched. This module mirrors the display-mode recovery watchdog
//! (display_info.rs): a tiny child process, a full copy of the exe running
//! with `--gamma-watchdog <pid>` before any Tauri/WebView2 init, watches a
//! record file and resets every monitor's ramp to linear the moment the app
//! process is gone.
//!
//! The record exists only while a non-default ramp is applied (any global
//! black lift, any per-monitor lift, or any screen-dimmer brightness below
//! full). It is written on the first such transition, refreshed on takeover
//! by a new session, and removed on graceful shutdown. The child:
//!   - exits when the record disappears (graceful shutdown),
//!   - exits when the record's pid no longer matches its own (a newer session
//!     took over and spawned its own child),
//!   - otherwise resets all ramps, removes the record, and exits once the
//!     watched pid is no longer running.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

const RECORD_FILE: &str = "oled-helper-gamma-watch.json";
// Child poll cadence: fast enough to restore a killed session's ramps within
// a couple of seconds, slow enough to be a rounding error on battery.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

#[derive(serde::Serialize, serde::Deserialize)]
struct GammaWatchRecord {
    pid: u32,
}

fn record_path() -> PathBuf {
    std::env::temp_dir().join(RECORD_FILE)
}

fn read_record() -> Option<u32> {
    let path = record_path();
    let data = std::fs::read(&path).ok()?;
    let record: GammaWatchRecord = serde_json::from_slice(&data).ok()?;
    if record.pid == 0 {
        return None;
    }
    Some(record.pid)
}

fn write_record(pid: u32) -> Result<(), String> {
    let path = record_path();
    let temp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(&GammaWatchRecord { pid })
        .map_err(|error| format!("could not serialize gamma watch state: {error}"))?;
    std::fs::write(&temp, data)
        .map_err(|error| format!("could not save gamma watch state: {error}"))?;
    std::fs::rename(&temp, &path)
        .map_err(|error| format!("could not publish gamma watch state: {error}"))
}

fn remove_record() {
    let path = record_path();
    let temp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&temp);
}

// Non-blocking liveness probe. OpenProcess failing with ERROR_INVALID_PARAMETER
// means the pid is gone; any other failure (access denied, etc.) cannot prove
// death, so err toward alive and keep polling.
#[cfg(target_os = "windows")]
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, STILL_ACTIVE,
    };
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => handle,
            Err(_) => return GetLastError() != ERROR_INVALID_PARAMETER,
        };
        let mut exit_code = 0u32;
        let alive = GetExitCodeProcess(handle, &mut exit_code).is_ok()
            && exit_code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(handle);
        alive
    }
}

#[cfg(not(target_os = "windows"))]
fn pid_alive(_pid: u32) -> bool {
    true
}

// Resets the primary display and every connected monitor to the linear ramp.
// Devices that disappeared cannot be reset; the driver restores those on
// unplug, and the next app launch re-seeds whatever the user wants.
fn reset_all_ramps() {
    unsafe {
        crate::gamma_ramp::reset_display_ramp();
    }
    for monitor in crate::monitors::all_monitors() {
        if monitor.device_name.is_empty() {
            continue;
        }
        unsafe {
            let _ = crate::gamma_ramp::reset_monitor_ramp(&monitor.device_name);
        }
    }
}

fn spawn_watchdog(record_pid: u32) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not locate the gamma watchdog: {error}"))?;
    match std::process::Command::new(&exe)
        .arg("--gamma-watchdog")
        .arg(record_pid.to_string())
        .spawn()
    {
        Ok(_) => Ok(()),
        Err(error) => Err(format!("could not start the gamma watchdog: {error}")),
    }
}

// Pid this session has already written to the record and spawned a child for.
// Only the first active transition per session performs the file write and the
// spawn; the safety loop re-syncs every 5 seconds without touching the disk.
static SPAWNED_FOR_PID: Mutex<Option<u32>> = Mutex::new(None);
static SYNC_LOCK: Mutex<()> = Mutex::new(());

// Startup: a stale record (from a session that died with a ramp applied)
// spawns a resolver child immediately, so a stuck ramp is cleared even when
// the new session never touches gamma. The child exits once it has reset and
// removed the record (or once this session takes the record over).
pub(crate) fn start() {
    let Some(pid) = read_record() else {
        return;
    };
    if let Err(error) = spawn_watchdog(pid) {
        log::warn!("[gamma-watch] could not spawn the startup resolver watchdog: {error}");
    }
}

// Reflects the current gamma intent in the watch record. Non-default state is
// edge-triggered: the record is written (and the child spawned) on the first
// active transition, so the hot slider/safety paths never touch the disk.
// Called with clones of the same maps the ramp writer used.
pub(crate) fn sync(global_lift: i32, per_monitor: &std::collections::HashMap<String, i32>, brightness: &std::collections::HashMap<String, u32>) {
    let active = global_lift != 0
        || per_monitor.values().any(|value| *value != 0)
        || brightness.values().any(|value| *value < 1000);
    let pid = std::process::id();
    let _sync_guard = SYNC_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    if !active {
        return;
    }
    let mut spawned = SPAWNED_FOR_PID.lock().unwrap_or_else(|error| error.into_inner());
    if *spawned == Some(pid) {
        return;
    }
    if let Err(error) = write_record(pid) {
        log::warn!("[gamma-watch] could not save watch state: {error}");
        return;
    }
    match spawn_watchdog(pid) {
        Ok(()) => *spawned = Some(pid),
        Err(error) => log::warn!("[gamma-watch] could not start the watchdog: {error}"),
    }
}

// Graceful shutdown: the ramps have been reset by the caller, so the record is
// removed and the child exits on its next poll.
pub(crate) fn shutdown() {
    if let Ok(mut spawned) = SPAWNED_FOR_PID.lock() {
        *spawned = None;
    }
    remove_record();
}

// The `--gamma-watchdog <pid>` entry point. Runs before any Tauri/WebView2
// init (see run() in lib.rs). Loops until the record disappears, the session
// changes, or the watched pid dies (in which case the ramps are restored).
pub(crate) fn run_watchdog() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let Some(pos) = args.iter().position(|arg| arg == "--gamma-watchdog") else {
        return;
    };
    let Some(watched_pid) = args
        .get(pos + 1)
        .and_then(|arg| arg.to_str())
        .and_then(|text| text.parse::<u32>().ok())
    else {
        return;
    };
    loop {
        match read_record() {
            // Graceful shutdown: the app removed the record itself.
            None => return,
            // A newer session rewrote the record and spawned its own child.
            Some(pid) if pid != watched_pid => return,
            Some(pid) => {
                if !pid_alive(pid) {
                    // The app died with a non-default ramp applied. Restore
                    // every monitor and clean the record so the next launch
                    // starts fresh.
                    reset_all_ramps();
                    remove_record();
                    log::warn!("[gamma-watch] restored display gamma after the app exited");
                    return;
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}
