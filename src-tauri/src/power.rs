// power.rs, power saving controls: display/sleep timeouts, the screen saver,
// an instant "turn off display" action, and a screen protector that fades the
// display output after a configurable idle timeout
// (a burn-in protection nicety no other tool offers).
//
// Timeouts are read/written through powrprof.dll (PowerGetActiveScheme +
// PowerReadACValue / PowerWriteACValue). The raw value for the monitor and
// standby settings is a DWORD of seconds, so arbitrary minute values work and
// the read is locale-independent (unlike parsing `powercfg /q` output). Writes
// to the active scheme only take effect after PowerSetActiveScheme.
//
// The screen saver is controlled through the documented SystemParametersInfo
// actions (active + timeout) plus the HKCU SCRNSAVE.EXE value for which
// .scr file runs.

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use tauri::{AppHandle, Emitter};

use serde::{Deserialize, Serialize};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, LPARAM, WPARAM};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    REG_VALUE_TYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, SystemParametersInfoW, HWND_BROADCAST, SC_MONITORPOWER, SPIF_UPDATEINIFILE,
    SPI_GETSCREENSAVEACTIVE, SPI_GETSCREENSAVETIMEOUT, SPI_SETSCREENSAVEACTIVE,
    SPI_SETSCREENSAVETIMEOUT, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_SYSCOMMAND,
};

// powercfg GUIDs (the same constants powercfg uses internally). Data4 is the
// byte array in the GUID's on-disk order.
const GUID_VIDEO_SUBGROUP: GUID = GUID::from_values(
    0x7516_b95f,
    0xf776,
    0x4464,
    [0x8c, 0x53, 0x06, 0x16, 0x7f, 0x40, 0xcc, 0x99],
);
const GUID_VIDEOIDLE_TIMEOUT: GUID = GUID::from_values(
    0x3c0b_c021,
    0xc8a8,
    0x4e07,
    [0xa9, 0x73, 0x6b, 0x14, 0xcb, 0xcb, 0x2b, 0x7e],
);
const GUID_SLEEP_SUBGROUP: GUID = GUID::from_values(
    0x238c_9fa8,
    0x0aad,
    0x41ed,
    [0x83, 0xf4, 0x97, 0xbe, 0x24, 0x2c, 0x8f, 0x20],
);
const GUID_STANDBY_TIMEOUT: GUID = GUID::from_values(
    0x29f6_c1db,
    0x86da,
    0x48c5,
    [0x9f, 0xdb, 0xf2, 0xb6, 0x7b, 0x1f, 0x44, 0xda],
);

const DESKTOP_SUBKEY: &str = r"Control Panel\Desktop";
const SCRNSAVE_VALUE: &str = "SCRNSAVE.EXE";

// powrprof exports used for READS only. (There is no PowerWriteACValue/
// PowerWriteDCValue export, powrprof only exposes the *ValueIndex write
// variants, so arbitrary second values are written through `powercfg /change`
// instead.) HKEY is repr(transparent) over *mut c_void, so it is safe to pass
// a raw pointer here; the root power key is always NULL (active user).
#[link(name = "powrprof")]
unsafe extern "system" {
    fn PowerGetActiveScheme(
        user_root_power_key: *mut core::ffi::c_void,
        active_policy_guid: *mut *mut GUID,
    ) -> u32;
    fn PowerReadACValue(
        root_power_key: *mut core::ffi::c_void,
        scheme_guid: *const GUID,
        subgroup_guid: *const GUID,
        setting_guid: *const GUID,
        value_type: *mut u32,
        buffer: *mut u8,
        buffer_size: *mut u32,
    ) -> u32;
}

// kernel32 tick count for the dim-before-off idle timer.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetTickCount64() -> u64;
}

#[derive(Serialize, Clone)]
pub struct ScreensaverInfo {
    pub name: String,
    pub path: String,
}

#[derive(Serialize, Clone)]
pub struct PowerState {
    pub display_timeout_minutes: u32,
    pub sleep_timeout_minutes: u32,
    pub screensaver_enabled: bool,
    pub screensaver_timeout_minutes: u32,
    pub screensaver_path: String,
    pub screensavers: Vec<ScreensaverInfo>,
    pub screen_protector_enabled: bool,
    pub screen_protector_seconds: u32,
    pub screen_protector_opacity: f64,
    pub screen_protector_fade_in_duration_ms: u64,
    pub screen_protector_fade_out_duration_ms: u64,
    pub screen_protector_fullscreen_detection: bool,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn update_flags() -> SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS {
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(SPIF_UPDATEINIFILE.0)
}

// ── Timeouts (powrprof) ──────────────────────────────────────────────

fn active_scheme() -> Result<*mut GUID, String> {
    unsafe {
        let mut scheme: *mut GUID = std::ptr::null_mut();
        let rc = PowerGetActiveScheme(std::ptr::null_mut(), &mut scheme);
        if rc != 0 || scheme.is_null() {
            return Err(format!("PowerGetActiveScheme failed ({rc})"));
        }
        Ok(scheme)
    }
}

unsafe fn free_scheme(scheme: *mut GUID) {
    windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
        scheme as *mut core::ffi::c_void,
    )));
}

fn read_timeout_seconds(subgroup: &GUID, setting: &GUID) -> Result<u32, String> {
    unsafe {
        let scheme = active_scheme()?;
        let mut value_type: u32 = 0;
        let mut value: u32 = 0;
        let mut size: u32 = 4;
        let rc = PowerReadACValue(
            std::ptr::null_mut(),
            scheme,
            subgroup,
            setting,
            &mut value_type,
            &mut value as *mut u32 as *mut u8,
            &mut size,
        );
        free_scheme(scheme);
        if rc != 0 {
            return Err(format!("PowerReadACValue failed ({rc})"));
        }
        Ok(value)
    }
}

// Writes a timeout in minutes through `powercfg /change`, which accepts
// arbitrary minute values (0 = never) and applies to the active scheme
// immediately. Written to both AC and DC so the value holds on mains and
// battery alike (DC is simply unused on a desktop without one).
fn powercfg_change(setting: &str, minutes: u32) -> Result<(), String> {
    let run = |suffix: &str| -> Result<(), String> {
        let arg = format!("{setting}-{suffix}");
        let output = std::process::Command::new("powercfg")
            .args(["/change", &arg, &minutes.to_string()])
            // CREATE_NO_WINDOW: powercfg is a console app, so without this a
            // console window flashes on screen for every change.
            .creation_flags(0x0800_0000)
            .output()
            .map_err(|e| format!("could not run powercfg: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "powercfg /change {arg} {minutes} failed: {}",
                stderr.trim()
            ))
        }
    };
    run("ac")?;
    run("dc")
}

// ── Screen saver (SystemParametersInfo + registry) ──────────────────

fn read_screensaver_active() -> Result<bool, String> {
    unsafe {
        let mut active: i32 = 0;
        SystemParametersInfoW(
            SPI_GETSCREENSAVEACTIVE,
            0,
            Some(&mut active as *mut i32 as *mut core::ffi::c_void),
            update_flags(),
        )
        .map_err(|e| format!("SPI_GETSCREENSAVEACTIVE failed: {e}"))?;
        Ok(active != 0)
    }
}

fn set_screensaver_active(active: bool) -> Result<(), String> {
    unsafe {
        // The enable flag is passed in uiParam, NOT pvParam (some MSDN prose
        // says pvParam, but the implementation reads uiParam).
        SystemParametersInfoW(
            SPI_SETSCREENSAVEACTIVE,
            if active { 1 } else { 0 },
            None,
            update_flags(),
        )
        .map_err(|e| format!("SPI_SETSCREENSAVEACTIVE failed: {e}"))
    }
}

fn read_screensaver_timeout_seconds() -> Result<u32, String> {
    unsafe {
        let mut timeout: u32 = 0;
        SystemParametersInfoW(
            SPI_GETSCREENSAVETIMEOUT,
            0,
            Some(&mut timeout as *mut u32 as *mut core::ffi::c_void),
            update_flags(),
        )
        .map_err(|e| format!("SPI_GETSCREENSAVETIMEOUT failed: {e}"))?;
        Ok(timeout)
    }
}

fn set_screensaver_timeout_seconds(seconds: u32) -> Result<(), String> {
    unsafe {
        // SPI_SETSCREENSAVETIMEOUT takes the timeout (seconds) in uiParam,
        // NOT pvParam.
        SystemParametersInfoW(SPI_SETSCREENSAVETIMEOUT, seconds, None, update_flags())
            .map_err(|e| format!("SPI_SETSCREENSAVETIMEOUT failed: {e}"))
    }
}

fn read_string(subkey: &str, value: &str) -> Result<Option<String>, String> {
    unsafe {
        let mut key = HKEY::default();
        let open = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(subkey).as_ptr()),
            Some(0),
            KEY_QUERY_VALUE,
            &mut key,
        );
        if open == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if open != ERROR_SUCCESS || key.is_invalid() {
            return Err(format!("RegOpenKeyExW({subkey}) failed: {open:?}"));
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
        if size_query == ERROR_FILE_NOT_FOUND {
            let _ = RegCloseKey(key);
            return Ok(None);
        }
        if size_query != ERROR_SUCCESS || required == 0 || required > 1024 {
            let _ = RegCloseKey(key);
            return Err(format!(
                "RegQueryValueExW size query failed: {size_query:?}"
            ));
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
            return Err(format!("RegQueryValueExW read failed: {query:?}"));
        }
        Ok(Some(
            String::from_utf16_lossy(&buf)
                .trim_end_matches('\0')
                .to_string(),
        ))
    }
}

