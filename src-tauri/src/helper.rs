//! Privileged helper: the same executable re-entered as a short-lived
//! elevated child, driven by a registered Task Scheduler task ("Run with
//! highest privileges"). The main app stays non-elevated permanently (an
//! elevated GUI session is what breaks external low-level-hook hotkeys such
//! as Volume2 over the taskbar), while the few operations that genuinely
//! need admin run here on demand with no UAC prompt per call:
//!
//!   - the Screen Dimmer's power-request probe (powercfg /requests), which
//!     requires elevation and used to force the whole app elevated;
//!   - NVIDIA DRS writes (RTX HDR) that return NVAPI_INVALID_USER_PRIVILEGE
//!     from a standard-user process;
//!   - font file surgery (takeown/icacls on TrustedInstaller-owned files).
//!
//! Flow: the app writes helper-request.json (op + payload + request id),
//! triggers the task with `schtasks /Run` (no UAC: the task was registered
//! once with /RL HIGHEST), the child runs the op elevated, writes
//! helper-result.json with the same request id, and exits. The request id
//! keeps a stale result from a previous run from being consumed. Files live
//! in the app's user-data folder next to config.json.
//!
//! The task runs as the current user with an interactive token (schtasks
//! /Create without /RU defaults to the current user, "run only when logged
//! on"), so the child shares the user's session: it can load nvapi64.dll and
//! read session power state exactly like the old elevated app could.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

use serde::{Deserialize, Serialize};

pub(crate) const ARG_PRIVILEGED_HELPER: &str = "--privileged-helper";
pub(crate) const ARG_PRIVILEGED_KEY_OBSERVER: &str = "--privileged-key-observer";
pub(crate) const ARG_REGISTER_HELPER_TASK: &str = "--register-helper-task";
pub(crate) const HELPER_TASK_NAME: &str = "OledHelperPrivilegedHelper";

const REQUEST_FILE: &str = "helper-request.json";
const RESULT_FILE: &str = "helper-result.json";
const KEY_OBSERVER_REQUEST_FILE: &str = "key-observer-request.json";

// CREATE_NO_WINDOW: keep the helper and schtasks invisible.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// The app's user-data folder, %LOCALAPPDATA%/<identifier>. Same path
// config.rs resolves (identifier baked in from the embedded tauri.conf.json)
// so both the elevated child and the non-elevated app agree on the files
// without needing an AppHandle.
fn app_data_dir() -> Option<PathBuf> {
    #[derive(Deserialize)]
    struct Conf {
        identifier: String,
    }
    let conf: Conf = serde_json::from_str(include_str!("../tauri.conf.json")).ok()?;
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join(conf.identifier))
}

fn request_path() -> Option<PathBuf> {
    app_data_dir().map(|dir| dir.join(REQUEST_FILE))
}

fn result_path() -> Option<PathBuf> {
    app_data_dir().map(|dir| dir.join(RESULT_FILE))
}

fn key_observer_request_path() -> Option<PathBuf> {
    app_data_dir().map(|dir| dir.join(KEY_OBSERVER_REQUEST_FILE))
}