fn write_string(subkey: &str, value: &str, data: &str) -> Result<(), String> {
    unsafe {
        let mut key = HKEY::default();
        let create = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(subkey).as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        );
        if create != ERROR_SUCCESS || key.is_invalid() {
            return Err(format!("RegCreateKeyExW({subkey}) failed: {create:?}"));
        }
        let value_name = wide(value);
        let bytes = wide(data);
        let set = RegSetValueExW(
            key,
            PCWSTR(value_name.as_ptr()),
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                bytes.as_ptr() as *const u8,
                bytes.len() * 2,
            )),
        );
        let _ = RegCloseKey(key);
        if set != ERROR_SUCCESS {
            return Err(format!("RegSetValueExW({value}) failed: {set:?}"));
        }
        Ok(())
    }
}

// The .scr file list barely changes, and power_get_state is polled by the
// frontend every few seconds, so enumerate once and serve the cached copy.
static SCREENSAVERS: OnceLock<Vec<ScreensaverInfo>> = OnceLock::new();

fn list_screensavers() -> Vec<ScreensaverInfo> {
    SCREENSAVERS
        .get_or_init(|| {
            let mut dirs: Vec<PathBuf> = Vec::new();
            if let Ok(root) = std::env::var("SystemRoot") {
                dirs.push(PathBuf::from(&root).join("System32"));
                dirs.push(PathBuf::from(&root).join("SysWOW64"));
            }
            let mut out: Vec<ScreensaverInfo> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for dir in dirs {
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let is_scr = path
                        .extension()
                        .map(|e| e.eq_ignore_ascii_case("scr"))
                        .unwrap_or(false);
                    if !is_scr {
                        continue;
                    }
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if name.is_empty() || !seen.insert(name.clone()) {
                        continue;
                    }
                    out.push(ScreensaverInfo {
                        name,
                        path: path.to_string_lossy().to_string(),
                    });
                }
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            out
        })
        .clone()
}

// ── Instant display off ─────────────────────────────────────────────
// The display is turned off AFTER a short delay rather than immediately: the
// key-up events of the triggering hotkey (and any hand-settle input) arrive
// in the moments right after the press, and if the display is already off
// they count as fresh activity and wake it instantly. Waiting lets that input
// land while the display is still on, so the off transition is stable.
const TURN_OFF_DELAY_MS: u64 = 1000;
static TURN_OFF_PENDING: AtomicBool = AtomicBool::new(false);

fn turn_off_display_now() -> Result<(), String> {
    unsafe {
        // SC_MONITORPOWER with lParam 2 powers the display off. Posted (not
        // sent) so a busy top-level window cannot block the app on a
        // synchronous broadcast.
        PostMessageW(
            Some(HWND_BROADCAST),
            WM_SYSCOMMAND,
            WPARAM(SC_MONITORPOWER as usize),
            LPARAM(2),
        )
        .map_err(|e| format!("PostMessageW(WM_SYSCOMMAND, SC_MONITORPOWER) failed: {e}"))
    }
}

/// Restart Windows immediately. Used after a system font replacement, which
/// only takes effect at the next boot. Requires elevation (SeShutdownPrivilege
/// is granted to the elevated app, and the font apply already enforces admin).
/// EWX_FORCE skips the per-app close handshake because the frontend shows a
/// confirmation dialog first; the reason is logged as "application, planned".
#[tauri::command]
pub fn system_restart() -> Result<(), String> {
    use windows::Win32::System::Shutdown::{
        ExitWindowsEx, EWX_FORCE, EWX_REBOOT, SHTDN_REASON_FLAG_PLANNED,
        SHTDN_REASON_MAJOR_APPLICATION,
    };
    unsafe {
        ExitWindowsEx(
            EWX_REBOOT | EWX_FORCE,
            SHTDN_REASON_MAJOR_APPLICATION | SHTDN_REASON_FLAG_PLANNED,
        )
        .map_err(|e| format!("ExitWindowsEx failed: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
pub fn power_turn_off_display() -> Result<(), String> {
    // One pending turn-off at a time: repeated triggers while one is already
    // scheduled are ignored (the first will fire) rather than stacking
    // threads.
    if TURN_OFF_PENDING.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("turn-off-display".to_string())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(TURN_OFF_DELAY_MS));
            TURN_OFF_PENDING.store(false, Ordering::Release);
            if let Err(e) = turn_off_display_now() {
                log::error!("[power] turn off display failed: {e}");
            }
        })
        .map_err(|e| {
            TURN_OFF_PENDING.store(false, Ordering::Release);
            format!("could not spawn turn-off thread: {e}")
        })?;
    Ok(())
}

// ── Screen protector (dim on idle) ──────────────────────────────────
// A background watcher polls the input-idle time against a user-configured
// timeout; once the user has been idle that long, the display fades to black
// and stays dimmed until any input arrives, which fades it back up. Unlike
// the old "dim before off", this is its own protector: it does not depend on
// Windows' display-off timeout. Keyboard/mouse idle comes from a low-level
// hook (input_monitor) rather than GetLastInputInfo, because gamepad/HID
// reports reset GetLastInputInfo on some systems.
//
// The fade is rendered through per-monitor gamma ramps rather than a window
// overlay. This covers Windows shell flyouts and taskbar previews as well as
// ordinary windows. Black Crush values are composed into the same ramps so the
// screen protector restores the selected per-monitor calibration correctly.

// 60 Hz keeps the gamma fade aligned with normal display frame cadence while
// avoiding the coarse 20 Hz stepping that was visible during the transition.
const POLL_MS: u64 = 16;
const FULLSCREEN_CHECK_INTERVAL_MS: u64 = 100;
// Windows does not broadcast notifications when another process creates or
// clears a display power request, so the only way to learn the culprit is to
// query powercfg. The probe spawns a subprocess, so it only runs at the
// one-second cadence when the dimmer is close to activating or already
// dimmed (the shortest Screen Dimmer timeout is five seconds), plus on
// demand whenever a screen-dimmer state readout is open (the diagnostics
// command sets DIM_DIAGNOSTICS_PROBE on every poll). With no readout open
// and the dimmer far from a timeout, no process is spawned at all.
const DISPLAY_REQUEST_CHECK_INTERVAL_MS: u64 = 1000;
// How many seconds before the countdown reaches the timeout the powercfg
// probe starts running. While a blocker holds, the countdown is pinned just
// below the timeout, so this lead also keeps the probe alive the whole time a
// video holds its wake lock; the fresh answers are what let the dimmer tell a
// real blocker from a stale one.
const DISPLAY_PROBE_LEAD_SECONDS: u32 = 5;
// Gamepad polling cadence while the dimmer is enabled. Controllers are polled
// at a low rate (not every 16 ms tick) so the DirectInput enumeration stays
// cheap; 100 ms keeps the idle timer accurate without a busy loop.
const GAMEPAD_CHECK_INTERVAL_MS: u64 = 100;
const MIN_SCREEN_PROTECTOR_SECONDS: u32 = 5;
const MAX_SCREEN_PROTECTOR_SECONDS: u32 = 180;
// Matches ScreenDimmerProfile::default() / config default_screen_protector_seconds.
const DEFAULT_SCREEN_PROTECTOR_SECONDS: u32 = 30;
const DEFAULT_SCREEN_PROTECTOR_OPACITY: u32 = 800;
const DEFAULT_SCREEN_PROTECTOR_FADE_IN_MS: u32 = 1000;
const DEFAULT_SCREEN_PROTECTOR_FADE_OUT_MS: u32 = 200;

// The screen protector rests at a partial dim by default. The target is stored
// as the inverse dim-factor level so dim strength remains a simple 0..100%
// setting while the existing eased brightness curve stays unchanged.
static DIM_TARGET_LEVEL: AtomicU32 = AtomicU32::new(290);
static DIM_OPACITY: AtomicU32 = AtomicU32::new(DEFAULT_SCREEN_PROTECTOR_OPACITY);
static DIM_FADE_IN_MS: AtomicU32 = AtomicU32::new(DEFAULT_SCREEN_PROTECTOR_FADE_IN_MS);
static DIM_FADE_OUT_MS: AtomicU32 = AtomicU32::new(DEFAULT_SCREEN_PROTECTOR_FADE_OUT_MS);

static DIM_ENABLED: AtomicBool = AtomicBool::new(false);
static DIM_TIMEOUT_SECONDS: AtomicU32 = AtomicU32::new(30);
// Whether a fullscreen app on a monitor suppresses the dim there. Mirrors
// config screen_protector_fullscreen_detection; the watcher skips its
// per-monitor fullscreen check while this is off.
static DIM_FULLSCREEN_DETECTION: AtomicBool = AtomicBool::new(true);
static DIM_SHUTDOWN: AtomicBool = AtomicBool::new(false);

// Per-monitor screen protector settings, keyed by the monitor's stable
// identity (monitor_id || sig) in the profiles map. One entry per connected
// monitor; a monitor without an entry uses ScreenDimmerProfile::default().
// The master enabled toggle stays global (DIM_ENABLED): one switch dims
// every monitor, while each monitor keeps its own timeout, dim strength,
// fade durations, and fullscreen suppression.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenDimmerProfile {
    pub seconds: u32,
    pub opacity: f64,
    pub fade_in_duration_ms: u64,
    pub fade_out_duration_ms: u64,
    pub fullscreen_detection: bool,
}

impl Default for ScreenDimmerProfile {
    fn default() -> Self {
        // Must match config.rs's default_screen_protector_* helpers and the
        // UI defaults (SCREEN_DIMMER_DEFAULTS in stores.svelte.js).
        Self {
            seconds: 30,
            opacity: 0.8,
            fade_in_duration_ms: 1000,
            fade_out_duration_ms: 200,
            fullscreen_detection: true,
        }
    }
}

// Live snapshot of the watcher's per-tick state, published for the Debug
// tab's screen-dimmer readout. Kept in the watcher so the readout reflects
// exactly what the dimming loop decided, and the expensive powercfg probe
// cost stays inside the watcher's existing 1s cadence instead of being
// repeated on every diagnostics poll.
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenDimmerDiagnostics {
    pub enabled: bool,
    pub dimmed: bool,
    pub idle_seconds: u64,
    pub timeout_seconds: u64,
    pub fullscreen_active: bool,
    pub display_request_active: bool,
    // The named culprits (process/driver/service) holding DISPLAY power
    // requests, so the readout can point at who is keeping the screen awake.
    pub display_requests: Vec<DisplayRequestInfo>,
    pub gamepad_active: bool,
}

static DIM_DIAGNOSTICS: OnceLock<Mutex<ScreenDimmerDiagnostics>> = OnceLock::new();

fn screen_dimmer_diagnostics() -> &'static Mutex<ScreenDimmerDiagnostics> {
    DIM_DIAGNOSTICS.get_or_init(|| Mutex::new(ScreenDimmerDiagnostics::default()))
}

// App handle for emitting state-change events to the frontend readout. Set
// once at startup (mirrors display_info::set_app_handle).
static DIM_APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

pub(crate) fn set_app_handle(app: tauri::AppHandle) {
    let _ = DIM_APP_HANDLE.set(app);
}

// Fired the moment the dimmed flag flips, so an open readout (Debug or Screen
// Dimmer tab) can show the state change instantly instead of waiting for its
// next poll. The event is a notification, the readout still re-reads the full
// diagnostics snapshot on receipt.
fn emit_screen_dimmer_state_changed(dimmed: bool) {
    if let Some(handle) = DIM_APP_HANDLE.get() {
        let _ = handle.emit("screen-dimmer-state-changed", dimmed);
    }
}

// Set by the diagnostics command whenever a screen-dimmer state readout is
// open, asking the watcher to probe power requests on its next tick so the
// culprit list is fresh instead of waiting for the timer to near its end.
// Consumed by the watcher, so at most one request per command call.
static DIM_DIAGNOSTICS_PROBE: AtomicBool = AtomicBool::new(false);

// The DISPLAY power-request probe used to run inline in the dim watcher
// (one powercfg subprocess per check). It now runs in the privileged helper
// child (helper.rs) on a dedicated worker thread, because the scheduled-task
// round trip can take a few hundred milliseconds, far too slow for the
// 16 ms dim tick. The watcher only asks (fire-and-forget) and reads the
// cached answer; the worker coalesces asks to at most one helper run per
// second, preserving the old inline gate.

// Cached probe answer: when it was produced and the parsed DISPLAY list.
fn display_probe_cache() -> &'static Mutex<(std::time::Instant, Vec<DisplayRequestInfo>)> {
    static CACHE: OnceLock<Mutex<(std::time::Instant, Vec<DisplayRequestInfo>)>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new((
            std::time::Instant::now() - std::time::Duration::from_secs(60),
            Vec::new(),
        ))
    })
}

fn request_display_probe() {
    let _ = probe_worker_sender().try_send(());
}

fn probe_worker_sender() -> &'static std::sync::mpsc::SyncSender<()> {
    static SENDER: OnceLock<std::sync::mpsc::SyncSender<()>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<()>(4);
        let _ = std::thread::Builder::new()
            .name("dim-power-probe".into())
            .spawn(move || {
                // Coalescing gate: at most one helper run per second, the
                // cadence the inline probe used.
                let mut last_run = std::time::Instant::now() - std::time::Duration::from_secs(60);
                while receiver.recv().is_ok() {
                    // Drain queued asks so a burst of readout polls collapses
                    // into a single probe.
                    while receiver.try_recv().is_ok() {}
                    if last_run.elapsed()
                        < std::time::Duration::from_millis(DISPLAY_REQUEST_CHECK_INTERVAL_MS)
                    {
                        continue;
                    }
                    last_run = std::time::Instant::now();
                    let requests = run_elevated_power_probe();
                    if let Ok(mut cache) = display_probe_cache().lock() {
                        cache.0 = std::time::Instant::now();
                        cache.1 = requests;
                    }
                }
            });
        sender
    })
}

fn latest_display_requests() -> (std::time::Instant, Vec<DisplayRequestInfo>) {
    let Ok(cache) = display_probe_cache().lock() else {
        return (std::time::Instant::now(), Vec::new());
    };
    (cache.0, cache.1.clone())
}

fn clear_display_probe_cache() {
    if let Ok(mut cache) = display_probe_cache().lock() {
        cache.0 = std::time::Instant::now() - std::time::Duration::from_secs(60);
        cache.1.clear();
    }
}

// Runs one elevated power-request probe. From a legacy elevated session it
// queries powercfg in-process (no task needed); from the normal non-elevated
// session it round-trips through the scheduled-task helper child. An empty
// list is returned when the helper is unavailable (for example the user
// declined the one-time registration), so the dimmer degrades to idle-only
// suppression instead of hanging the watcher.
fn run_elevated_power_probe() -> Vec<DisplayRequestInfo> {
    if crate::process::process_is_elevated() {
        return display_power_requests();
    }
    match crate::helper::run_request("power-requests", |_| {}, std::time::Duration::from_secs(5)) {
        Ok(result) if result.ok => result
            .data
            .and_then(|data| serde_json::from_value::<Vec<DisplayRequestInfo>>(data).ok())
            .unwrap_or_default(),
        Ok(result) => {
            log::debug!("[power] helper power probe failed: {:?}", result.error);
            Vec::new()
        }
        Err(error) => {
            // A helper timeout is expected when Task Scheduler is busy or the
            // helper is unavailable. This runs on a one second cadence near a
            // dim timeout, so logging every failure creates the debug spam the
            // diagnostics setting was intended to prevent. Keep the actionable
            // missing-task warning, and only expose other failures when the
            // user explicitly enabled detailed logging.
            static UNREGISTERED_WARNED: OnceLock<()> = OnceLock::new();
            if error.contains("not registered") {
                let _ = UNREGISTERED_WARNED.get_or_init(|| {
                    log::warn!(
                        "[power] media detection is off: the privileged helper task is missing \
                         or points at an old location (the portable exe may have moved). Toggle \
                         the Screen Dimmer off and on once to re-register it."
                    );
                });
            }
            Vec::new()
        }
    }
}

// Per-monitor settings pushed by the frontend, shared by the Tauri commands
// and the watcher thread.
static SCREEN_DIMMER_PROFILES: OnceLock<Mutex<HashMap<String, ScreenDimmerProfile>>> =
    OnceLock::new();

pub(crate) fn screen_dimmer_profiles_map() -> &'static Mutex<HashMap<String, ScreenDimmerProfile>> {
    SCREEN_DIMMER_PROFILES.get_or_init(|| Mutex::new(HashMap::new()))
}

// Resolve the profile key for a monitor. A supplied key wins; otherwise fall
// back to the primary monitor's stable identity, so callers without a
// monitor selector (power_get_state, the dim preview) keep operating on the
// primary monitor.
fn resolve_profile_key(monitor_key: Option<&str>) -> String {
    if let Some(key) = monitor_key {
        let key = key.trim();
        if !key.is_empty() {
            // Callers (the main window and the tray) send the monitor's
            // current profile identity; pass it through. Topology re-keys
            // are handled by rekey_profiles on display changes, so the hot
            // slider paths never pay for a monitor enumeration.
            return key.to_string();
        }
    }
    for m in crate::monitors::all_monitors() {
        if m.primary {
            return m.profile_id;
        }
    }
    String::new()
}

// Re-key the shared profile map to the current monitor identities after a
// display-topology change (see monitors::rekey_profile_map).
pub(crate) fn rekey_profiles() {
    if let Ok(mut profiles) = screen_dimmer_profiles_map().lock() {
        crate::monitors::rekey_profile_map(&mut profiles);
    }
}

// The profile-map key for a connected monitor: its profile identity (EDID
// id, disambiguated for identical panels). Must match resolve_profile_key /
// the frontend's dimmerMonKey, or the watcher would look its own profiles
// up under a different key and fall back to the defaults for every monitor.
fn monitor_profile_key(monitor: &crate::monitors::AppMonitor) -> String {
    monitor.profile_id.clone()
}

// Tick count of the most recent gamepad input. GetLastInputInfo does not see
// controllers, so the watcher records its own timestamp whenever a polled
// device reports non-neutral input; idle_seconds() takes the earlier of the
// keyboard/mouse clock and this one.
static LAST_GAMEPAD_INPUT: AtomicU32 = AtomicU32::new(0);

struct GammaDimContext {
    // Fallback for the primary monitor while the frontend is still loading
    // the selected monitor's profile into the per-monitor table.
    desired: Arc<AtomicI32>,
    per_monitor: Arc<Mutex<HashMap<String, i32>>>,
    brightness: Arc<Mutex<HashMap<String, u32>>>,
}

static GAMMA_DIM_CONTEXT: OnceLock<GammaDimContext> = OnceLock::new();
static DIM_THREAD: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();

// Returns (keyboard/mouse idle, gamepad idle) in seconds. Both clocks are the
// 32-bit GetTickCount wrap-safe difference against their last-input tick. The
// keyboard/mouse clock comes from the low-level hook in input_monitor, not
// GetLastInputInfo, because a connected gamepad resets the system idle clock.
fn idle_breakdown() -> (u32, u32) {
    let now = unsafe { GetTickCount64() } as u32;
    // dwTime and GetTickCount64 both wrap at ~49.7 days; wrapping_sub
    // makes the difference correct across the wrap.
    let kb_mouse_idle = now.wrapping_sub(crate::input_monitor::last_kb_mouse_input_ms()) / 1000;
    let gamepad_idle = now.wrapping_sub(LAST_GAMEPAD_INPUT.load(Ordering::Acquire)) / 1000;
    (kb_mouse_idle, gamepad_idle)
}

fn idle_seconds() -> u32 {
    let (kb_mouse_idle, gamepad_idle) = idle_breakdown();
    kb_mouse_idle.min(gamepad_idle)
}