fn run_hidden(command: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new(command)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HelperRequest {
    // Operation name: "power-requests", "hdr-mode", "hdr-flags",
    // "hdr-enabled", "font-apply", "font-restore".
    pub op: String,
    // Monotonic token echoed into the result so a stale file from an earlier
    // run is never mistaken for this run's answer.
    pub request_id: u64,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub flags: Option<u32>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub primary_path: Option<String>,
    // Named pipe the resident key observer (op "key-observer") connects to.
    #[serde(default)]
    pub pipe_name: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HelperResult {
    pub request_id: u64,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

// Set after the first run_request of the process confirms the task is
// present and pointed at the running exe. Both facts are stable for the
// life of the process (the exe path of a running image cannot change, and
// re-registration only happens through ensure_registered), so the probe
// skips the schtasks query on its hot near-timeout cadence after that.
// Left unset on a failure so a later ensure_registered self-heal is
// re-checked on the next run_request.
static REGISTRATION_VERIFIED: OnceLock<()> = OnceLock::new();
// The request/result files are a single-slot protocol. Serialize all
// one-shot operations so a power probe cannot overwrite an HDR or font
// request, and vice versa.
static REQUEST_LOCK: Mutex<()> = Mutex::new(());

// ── Task registration ──────────────────────────────────────────────────

fn task_exists() -> bool {
    let Ok(output) = run_hidden("schtasks", &["/Query", "/TN", HELPER_TASK_NAME]) else {
        return false;
    };
    output.status.success()
}

// Whether the registered task launches THIS executable. The app is a
// portable exe: if the user moves it to a new folder, the task keeps
// pointing at the old path — it still exists (the name query above passes)
// but every /Run starts a stale or missing binary. schtasks /V /FO LIST
// prints the task's full "Task To Run" command verbatim, so a
// case-insensitive substring match on the current exe path is the check.
fn task_matches_current_exe_named(task_name: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(exe) = exe.to_str() else {
        return false;
    };
    let Ok(output) = run_hidden(
        "schtasks",
        &["/Query", "/TN", task_name, "/V", "/FO", "LIST"],
    ) else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
    text.contains(&exe.to_lowercase())
}

// A usable registration is both present and pointed at this exe.
fn task_registered_and_current() -> bool {
    task_exists() && task_matches_current_exe_named(HELPER_TASK_NAME)
}

// Creates the helper task. The process creating it must be elevated:
// /RL HIGHEST is refused from a standard-user token. The /TR command is
// generic for one-shot operations: the child reads helper-request.json, so
// one registration serves every one-shot operation. The observer request is
// handled first and launches a detached elevated observer child. /SC ONCE is
// only the trigger used by schtasks /Run; it has no useful automatic schedule.
fn create_task() -> Result<(), String> {
    let exe =
        std::env::current_exe().map_err(|error| format!("could not resolve app path: {error}"))?;
    let command = format!("\"{}\" {}", exe.display(), ARG_PRIVILEGED_HELPER);
    let output = run_hidden(
        "schtasks",
        &[
            "/Create",
            "/F",
            "/TN",
            HELPER_TASK_NAME,
            "/TR",
            &command,
            "/SC",
            "ONCE",
            "/ST",
            "23:59",
            "/RL",
            "HIGHEST",
        ],
    )
    .map_err(|error| format!("could not create the privileged helper task: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            "the privileged helper task could not be created".to_string()
        } else {
            stderr
        })
    }
}

// Ensures the Task Scheduler task exists. Idempotent. When the app is
// elevated (installer, legacy always-elevated session) it creates the task
// directly; otherwise it spawns a one-shot elevated child (UAC, once) that
// registers the task and exits. The caller decides when a prompt is
// acceptable: for the Screen Dimmer that is the moment the user enables it,
// for RTX HDR the first write, and for fonts the first apply.
// Non-command entry used by backend fallback paths (RTX HDR writes, font
// apply) that route through the helper. Same semantics as ensure_helper_task.
pub(crate) fn ensure_registered() -> Result<(), String> {
    // A stale task (portable exe moved) is treated as unregistered: the
    // existing task would /Run a dead path. Re-registering overwrites it
    // with the current exe location (/F). Callers are user-initiated
    // actions (enabling the dimmer, an HDR write, a font apply), so a UAC
    // prompt here is expected — unlike the background power probe.
    if task_registered_and_current() {
        return Ok(());
    }
    if crate::process::process_is_elevated() {
        return create_task();
    }
    // Elevated register child (same exe, --register-helper-task). The parent
    // keeps running; the child only creates the task and exits. A declined
    // prompt leaves the task unregistered and returns the decline message.
    crate::process::spawn_elevated(&[ARG_REGISTER_HELPER_TASK.to_string()])?;
    // The child registers within a second or two of UAC acceptance; poll
    // briefly so the caller knows the setup actually landed. The spawn
    // already reported UAC dismissal, so a missing task after that is a
    // registration failure worth surfacing.
    for _ in 0..25 {
        if task_registered_and_current() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err("the privileged helper task was not created after the elevation prompt".to_string())
}

#[tauri::command]
pub fn ensure_helper_task() -> Result<(), String> {
    ensure_registered()
}

// Maps a finished helper round trip onto the plain Result the interactive
// commands return: Ok for a successful op, the helper's error otherwise.
pub(crate) fn result_into_std(result: HelperResult) -> Result<(), String> {
    if result.ok {
        Ok(())
    } else {
        Err(result
            .error
            .unwrap_or_else(|| "the privileged helper reported a failure".to_string()))
    }
}

// Tray/UI probe: is the helper task registered and pointed at this exe?
// Surfaced as a small warning on the Screen Dimmer row when it is on but
// the helper was never set up (or points at an old location after the
// portable exe moved).
#[tauri::command]
pub fn helper_task_registered() -> bool {
    task_registered_and_current()
}

// Launches the resident key observer: writes a "key-observer" request (the
// child connects to the named pipe and serves key-state queries until the
// pipe closes) and triggers the task, then returns without waiting for any
// result. Fire and forget by design, the observer has no result. Fails fast
// (no UAC) when the task is missing or the app session is already elevated
// (where the observer is unneeded because local key reads already work).
pub(crate) fn launch_key_observer(pipe_name: &str) -> Result<(), String> {
    let _request_guard = REQUEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if crate::process::process_is_elevated() {
        return Err("the app is elevated, the key observer is unneeded".to_string());
    }
    let Some(request_file) = key_observer_request_path() else {
        return Err("app data folder is unavailable".to_string());
    };
    if let Some(dir) = request_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let request = HelperRequest {
        op: "key-observer".to_string(),
        request_id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
        mode: None,
        flags: None,
        enabled: None,
        primary_path: None,
        pipe_name: Some(pipe_name.to_string()),
    };
    let request_json = match serde_json::to_vec(&request) {
        Ok(json) => json,
        Err(error) => return Err(format!("could not serialize helper request: {error}")),
    };
    if let Err(error) = std::fs::write(&request_file, request_json) {
        return Err(format!("could not write helper request: {error}"));
    }
    // No background observer start may raise a UAC prompt, so a missing or
    // stale task is a fail-fast error. The user initiated helper setup creates
    // the task, and the observer retries on the next app start.
    if !task_registered_and_current() {
        let _ = std::fs::remove_file(&request_file);
        return Err("the privileged helper task is not registered".to_string());
    }
    let trigger = run_hidden("schtasks", &["/Run", "/TN", HELPER_TASK_NAME])
        .map_err(|error| format!("could not trigger the elevated key observer: {error}"))?;
    if !trigger.status.success() {
        let _ = std::fs::remove_file(&request_file);
        let stderr = String::from_utf8_lossy(&trigger.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "could not run the elevated key observer task".to_string()
        } else {
            stderr
        });
    }
    Ok(())
}

// Runs the registered task and waits for the result file carrying the
// matching request id. Returns the full result envelope. `timeout` bounds
// the wait so a stuck or missing helper cannot hang a caller forever.
pub(crate) fn run_request(
    op: &str,
    build_payload: impl FnOnce(&mut HelperRequest),
    timeout: std::time::Duration,
) -> Result<HelperResult, String> {
    let _request_guard = REQUEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(request_file) = request_path() else {
        return Err("app data folder is unavailable".to_string());
    };
    let Some(result_file) = result_path() else {
        return Err("app data folder is unavailable".to_string());
    };
    let Some(dir) = request_file.parent() else {
        return Err("app data folder is unavailable".to_string());
    };
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("could not create app data folder: {error}"))?;

    // The helper child re-enters the app with no Tauri logger, so it can
    // never log; the request/result files are the whole conversation.
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let mut request = HelperRequest {
        op: op.to_string(),
        request_id,
        mode: None,
        flags: None,
        enabled: None,
        primary_path: None,
        pipe_name: None,
    };
    build_payload(&mut request);
    let request_json = serde_json::to_vec(&request)
        .map_err(|error| format!("could not serialize helper request: {error}"))?;
    std::fs::write(&request_file, request_json)
        .map_err(|error| format!("could not write helper request: {error}"))?;
    // Clear any previous result before triggering, so a poll cannot match an
    // old file that happens to carry the same id after a counter reset.
    let _ = std::fs::remove_file(&result_file);

    if REGISTRATION_VERIFIED.get().is_none() {
        // Missing task, or a stale one pointing at an old location after the
        // portable exe moved. The background power probe must never raise a
        // UAC prompt, so this is a fail-fast error, not a self-heal: the
        // caller degrades gracefully (the dimmer suppresses on idle alone)
        // and the next user-initiated action re-registers via
        // ensure_registered. "not registered" doubles as the marker power.rs
        // uses to warn exactly once.
        if !task_registered_and_current() {
            let _ = std::fs::remove_file(&request_file);
            return Err("the privileged helper task is not registered".to_string());
        }
        let _ = REGISTRATION_VERIFIED.set(());
    }
    let trigger = run_hidden("schtasks", &["/Run", "/TN", HELPER_TASK_NAME])
        .map_err(|error| format!("could not trigger the privileged helper: {error}"))?;
    if !trigger.status.success() {
        let stderr = String::from_utf8_lossy(&trigger.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "could not run the privileged helper task".to_string()
        } else {
            stderr
        });
    }

    // Poll for the matching result. 50 ms cadence keeps the common power
    // probe (which finishes in a few hundred ms) snappy without burning CPU.
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(content) = std::fs::read_to_string(&result_file) {
            if let Ok(result) = serde_json::from_str::<HelperResult>(&content) {
                if result.request_id == request_id {
                    let _ = std::fs::remove_file(&request_file);
                    return Ok(result);
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            let _ = std::fs::remove_file(&request_file);
            return Err(format!(
                "the privileged helper did not answer within {}s",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

// ── Elevated child entries (dispatched in run() before any Tauri init) ──

// --register-helper-task: create the Task Scheduler task and exit. Runs
// elevated (the non-elevated app spawned this child through ShellExecuteW
// "runas"); does not read a request.
#[cfg(target_os = "windows")]
pub(crate) fn run_register_helper_task() {
    match create_task() {
        Ok(()) => std::process::exit(0),
        Err(_) => std::process::exit(1),
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn run_register_helper_task() {
    std::process::exit(0);
}

// --privileged-key-observer: the detached elevated resident observer entry.
// It receives only the app's named pipe on the command line. The one-shot
// helper task exits before this child begins its blocking pipe loop, so power,
// HDR, and font requests remain independent.
#[cfg(target_os = "windows")]
pub(crate) fn run_privileged_key_observer() {
    let pipe = std::env::args()
        .skip(1)
        .find(|arg| arg != ARG_PRIVILEGED_KEY_OBSERVER);
    if !crate::process::process_is_elevated() {
        std::process::exit(1);
    }
    if let Some(pipe) = pipe {
        crate::key_observer::run_elevated_observer(&pipe);
    }
    std::process::exit(0);
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn run_privileged_key_observer() {
    std::process::exit(0);
}

pub(crate) fn run_privileged_helper() {
    // The observer uses a separate request file because the normal helper
    // protocol is also used by the power probe. Handle it before the
    // one-shot request so the two protocols cannot overwrite each other.
    if let Some(observer_request_file) = key_observer_request_path() {
        if let Ok(content) = std::fs::read_to_string(&observer_request_file) {
            if let Ok(request) = serde_json::from_str::<HelperRequest>(&content) {
                let _ = std::fs::remove_file(&observer_request_file);
                if request.op == "key-observer" && crate::process::process_is_elevated() {
                    if let Some(pipe) = request.pipe_name.as_deref() {
                        let Ok(exe) = std::env::current_exe() else {
                            std::process::exit(1);
                        };
                        let spawned = std::process::Command::new(exe)
                            .arg(ARG_PRIVILEGED_KEY_OBSERVER)
                            .arg(pipe)
                            .creation_flags(CREATE_NO_WINDOW)
                            .spawn();
                        if spawned.is_err() {
                            std::process::exit(1);
                        }
                    }
                    std::process::exit(0);
                }
            }
        }
    }

    let Some(request_file) = request_path() else {
        std::process::exit(1);
    };
    let Some(result_file) = result_path() else {
        std::process::exit(1);
    };
    let Ok(content) = std::fs::read_to_string(&request_file) else {
        // No pending request (for example the task's harmless midnight
        // auto-run): exit quietly.
        std::process::exit(0);
    };
    let Ok(request) = serde_json::from_str::<HelperRequest>(&content) else {
        let _ = std::fs::remove_file(&request_file);
        std::process::exit(1);
    };

    // Elevation is the whole point of this child; refuse to answer from a
    // standard-user token so a misconfigured task cannot silently run the
    // operation without the rights the caller assumed it had.
    let mut result = HelperResult {
        request_id: request.request_id,
        ok: false,
        error: None,
        data: None,
    };
    if !crate::process::process_is_elevated() {
        result.error = Some("the privileged helper started without elevation".to_string());
    } else {
        match request.op.as_str() {
            "power-requests" => {
                let requests = crate::power::display_power_requests();
                result.data = serde_json::to_value(requests).ok();
                result.ok = result.data.is_some();
            }
            // HDR mode / flags are dispatched through the same
            // apply_hdr_mode the interactive commands use, so a failed
            // transition rolls back identically whether it runs in the app
            // or here.
            "hdr-mode" => match request.mode.as_deref() {
                Some(mode) => match crate::apply_hdr_mode(mode) {
                    Ok(()) => result.ok = true,
                    Err(error) => result.error = Some(error),
                },
                None => result.error = Some("hdr-mode request is missing its mode".to_string()),
            },
            "hdr-flags" => match request.flags {
                Some(flags) => match crate::vendor_info::set_rtx_hdr_flags(flags) {
                    Ok(()) => result.ok = true,
                    Err(error) => result.error = Some(error),
                },
                None => result.error = Some("hdr-flags request is missing its flags".to_string()),
            },
            "hdr-enabled" => match request.enabled {
                Some(enabled) => match crate::vendor_info::set_rtx_hdr_enabled(enabled) {
                    Ok(()) => result.ok = true,
                    Err(error) => result.error = Some(error),
                },
                None => result.error = Some("hdr-enabled request is missing its value".to_string()),
            },
            "font-apply" => match request.primary_path.as_deref() {
                Some(path) => {
                    match crate::font_replacement::apply_replacement(
                        std::path::Path::new(path),
                        None,
                    ) {
                        Ok(state) => {
                            result.data = serde_json::to_value(state).ok();
                            result.ok = result.data.is_some();
                        }
                        Err(error) => result.error = Some(error),
                    }
                }
                None => {
                    result.error =
                        Some("font-apply request is missing its primary path".to_string());
                }
            },
            "font-restore" => match crate::font_replacement::restore_originals(None) {
                Ok(()) => result.ok = true,
                Err(error) => result.error = Some(error),
            },
            other => {
                result.error = Some(format!("unknown privileged helper op '{other}'"));
            }
        }
    }

    if let Ok(json) = serde_json::to_vec(&result) {
        let _ = std::fs::File::create(&result_file).and_then(|mut file| file.write_all(&json));
    }
    let _ = std::fs::remove_file(&request_file);
    std::process::exit(0);
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn run_privileged_helper() {
    std::process::exit(0);
}