// Effective idle countdown the dim gate compares against a monitor's timeout.
// While a blocker (fullscreen app or display power request) holds, the
// countdown is pinned just below the timeout: the blocker clearing then
// restarts the wait from zero rather than dimming instantly off a stale idle
// value (a video that buffered while the user was away used to dim the moment
// its wake lock dropped). The pin also keeps a blocked monitor from ever
// satisfying the gate, so the loop body no longer tests the blockers directly.
// With no blocker the raw idle applies, clamped by the time since the last
// blocker cleared, so real input resets still win through the min().
fn dim_countdown_seconds(idle: u32, timeout: u32, blocked: bool, since_clear: u32) -> u32 {
    if blocked {
        idle.min(timeout.saturating_sub(1))
    } else {
        idle.min(since_clear)
    }
}

// One entry of the DISPLAY section emitted by powercfg /requests: the holder
// kind ([PROCESS], [DRIVER], [SERVICE]), its path or name, and the reason
// line that follows it ("Video Wake Lock", possibly localized). Round-trips
// through the privileged-helper result file (Serialize in the helper child,
// Deserialize in the app), so both sides derive both traits.
#[derive(Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayRequestInfo {
    pub kind: String,
    pub process: String,
    pub reason: String,
}

// Parse the stable DISPLAY section emitted by powercfg /requests. The section
// headings remain English even on localized Windows, while the reason text
// (for example "Video Wake Lock") and some other headings may be localized.
// Every entry (process, driver, or service) names the culprit that is asking
// Windows to keep the display available.
fn display_requests_from_powercfg_output(output: &str) -> Vec<DisplayRequestInfo> {
    let mut requests = Vec::new();
    let mut in_display = false;
    // Entry line (kind + name) waiting for its reason line to complete it.
    let mut pending: Option<(String, String)> = None;
    for line in output.lines() {
        let trimmed = line.trim().trim_start_matches('\u{feff}');
        if let Some(header) = trimmed.strip_suffix(':') {
            in_display = header.trim().eq_ignore_ascii_case("DISPLAY");
            pending = None;
            continue;
        }
        if !in_display {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.contains(']') {
            // Entry line: [PROCESS]\\Device\\...\\app.exe (path may itself
            // contain brackets, so split on the first closing bracket only).
            let end = trimmed.find(']').unwrap_or(0);
            let kind = trimmed[1..end].trim().to_string();
            let name = trimmed[end + 1..].trim().to_string();
            pending = Some((kind, name));
        } else if let Some((kind, name)) = pending.take() {
            let reason = trimmed.to_string();
            requests.push(DisplayRequestInfo {
                kind,
                process: name,
                reason,
            });
        }
        // A bare text line with no pending entry (for example the localized
        // "None" placeholder) is ignored.
    }
    requests
}

fn decode_powercfg_output(bytes: &[u8]) -> String {
    // powercfg normally writes the console code page, but some Windows
    // configurations return UTF-16 when stdout is redirected. Handle both so
    // the detector does not depend on the user's console/output settings.
    if bytes.starts_with(&[0xff, 0xfe]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).replace('\0', "")
}

// There is no documented event that notifies an unrelated process when it
// creates or clears a power request. `powercfg /requests` is the supported
// diagnostic surface, so use it as a conditional, low-rate fallback rather
// than polling continuously. Capturing output and suppressing the console
// window keeps this invisible to the user. pub(crate): the privileged helper
// child (helper.rs) runs this in-process while elevated.
pub(crate) fn display_power_requests() -> Vec<DisplayRequestInfo> {
    // /requests and the native request-list query both require elevation on
    // current Windows builds. The Screen Dimmer asks for elevation before it
    // can be enabled, so reaching this branch as a standard-user process is
    // only possible for a legacy config; fail quietly instead of spawning a
    // command that can never succeed.
    if !crate::process::process_is_elevated() {
        return Vec::new();
    }
    let executable = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("powercfg.exe"))
        .unwrap_or_else(|| PathBuf::from("powercfg.exe"));
    let output = std::process::Command::new(&executable)
        .args(["/requests"])
        .creation_flags(0x0800_0000)
        .output();
    let Ok(output) = output else {
        log::debug!("[power] could not execute powercfg /requests");
        return Vec::new();
    };
    let stdout = decode_powercfg_output(&output.stdout);
    let stderr = decode_powercfg_output(&output.stderr);
    let mut requests = display_requests_from_powercfg_output(&stdout);
    requests.extend(display_requests_from_powercfg_output(&stderr));
    if !output.status.success() {
        log::debug!(
            "[power] powercfg /requests exited with {}; stdout={:?}; stderr={:?}",
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }

    // Some vendor-customized powercfg builds accept the dash spelling but
    // return exit code 1 for the slash spelling. Only retry when the first
    // invocation did not produce a display match, so the normal path still
    // launches at most one process per check.
    if requests.is_empty() && !output.status.success() {
        let fallback = std::process::Command::new(&executable)
            .args(["-requests"])
            .creation_flags(0x0800_0000)
            .output();
        if let Ok(fallback) = fallback {
            let fallback_stdout = decode_powercfg_output(&fallback.stdout);
            let fallback_stderr = decode_powercfg_output(&fallback.stderr);
            requests = display_requests_from_powercfg_output(&fallback_stdout);
            requests.extend(display_requests_from_powercfg_output(&fallback_stderr));
            if !fallback.status.success() {
                log::debug!(
                    "[power] powercfg -requests exited with {}; stdout={:?}; stderr={:?}",
                    fallback.status,
                    fallback_stdout.trim(),
                    fallback_stderr.trim()
                );
            }
        }
    }
    requests
}

// Map the linear dim level (0..1000) to an eased brightness factor. The raw
// level advances in equal steps, but a straight linear mapping looks steppy and
// jumps at the ends; smoothstep (ease-in-out) starts and settles gently and
// moves fastest through the middle, which reads as one fluid transition.
fn dim_factor(level: u32) -> f32 {
    let t = (level.min(1000) as f32) / 1000.0;
    t * t * (3.0 - 2.0 * t)
}

// Convert the requested dim strength to the level used by the eased
// brightness curve. A short binary search avoids introducing a second easing
// curve and keeps the displayed percentage numerically consistent.
fn dim_level_for_opacity(opacity: f64) -> u32 {
    let target_brightness = (1.0 - opacity.clamp(0.0, 1.0)) as f32;
    let mut low = 0u32;
    let mut high = 1000u32;
    while low < high {
        let mid = low + (high - low) / 2;
        if dim_factor(mid) < target_brightness {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low.min(1000)
}

// Interpolate the level against elapsed time rather than advancing by a
// fixed integer step. This keeps the configured duration accurate and avoids
// visible stalls/jumps when the scheduler wakes a little late.
fn interpolated_level(from: u32, target: u32, elapsed_ms: u128, duration_ms: u32) -> u32 {
    if from == target || duration_ms == 0 {
        return target;
    }
    let progress = (elapsed_ms as f64 / duration_ms as f64).clamp(0.0, 1.0);
    (from as f64 + (target as f64 - from as f64) * progress).round() as u32
}

// Write one monitor's screen protector profile (resolved from the optional
// monitor key, primary fallback) into the shared profiles map, then re-sync
// the legacy flat statics from the primary monitor's entry so
// power_get_state (the no-selector status read) keeps returning real values.
fn write_screen_protector_profile(
    monitor_key: Option<&str>,
    seconds: u32,
    opacity: f64,
    fade_in_duration_ms: u64,
    fade_out_duration_ms: u64,
    fullscreen_detection: bool,
) {
    let opacity = opacity.clamp(0.0, 1.0);
    let key = resolve_profile_key(monitor_key);
    {
        let mut profiles = screen_dimmer_profiles_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = profiles.entry(key).or_default();
        entry.seconds = seconds.clamp(MIN_SCREEN_PROTECTOR_SECONDS, MAX_SCREEN_PROTECTOR_SECONDS);
        entry.opacity = opacity;
        entry.fade_in_duration_ms = fade_in_duration_ms.min(5000);
        entry.fade_out_duration_ms = fade_out_duration_ms.min(5000);
        entry.fullscreen_detection = fullscreen_detection;
    }
    sync_primary_mirror();
}

// Mirror the primary monitor's profile into the flat statics that
// power_get_state and the dim preview read. Keeps the no-selector paths
// (TabPower's status poll, the Debug-tab preview) operating on the primary.
fn sync_primary_mirror() {
    let key = resolve_profile_key(None);
    let profile = screen_dimmer_profiles_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
        .unwrap_or_default();
    DIM_TIMEOUT_SECONDS.store(profile.seconds, Ordering::Release);
    DIM_OPACITY.store((profile.opacity * 1000.0).round() as u32, Ordering::Release);
    DIM_TARGET_LEVEL.store(dim_level_for_opacity(profile.opacity), Ordering::Release);
    DIM_FADE_IN_MS.store(profile.fade_in_duration_ms as u32, Ordering::Release);
    DIM_FADE_OUT_MS.store(profile.fade_out_duration_ms as u32, Ordering::Release);
    DIM_FULLSCREEN_DETECTION.store(profile.fullscreen_detection, Ordering::Release);
}

/*
// A fullscreen WS_POPUP window with WS_EX_LAYERED (alpha-driven), plus
// WS_EX_TRANSPARENT + WS_EX_NOACTIVATE + HTTRANSPARENT so it never steals
// input, any physical mouse/keyboard input still updates the system idle
// clock, which is what restores the screen. Created on the main thread during
// setup so the app's message loop pumps its WM_PAINT; every call the watcher
// thread makes on it (SetLayeredWindowAttributes, SetWindowPos, ShowWindow,
// InvalidateRect) is thread-safe.
static OVERLAY_HWND: AtomicIsize = AtomicIsize::new(0);

fn overlay_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn overlay_class_name() -> &'static [u16] {
    static NAME: OnceLock<Vec<u16>> = OnceLock::new();
    NAME.get_or_init(|| overlay_wide("OledHelperScreenProtector"))
}

struct ShellFlyoutSearch {
    found: Option<HWND>,
}

unsafe extern "system" fn enum_shell_flyout(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam.0 as *mut ShellFlyoutSearch);
    if search.found.is_some() || !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut class_buf = [0u16; 128];
    let class_len = GetClassNameW(hwnd, &mut class_buf);
    if class_len <= 0 {
        return BOOL(1);
    }
    let class = String::from_utf16_lossy(&class_buf[..class_len as usize]);
    let mut title_buf = [0u16; 256];
    let title_len = GetWindowTextW(hwnd, &mut title_buf);
    let title = if title_len > 0 {
        String::from_utf16_lossy(&title_buf[..title_len as usize])
    } else {
        String::new()
    };
    let taskbar_owner = GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT);
    let shell_flyout = class == "TaskListThumbnailWnd"
        || class == "XamlExplorerHostIslandWindow"
        || (class == "Windows.UI.Core.CoreWindow"
            && (taskbar_owner != 0
                || title.contains("Notification Center")
                || title.contains("Control Center")
                || title.contains("Quick Settings")
                || title.contains("Calendar")));
    if shell_flyout {
        search.found = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

// Windows 11 shell flyouts can be above the ordinary topmost band. Detect the
// real HWND and temporarily make the screen protector its owned popup; owned
// popups are ordered above their owner. The fallback leaves the protector
// unowned, avoiding the taskbar dimmer's deliberate "behind shell menus"
// relationship.
fn shell_flyout_hwnd() -> Option<HWND> {
    let mut search = ShellFlyoutSearch { found: None };
    unsafe {
        let _ = EnumWindows(
            Some(enum_shell_flyout),
            LPARAM((&mut search as *mut ShellFlyoutSearch) as isize),
        );
    }
    search.found
}

unsafe extern "system" fn overlay_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // Click-through: never intercept mouse input.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut paint);
            let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
            FillRect(hdc, &paint.rcPaint, black);
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn create_overlay() -> Result<(), String> {
    if OVERLAY_HWND.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    let class = WNDCLASSW {
        lpfnWndProc: Some(overlay_window_proc),
        lpszClassName: PCWSTR(overlay_class_name().as_ptr()),
        ..Default::default()
    };
    unsafe {
        // A second registration simply fails and leaves the existing class
        // valid.
        let _ = RegisterClassW(&class);
    }
    let title = overlay_wide("OLED Helper Screen Protector");
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED
                | WS_EX_TRANSPARENT
                | WS_EX_TOOLWINDOW
                | WS_EX_NOACTIVATE
                | WS_EX_TOPMOST,
            PCWSTR(overlay_class_name().as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            Some(std::ptr::null::<c_void>()),
        )
    }
    .map_err(|e| format!("could not create screen protector overlay: {e}"))?;
    OVERLAY_HWND.store(hwnd.0 as isize, Ordering::Release);
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    Ok(())
}

// Bounding rect of the entire virtual desktop (all monitors).
fn virtual_screen_rect() -> RECT {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        RECT {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        }
    }
}

*/
// Apply the current screen-protector level through the per-monitor gamma
// ramps. 1000 is full brightness; lower values multiply the existing Black
// Crush ramp so the calibration is preserved while every monitor is dimmed.
fn apply_dim_levels(levels: &HashMap<String, u32>, restore_unconfigured: bool) {
    if let Some(context) = GAMMA_DIM_CONTEXT.get() {
        if let Ok(mut brightness) = context.brightness.lock() {
            *brightness = levels.clone();
        }
        crate::gamma::apply_screen_gamma_ramps(
            &context.desired,
            &context.per_monitor,
            levels,
            restore_unconfigured,
        );
    }
}

fn uniform_dim_levels(level: u32) -> HashMap<String, u32> {
    // Keyed by the device name (unique per physical monitor), matching the
    // dim watcher's per-tick maps.
    crate::monitors::all_monitors()
        .into_iter()
        .filter(|monitor| !monitor.device_name.is_empty())
        .map(|monitor| (monitor.device_name, level))
        .collect()
}

fn apply_uniform_dim_level(level: u32) {
    let levels = uniform_dim_levels(level);
    apply_dim_levels(&levels, true);
}

/*

fn overlay_needs_zorder_repair(hwnd: HWND) -> bool {
    unsafe { GetTopWindow(None).map(|top| top != hwnd).unwrap_or(true) }
}

fn repair_overlay_zorder() {
    let raw = OVERLAY_HWND.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    // When a shell flyout is open, make the protector an owned popup of that
    // exact HWND. When it closes, remove the owner again; keeping Shell_TrayWnd
    // as a permanent owner is what makes the taskbar dimmer intentionally
    // yield to shell-owned popups.
    let desired_owner = shell_flyout_hwnd().map(|flyout| flyout.0 as isize).unwrap_or(0);
    unsafe {
        if GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT) != desired_owner {
            let _ = SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, desired_owner);
        }
    }
    if !overlay_needs_zorder_repair(hwnd) {
        return;
    }
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS(
                SWP_NOMOVE.0 | SWP_NOSIZE.0 | SWP_NOACTIVATE.0 | SWP_SHOWWINDOW.0,
            ),
        );
    }
}

// ── Cursor hiding ───────────────────────────────────────────────────
// The overlay is a window, so the system cursor still renders on top of it at
// full brightness, a static bright cursor on the dimmed screen is exactly the
// burn-in hazard the protector exists to prevent.
//
// ── Cursor hiding ───────────────────────────────────────────────────
// The cursor is rendered above ordinary windows, including our overlay. A
// transparent topmost overlay therefore cannot cover it. SetSystemCursor is
// the reliable global mechanism, but it consumes the cursor handle passed to
// it, so a fresh transparent cursor must be created for EVERY cursor shape.
// Reapply periodically because Windows/apps may reload the system cursors while
// the protector is holding the dim.
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
static CURSOR_LAST_REFRESH_MS: AtomicU64 = AtomicU64::new(0);
const CURSOR_REFRESH_MS: u64 = 250;
const SYSTEM_CURSOR_IDS: [SYSTEM_CURSOR_ID; 16] = [
    OCR_NORMAL, OCR_IBEAM, OCR_WAIT, OCR_CROSS, OCR_UP, SYSTEM_CURSOR_ID(OCR_SIZE),
    SYSTEM_CURSOR_ID(OCR_ICON), OCR_SIZENWSE, OCR_SIZENESW, OCR_SIZEWE, OCR_SIZENS, OCR_SIZEALL,
    OCR_NO, OCR_HAND, OCR_APPSTARTING, OCR_HELP,
];

fn make_invisible_cursor() -> Option<HCURSOR> {
    // 1x1 monochrome cursor: an all-ones AND mask and zero XOR mask leave
    // the screen unchanged, making the cursor pixel transparent. Each mask
    // scanline is DWORD-aligned, hence four bytes for the 1-pixel row.
    let and_mask = [0xFFu8, 0, 0, 0];
    let xor_mask = [0u8; 4];
    unsafe {
        CreateCursor(
            None,
            0,
            0,
            1,
            1,
            and_mask.as_ptr() as *const c_void,
            xor_mask.as_ptr() as *const c_void,
        )
        .ok()
    }
}

fn replace_system_cursors() {
    for id in SYSTEM_CURSOR_IDS {
        // SetSystemCursor destroys/consumes its HCURSOR argument. Do not reuse
        // one handle for multiple IDs, or only the first replacement succeeds.
        if let Some(invisible) = make_invisible_cursor() {
            unsafe {
                let _ = SetSystemCursor(invisible, id);
            }
        }
    }
}

fn restore_system_cursors() {
    // Reload the user's configured cursor files without writing any settings.
    // This also restores custom cursor themes, not only the standard arrow.
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETCURSORS,
            0,
            None,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
}

fn sync_cursor(level: u32) {
    if level < 1000 {
        let now = unsafe { GetTickCount64() };
        let last = CURSOR_LAST_REFRESH_MS.load(Ordering::Acquire);
        if !CURSOR_HIDDEN.swap(true, Ordering::AcqRel)
            || now.wrapping_sub(last) >= CURSOR_REFRESH_MS
        {
            replace_system_cursors();
            CURSOR_LAST_REFRESH_MS.store(now, Ordering::Release);
        }
    } else if CURSOR_HIDDEN.swap(false, Ordering::AcqRel) {
        restore_system_cursors();
        CURSOR_LAST_REFRESH_MS.store(0, Ordering::Release);
    }
}

// Restore the cursor unconditionally on shutdown. This is also safe if the
// app is stopped while the overlay is still dimmed.
fn ensure_cursor_visible() {
    if CURSOR_HIDDEN.swap(false, Ordering::AcqRel) {
        restore_system_cursors();
    }
    CURSOR_LAST_REFRESH_MS.store(0, Ordering::Release);
}

*/
fn dim_watcher_loop() {
    let mut levels: HashMap<String, u32> = HashMap::new();
    let mut transitions: HashMap<String, (u32, u32, std::time::Instant)> = HashMap::new();
    let mut monitors: Vec<crate::monitors::AppMonitor> = Vec::new();
    let mut fullscreen_by_monitor: HashMap<String, bool> = HashMap::new();
    let mut display_request_active = false;
    let mut display_requests: Vec<DisplayRequestInfo> = Vec::new();
    let mut last_published_dimmed = false;
    let mut last_monitor_refresh = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let mut last_fullscreen_check =
        std::time::Instant::now() - std::time::Duration::from_millis(FULLSCREEN_CHECK_INTERVAL_MS);
    let mut last_gamepad_check =
        std::time::Instant::now() - std::time::Duration::from_millis(GAMEPAD_CHECK_INTERVAL_MS);
    let mut last_idle_diag = std::time::Instant::now() - std::time::Duration::from_secs(5);
    let mut gamepad_active = false;
    // Per-monitor re-baselined countdowns from the previous tick, consumed by
    // the request-probe gate below and the diagnostics snapshot.
    let mut dim_idle_baseline: HashMap<String, u64> = HashMap::new();
    // When each monitor's blocker last held. Updated on every blocked tick, so
    // the countdown restarts from the last blocked tick when the blocker
    // clears; absent for a monitor that was never blocked.
    let mut blocker_last_seen: HashMap<String, std::time::Instant> = HashMap::new();

    while !DIM_SHUTDOWN.load(Ordering::Acquire) {
        let enabled = DIM_ENABLED.load(Ordering::Acquire);
        let now = std::time::Instant::now();
        if monitors.is_empty()
            || last_monitor_refresh.elapsed() >= std::time::Duration::from_secs(1)
        {
            monitors = crate::monitors::all_monitors();
            last_monitor_refresh = now;
            fullscreen_by_monitor.clear();
        }
        // Snapshot the per-monitor profiles once per tick. Each monitor uses
        // its own timeout, dim strength, fade durations, and fullscreen
        // suppression; a monitor without an entry falls back to the defaults.
        let profiles = screen_dimmer_profiles_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        if enabled
            && last_gamepad_check.elapsed()
                >= std::time::Duration::from_millis(GAMEPAD_CHECK_INTERVAL_MS)
        {
            let active = crate::gamepad::gamepad_input_active();
            if active != gamepad_active {
                log::debug!(
                    "[power] gamepad input {}",
                    if active { "active" } else { "cleared" }
                );
                gamepad_active = active;
            }
            if active {
                LAST_GAMEPAD_INPUT.store(unsafe { GetTickCount64() } as u32, Ordering::Release);
            }
            last_gamepad_check = now;
        }

        if enabled
            && last_fullscreen_check.elapsed().as_millis() as u64 >= FULLSCREEN_CHECK_INTERVAL_MS
        {
            // Reuse the Taskbar Dimmer's fullscreen geometry/style detector,
            // but evaluate it independently for every connected monitor.
            // When fullscreen detection is disabled the map stays cleared,
            // so no monitor ever reports fullscreen and the dim runs over
            // fullscreen content like any other.
            fullscreen_by_monitor.clear();
            for monitor in &monitors {
                // Per-monitor fullscreen suppression: only monitors whose
                // profile keeps detection on are checked, the rest stay
                // absent from the map so the dim runs over fullscreen
                // content like any other. Keyed by the device name (unique
                // per physical monitor): the sig is shared by two identical
                // panels, so keying by it would make one panel's fullscreen
                // suppress the dim on its sibling.
                let profile = profiles
                    .get(&monitor_profile_key(monitor))
                    .cloned()
                    .unwrap_or_default();
                if profile.fullscreen_detection && !monitor.device_name.is_empty() {
                    fullscreen_by_monitor.insert(
                        monitor.device_name.clone(),
                        crate::taskbar_dimmer::fullscreen_active_on_monitor(monitor.hmonitor),
                    );
                }
            }
            last_fullscreen_check = now;
        }

        // The per-monitor countdowns are computed later in this loop (the
        // monitor pass); this tick's probe gate and readout consume the
        // previous tick's map, which is at most 16 ms stale.
        let idle = idle_seconds();
        let currently_dimmed = levels.values().any(|level| *level < 1000);
        // The powercfg request probe is system-wide, so it only needs to
        // know whether ANY monitor is close to its own dim timeout. Use the
        // shortest effective timeout across connected monitors, with the same
        // floor the dim gate applies so the two thresholds agree.
        let min_timeout = monitors
            .iter()
            .filter(|m| !m.device_name.is_empty())
            .map(|m| {
                profiles
                    .get(&monitor_profile_key(m))
                    .map(|p| p.seconds)
                    .unwrap_or(DEFAULT_SCREEN_PROTECTOR_SECONDS)
                    .max(MIN_SCREEN_PROTECTOR_SECONDS)
            })
            .min()
            .unwrap_or(DEFAULT_SCREEN_PROTECTOR_SECONDS);

        // Low-rate diagnostic so a stuck-bright dimmer can be traced to the
        // keyboard/mouse clock vs the gamepad clock vs fullscreen/requests.
        // Gated behind Detailed logging: it repeats every 5 seconds and would
        // otherwise flood the Debug tab and file log during normal use (the
        // same treatment as the periodic vendor/EDID diagnostics).
        if enabled
            && crate::detailed_logging_enabled()
            && last_idle_diag.elapsed() >= std::time::Duration::from_secs(5)
        {
            let (kb_mouse_idle, gamepad_idle) = idle_breakdown();
            log::debug!(
                "[power] idle={}s kb/mouse={}s gamepad={}s gamepad_active={} fullscreen={} request={}",
                idle,
                kb_mouse_idle,
                gamepad_idle,
                gamepad_active,
                fullscreen_by_monitor.values().any(|f| *f),
                display_request_active
            );
            last_idle_diag = now;
        }
        // Gate on the re-baselined countdowns: a blocker pins them just
        // below the timeout, so the probe keeps running while a video holds
        // the wake lock and the user stays away, instead of arming only one
        // second before an unblocked crossing (which let a stale empty probe
        // answer dim the screen during the helper round trip). The lead gives
        // a slow helper round trip time to land before the countdown crosses.
        let request_check_needed = enabled
            && (dim_idle_baseline.values().any(|countdown| {
                countdown.saturating_add(u64::from(DISPLAY_PROBE_LEAD_SECONDS))
                    >= u64::from(min_timeout)
            }) || currently_dimmed);
        // A readout is open: the diagnostics command sets this on every poll,
        // so the culprit appears the moment it exists instead of waiting for
        // the timer to near its end. With no readout open the flag stays
        // false and only the near-timeout cadence below spawns a probe. Both
        // paths share the one-second elapsed gate, so faster readout polling
        // never spawns more than one subprocess per second.
        let readout_probe_requested = DIM_DIAGNOSTICS_PROBE.swap(false, Ordering::AcqRel);
        if !enabled {
            // Nothing is dimming, so no probe is wanted. Clear the cached
            // culprit list so a later enable starts from "no requests"
            // instead of replaying a stale playback state.
            display_request_active = false;
            display_requests.clear();
            clear_display_probe_cache();
        } else if readout_probe_requested || request_check_needed {
            // Ask the worker to refresh the cached DISPLAY request list and
            // read whatever it last produced. The worker coalesces asks to
            // one elevated helper run per second, so this stays cheap even
            // though the watcher ticks every 16 ms. Near a timeout the first
            // fresh answer lands within the helper round trip (a few hundred
            // ms), well inside the 5 s minimum Screen Dimmer timeout.
            request_display_probe();
            let (_, requests) = latest_display_requests();
            let request_active = !requests.is_empty();
            if request_active != display_request_active {
                log::info!(
                    "[power] display power request {}",
                    if request_active { "active" } else { "cleared" }
                );
            }
            display_request_active = request_active;
            display_requests = requests;
        }

        let mut next_levels = HashMap::new();
        let mut seen = std::collections::HashSet::new();

        for monitor in &monitors {
            if monitor.device_name.is_empty() {
                continue;
            }
            // Per-tick state keyed by the device name (unique per physical
            // monitor), so two identical panels keep independent dim levels,
            // fades, and fullscreen suppression.
            let key = monitor.device_name.clone();
            seen.insert(key.clone());
            // This monitor's own profile: its timeout, dim strength, fade
            // durations, and fullscreen suppression. Looked up by the same
            // key the frontend writes (monitor_id || sig); missing = defaults.
            let profile = profiles
                .get(&monitor_profile_key(monitor))
                .cloned()
                .unwrap_or_default();
            let current = levels.get(&key).copied().unwrap_or(1000);
            let fullscreen = fullscreen_by_monitor.get(&key).copied().unwrap_or(false);
            // Re-baselined countdown for this monitor (dim_countdown_seconds):
            // a blocked monitor is pinned below its timeout, so it cannot dim
            // while the blocker holds, and the wait restarts at zero when the
            // blocker clears instead of dimming instantly.
            let blocked = fullscreen || display_request_active;
            if blocked {
                blocker_last_seen.insert(key.clone(), now);
            }
            // Same floor write_screen_protector_profile enforces, so a
            // hand-edited config cannot drop the timeout to zero, where the
            // blocked pin (timeout - 1) would reach zero and satisfy the gate
            // while the blocker still holds.
            let timeout = profile.seconds.max(MIN_SCREEN_PROTECTOR_SECONDS);
            let since_clear = blocker_last_seen
                .get(&key)
                .map(|t| t.elapsed().as_secs().min(u32::MAX as u64) as u32)
                .unwrap_or(u32::MAX);
            let countdown = dim_countdown_seconds(idle, timeout, blocked, since_clear);
            dim_idle_baseline.insert(key.clone(), countdown as u64);
            let target = if enabled && countdown >= timeout {
                dim_level_for_opacity(profile.opacity)
            } else {
                1000
            };

            let transition = transitions
                .entry(key.clone())
                .or_insert((current, target, now));
            if transition.1 != target {
                // A fullscreen transition or input interruption reverses from
                // the exact level already displayed on this monitor.
                *transition = (current, target, now);
            }
            let next = if current == target {
                target
            } else {
                let duration = if target < transition.0 {
                    profile.fade_in_duration_ms as u32
                } else {
                    profile.fade_out_duration_ms as u32
                };
                interpolated_level(
                    transition.0,
                    transition.1,
                    transition.2.elapsed().as_millis(),
                    duration,
                )
            };
            next_levels.insert(key, next);
        }

        transitions.retain(|key, _| seen.contains(key));
        blocker_last_seen.retain(|key, _| seen.contains(key));
        dim_idle_baseline.retain(|key, _| seen.contains(key));
        if next_levels != levels {
            levels = next_levels;
            // `true` restores all monitors whose fullscreen suppression just
            // ended, while each monitor still receives its own brightness.
            apply_dim_levels(&levels, true);
        }
        // Publish the per-tick state for the Debug tab's screen-dimmer
        // readout. Small struct, one lock; the reasons are derived by the
        // diagnostics command from this snapshot plus the gamma-path checks.
        {
            // `levels` mirrors `next_levels` here (the branch above either
            // moved `next_levels` into it or left it equal).
            let dimmed = levels.values().any(|level| *level < 1000);
            {
                let mut diag = screen_dimmer_diagnostics()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                diag.enabled = enabled;
                diag.dimmed = dimmed;
                // Report the re-baselined countdown closest to dimming (the
                // blocked ones are pinned below their timeout), so the
                // readout's "idle Xs of Ys" reflects what the dim gate
                // actually compares instead of a blocker-skipped raw idle.
                diag.idle_seconds = dim_idle_baseline
                    .values()
                    .copied()
                    .min()
                    .unwrap_or(idle as u64);
                diag.timeout_seconds = min_timeout as u64;
                diag.fullscreen_active = fullscreen_by_monitor.values().any(|f| *f);
                diag.display_request_active = display_request_active;
                diag.display_requests = display_requests.clone();
                diag.gamepad_active = gamepad_active;
            }
            // Notify open readouts the instant the dim state flips, so the
            // message appears in lockstep with the screen instead of trailing
            // it by up to a poll interval. Emitted after the snapshot write so
            // a readout refreshing on this event always reads fresh data.
            if dimmed != last_published_dimmed {
                last_published_dimmed = dimmed;
                emit_screen_dimmer_state_changed(dimmed);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
    }
}

fn ensure_watcher() {
    let _ = DIM_THREAD.get_or_init(|| {
        let guard = Mutex::new(None);
        if let Ok(mut guard) = guard.lock() {
            if let Ok(handle) = std::thread::Builder::new()
                .name("dim-before-off".into())
                .spawn(dim_watcher_loop)
            {
                *guard = Some(handle);
            }
        }
        guard
    });
}

pub fn init(
    screen_protector_enabled: bool,
    screen_dimmer_profiles: HashMap<String, ScreenDimmerProfile>,
    gamma_desired: Arc<AtomicI32>,
    gamma_per_monitor: Arc<Mutex<HashMap<String, i32>>>,
    gamma_brightness: Arc<Mutex<HashMap<String, u32>>>,
) {
    let _ = GAMMA_DIM_CONTEXT.set(GammaDimContext {
        desired: gamma_desired,
        per_monitor: gamma_per_monitor,
        brightness: gamma_brightness,
    });
    // Seed the shared per-monitor profiles map from the persisted config.
    // lib.rs already migrated legacy flat fields into the primary entry, so
    // the map here is the source of truth for every monitor.
    {
        let mut profiles = screen_dimmer_profiles_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Re-key persisted entries (legacy sigs or bare EDID ids) to the
        // monitors' current profile identities, so the watcher's lookups
        // match even before the frontend's backfill rewrites the config.
        for (key, profile) in screen_dimmer_profiles {
            profiles.insert(crate::monitors::canonical_profile_id(&key), profile);
        }
    }
    sync_primary_mirror();
    DIM_ENABLED.store(screen_protector_enabled, Ordering::Release);
    start_watcher(screen_protector_enabled);
}

// The dimmer no longer requires an elevated session. Media-playback
// detection (powercfg /requests) runs in the transient privileged helper
// child (helper.rs), and gamma dimming never needed admin, so the watcher
// starts in a normal user session. Keeping the app non-elevated is what
// avoids the elevated-process interaction that broke external
// low-level-hook hotkeys (Volume2) when the taskbar had focus. The helper
// task is registered when the dimmer is enabled (helper::ensure_helper_task);
// until then the dimmer suppresses on idle alone.
fn start_watcher(enabled: bool) {
    if enabled {
        crate::input_monitor::start();
    } else {
        crate::input_monitor::stop();
    }
    // The watcher is cheap (one idle poll per tick) and handles the
    // disabled case by fading back up, so just make sure it is running.
    ensure_watcher();
    if enabled && !crate::process::process_is_elevated() {
        log::debug!(
            "[power] dim watcher running non-elevated; media detection goes through the privileged helper"
        );
    }
}

pub fn set_screen_protector(
    enabled: bool,
    monitor_key: Option<String>,
    seconds: u32,
    opacity: f64,
    fade_in_duration_ms: u64,
    fade_out_duration_ms: u64,
    fullscreen_detection: bool,
) {
    DIM_ENABLED.store(enabled, Ordering::Release);
    write_screen_protector_profile(
        monitor_key.as_deref(),
        seconds,
        opacity,
        fade_in_duration_ms,
        fade_out_duration_ms,
        fullscreen_detection,
    );
    // The watcher starts in any session now (media detection is delegated
    // to the privileged helper), so enabling from the tray, a hotkey, or the
    // tab all dim immediately without a UAC relaunch.
    start_watcher(enabled);
}

pub fn shutdown() {
    DIM_SHUTDOWN.store(true, Ordering::Release);
    if let Some(guard) = DIM_THREAD.get() {
        if let Ok(mut guard) = guard.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
    // Restore gamma before the process exits, in case shutdown occurs while
    // the protector is dimmed. The GammaState teardown also resets its normal
    // Black Crush ramps immediately afterward.
    crate::input_monitor::stop();
    apply_uniform_dim_level(1000);
}

// ── Tauri commands ──────────────────────────────────────────────────

#[tauri::command]
pub fn power_get_state() -> Result<PowerState, String> {
    let display_seconds =
        read_timeout_seconds(&GUID_VIDEO_SUBGROUP, &GUID_VIDEOIDLE_TIMEOUT).unwrap_or(0);
    let sleep_seconds =
        read_timeout_seconds(&GUID_SLEEP_SUBGROUP, &GUID_STANDBY_TIMEOUT).unwrap_or(0);
    let screensaver_path = read_string(DESKTOP_SUBKEY, SCRNSAVE_VALUE)
        .unwrap_or(None)
        .unwrap_or_default();
    Ok(PowerState {
        display_timeout_minutes: display_seconds / 60,
        sleep_timeout_minutes: sleep_seconds / 60,
        // Windows' own dialog disables the screen saver by clearing SCRNSAVE.EXE
        // (its "(None)" entry). Treat an empty value as disabled even if the SPI
        // active flag was left on, so both states agree.
        screensaver_enabled: read_screensaver_active().unwrap_or(false)
            && !screensaver_path.is_empty(),
        screensaver_timeout_minutes: read_screensaver_timeout_seconds().unwrap_or(0) / 60,
        screensaver_path,
        screensavers: list_screensavers(),
        screen_protector_enabled: DIM_ENABLED.load(Ordering::Acquire),
        screen_protector_seconds: DIM_TIMEOUT_SECONDS.load(Ordering::Acquire),
        screen_protector_opacity: DIM_OPACITY.load(Ordering::Acquire) as f64 / 1000.0,
        screen_protector_fade_in_duration_ms: DIM_FADE_IN_MS.load(Ordering::Acquire) as u64,
        screen_protector_fade_out_duration_ms: DIM_FADE_OUT_MS.load(Ordering::Acquire) as u64,
        screen_protector_fullscreen_detection: DIM_FULLSCREEN_DETECTION.load(Ordering::Acquire),
    })
}

#[tauri::command]
pub async fn power_set_display_timeout(minutes: u32) -> Result<(), String> {
    // `powercfg /change` spawns a process and blocks until it exits, once per
    // power source (AC then DC). A synchronous command body runs on the main
    // thread, so the UI froze for the length of both runs every time a slider
    // moved. spawn_blocking parks that wait on a worker thread and still
    // reports failures back to the frontend.
    let run = tauri::async_runtime::spawn_blocking(move || {
        powercfg_change("monitor-timeout", minutes)
    })
    .await
    .map_err(|error| format!("the powercfg task failed: {error}"))?;
    run
}

#[tauri::command]
pub async fn power_set_sleep_timeout(minutes: u32) -> Result<(), String> {
    // Same reason as the display timeout above: the two blocking powercfg runs
    // must not sit on the main thread.
    let run = tauri::async_runtime::spawn_blocking(move || {
        powercfg_change("standby-timeout", minutes)
    })
    .await
    .map_err(|error| format!("the powercfg task failed: {error}"))?;
    run
}

#[tauri::command]
pub fn power_set_screensaver(
    enabled: bool,
    timeout_minutes: u32,
    path: String,
) -> Result<(), String> {
    // Apply the path first so a freshly enabled screen saver has a real .scr
    // to run, then the timeout, then the active flag. Windows treats an empty
    // SCRNSAVE.EXE as "no screen saver configured", so an enable with no
    // concrete .scr yet skips the write (it defaults to the first available).
    if !path.is_empty() {
        write_string(DESKTOP_SUBKEY, SCRNSAVE_VALUE, &path)?;
    } else if !enabled {
        // Disabling: clear the configured .scr too, so Windows' own settings
        // dialog shows its "(None)" entry and agrees with the app's Disabled
        // state instead of still listing the last screensaver.
        write_string(DESKTOP_SUBKEY, SCRNSAVE_VALUE, "")?;
    }
    set_screensaver_timeout_seconds(timeout_minutes.saturating_mul(60).max(60))?;
    set_screensaver_active(enabled)
}

#[tauri::command]
pub fn power_set_screen_protector(
    app: AppHandle,
    monitor_key: Option<String>,
    enabled: bool,
    seconds: u32,
    opacity: f64,
    fade_in_duration_ms: u64,
    fade_out_duration_ms: u64,
    fullscreen_detection: bool,
) -> Result<(), String> {
    set_screen_protector(
        enabled,
        monitor_key,
        seconds,
        opacity,
        fade_in_duration_ms,
        fade_out_duration_ms,
        fullscreen_detection,
    );
    // The screen dimmer settings have no settings-change broadcast of their
    // own, so notify listeners (the tray's dimmer row) when they change
    // instead of leaving them to poll.
    let _ = app.emit("screen-dimmer-changed", ());
    Ok(())
}

// Per-monitor status read for the Screen Dimmer tab: returns the resolved
// monitor's profile plus the global master toggle. Mirrors
// get_taskbar_dimmer_status; power_get_state keeps returning the primary's
// values for the no-selector paths (TabPower's poll).
#[derive(Serialize)]
pub struct ScreenDimmerStatus {
    pub enabled: bool,
    pub seconds: u32,
    pub opacity: f64,
    pub fade_in_duration_ms: u64,
    pub fade_out_duration_ms: u64,
    pub fullscreen_detection: bool,
}

#[tauri::command]
pub fn power_get_screen_dimmer_status(monitor_key: Option<String>) -> ScreenDimmerStatus {
    let key = resolve_profile_key(monitor_key.as_deref());
    let profile = screen_dimmer_profiles_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
        .unwrap_or_default();
    ScreenDimmerStatus {
        enabled: DIM_ENABLED.load(Ordering::Acquire),
        seconds: profile.seconds,
        opacity: profile.opacity,
        fade_in_duration_ms: profile.fade_in_duration_ms,
        fade_out_duration_ms: profile.fade_out_duration_ms,
        fullscreen_detection: profile.fullscreen_detection,
    }
}

// Debug-tab readout: whether the screen dimmer is actively dimming and, if
// not, what is preventing it. The watcher publishes the per-tick facts
// (idle/timeout/fullscreen/power request/gamepad); the gamma-path blockers
// and the elevation state are evaluated live because they live outside the
// watcher.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenDimmerDiagnosticsView {
    pub enabled: bool,
    pub dimmed: bool,
    pub idle_seconds: u64,
    pub timeout_seconds: u64,
    pub fullscreen_active: bool,
    pub display_request_active: bool,
    pub display_requests: Vec<DisplayRequestInfo>,
    pub gamepad_active: bool,
    pub elevated: bool,
    pub nvidia_reference_mode: bool,
    pub hold_compare_active: bool,
    pub reasons: Vec<String>,
}

#[tauri::command]
pub fn power_get_screen_dimmer_diagnostics() -> ScreenDimmerDiagnosticsView {
    // A readout is being viewed, so ask the watcher for a fresh powercfg
    // probe on its next tick. The returned snapshot may be up to one poll
    // old; the next poll (one second later) reflects the probe.
    DIM_DIAGNOSTICS_PROBE.store(true, Ordering::Release);
    let snapshot = screen_dimmer_diagnostics()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let enabled = DIM_ENABLED.load(Ordering::Acquire);
    let elevated = crate::process::process_is_elevated();
    let nvidia_reference_mode = crate::vendor_info::nvidia_reference_mode_enabled();
    let hold_compare_active = crate::hotkeys::is_hold_compare_active();

    // The reasons a dimmed state is not visible on screen, mirroring the
    // watcher's own suppression conditions plus the gamma-path early-outs.
    // "Waiting for idle" is normal behavior, so it is not listed as a reason.
    // The dimmer runs in any session now (no elevation gate): media playback
    // is suppressed through the privileged helper, so a missing helper only
    // shows up as an absent display-request blocker, never as a hard block.
    let mut reasons = Vec::new();
    if !enabled {
        reasons.push("Dimming is turned off".to_string());
    } else {
        if snapshot.fullscreen_active {
            reasons.push("A fullscreen app is active".to_string());
        }
        for request in &snapshot.display_requests {
            // The path is a Windows device path, so the file name is the last
            // backslash-separated component (for a plain [DRIVER] name with no
            // path it is the whole string).
            let name = std::path::Path::new(&request.process)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(&request.process);
            let mut detail = format!("{name} is keeping the display awake");
            let reason = request.reason.trim();
            if !reason.is_empty() && !reason.eq_ignore_ascii_case("none") {
                detail.push_str(&format!(" ({reason})"));
            }
            reasons.push(detail);
        }
        if nvidia_reference_mode {
            reasons.push("NVIDIA reference mode is enabled".to_string());
        }
        if hold_compare_active {
            reasons.push("A gamma comparison is being held".to_string());
        }
    }

    ScreenDimmerDiagnosticsView {
        enabled,
        dimmed: snapshot.dimmed,
        idle_seconds: snapshot.idle_seconds,
        timeout_seconds: snapshot.timeout_seconds,
        fullscreen_active: snapshot.fullscreen_active,
        display_request_active: snapshot.display_request_active,
        display_requests: snapshot.display_requests.clone(),
        gamepad_active: snapshot.gamepad_active,
        elevated,
        nvidia_reference_mode,
        hold_compare_active,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    // Any entry in the DISPLAY section means a display power request, which
    // is what the old boolean parser asserted. The vec parser keeps that
    // contract while also naming the culprit.
    fn has_display_request(output: &str) -> bool {
        !super::display_requests_from_powercfg_output(output).is_empty()
    }

    #[test]
    fn detects_display_process_request() {
        let output = "DISPLAY:\n[PROCESS]\\Device\\HarddiskVolume2\\msedge.exe\nVideo Wake Lock\n\nSYSTEM:\nNone\n";
        assert!(has_display_request(output));
    }

    #[test]
    fn detects_utf16_display_request() {
        let units: Vec<u16> = "DISPLAY:\n[PROCESS] msedge.exe\nVideo Wake Lock\n"
            .encode_utf16()
            .collect();
        let mut bytes = vec![0xff, 0xfe];
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = super::decode_powercfg_output(&bytes);
        assert!(has_display_request(&decoded));
    }

    #[test]
    fn detects_display_driver_request() {
        let output = "DISPLAY:\n[DRIVER] Example display driver\nReason\n";
        assert!(has_display_request(output));
    }

    #[test]
    fn ignores_empty_display_section() {
        let output = "DISPLAY:\nNinguna\n\nSYSTEM:\n[PROCESS]\\foo.exe\n";
        assert!(!has_display_request(output));
    }

    #[test]
    fn extracts_display_request_culprit() {
        let output = "DISPLAY:\n[PROCESS]\\Device\\HarddiskVolume2\\msedge.exe\nVideo Wake Lock\n\nSYSTEM:\nNone\n";
        let requests = super::display_requests_from_powercfg_output(output);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].kind, "PROCESS");
        assert_eq!(requests[0].process, "\\Device\\HarddiskVolume2\\msedge.exe");
        assert_eq!(requests[0].reason, "Video Wake Lock");
    }

    #[test]
    fn extracts_multiple_display_request_culprits() {
        let output = "DISPLAY:\n[PROCESS]\\app1.exe\nPlaying video\n[DRIVER] Example display driver\nSome reason\n[PROCESS]\\app2.exe\n\nSYSTEM:\nNone\n";
        let requests = super::display_requests_from_powercfg_output(output);
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].kind, "PROCESS");
        assert_eq!(requests[0].process, "\\app1.exe");
        assert_eq!(requests[0].reason, "Playing video");
        assert_eq!(requests[1].kind, "DRIVER");
        assert_eq!(requests[1].process, "Example display driver");
        assert_eq!(requests[1].reason, "Some reason");
        assert_eq!(requests[2].kind, "PROCESS");
        assert_eq!(requests[2].process, "\\app2.exe");
        // A trailing entry with no reason line still completes with empty text.
        assert_eq!(requests[2].reason, "");
    }

    #[test]
    fn ignores_other_sections_culprits() {
        let output = "DISPLAY:\nNone\n\nSYSTEM:\n[PROCESS]\\foo.exe\nAudio\n";
        assert!(super::display_requests_from_powercfg_output(output).is_empty());
    }

    // The dim gate compares the re-baselined countdown against the timeout.
    // While a blocker holds, the pin (timeout - 1) must never satisfy it, and
    // after the blocker clears the countdown must restart near zero instead
    // of dimming instantly off the idle time that piled up while blocked.
    #[test]
    fn blocked_countdown_never_reaches_timeout() {
        // Idle far past the timeout with the blocker still holding (a video
        // wake lock while the user is away).
        let countdown = super::dim_countdown_seconds(600, 40, true, 0);
        assert!(countdown < 40);
        assert_eq!(countdown, 39);
    }

    #[test]
    fn blocked_countdown_tracks_idle_below_the_pin() {
        // Fresh idle time still shows through while blocked (small idle, so
        // the pin at timeout - 1 is not what limits the countdown).
        assert_eq!(super::dim_countdown_seconds(10, 40, true, 0), 10);
    }

    #[test]
    fn blocker_clear_restarts_the_countdown() {
        // The exact bug: idle piled up to 600 while a video held the display
        // awake, the video buffered and dropped the wake lock, and the dim
        // fired instantly. With the re-baseline, three seconds after the
        // clear the countdown is still 3, far from the 40s timeout.
        let countdown = super::dim_countdown_seconds(600, 40, false, 3);
        assert_eq!(countdown, 3);
        assert!(countdown < 40);
    }

    #[test]
    fn dim_fires_again_after_the_full_timeout_from_the_clear() {
        // Once the time since the blocker cleared reaches the timeout (and
        // the user genuinely never touched anything), the dim applies again.
        let countdown = super::dim_countdown_seconds(600, 40, false, 41);
        assert!(countdown >= 40);
    }

    #[test]
    fn unblocked_idle_without_a_prior_blocker_is_unchanged() {
        // No blocker ever seen: the sentinel since_clear leaves raw idle
        // intact, so the classic countdown behavior is preserved.
        assert_eq!(super::dim_countdown_seconds(35, 40, false, u32::MAX), 35);
    }

    #[test]
    fn real_input_reset_wins_over_the_blocker_clear() {
        // Input during or after the block resets idle to 2; the countdown
        // must take the smaller of idle and the since-clear clock.
        assert_eq!(super::dim_countdown_seconds(2, 40, false, 100), 2);
    }
}
