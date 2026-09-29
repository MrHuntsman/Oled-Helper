// display_info.rs, queries and changes the current display mode via Win32.
// Browser APIs (window.screen) can report resolution but never expose the real
// refresh rate or the complete set of supported display modes.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, OnceLock,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, path::PathBuf, process::Command};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiOpenDevRegKey, DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DEV,
    SP_DEVINFO_DATA,
};
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_TEST, CDS_TYPE,
    CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_PRIMARY_DEVICE,
    DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
};
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, KEY_QUERY_VALUE,
};
use windows::Win32::System::Threading::{
    CreateMutexW, ReleaseMutex, WaitForSingleObject, INFINITE,
};

use tauri::AppHandle;
use windows::core::{GUID, PCWSTR};

// Every record goes through the standard `log` facade. The tauri-plugin-log
// targets fan that same record out to stdout, the rotating file, and the
// frontend `log://log` event consumed by the Debug tab and DevTools bridge.
fn log_edid(level: &str, message: String) {
    if level != "warn" && level != "error" && !crate::detailed_logging_enabled() {
        return;
    }
    match level {
        "warn" => log::warn!("[display-info] {message}"),
        "error" => log::error!("[display-info] {message}"),
        _ => log::info!("[display-info] {message}"),
    }
}

#[derive(serde::Serialize, Clone)]
pub struct ResolutionInfo {
    pub width: u32,
    pub height: u32,
}

#[derive(serde::Serialize)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    pub device_name: String,
    pub monitor_name: String,
    pub monitor_id: String,
    // The identity per-monitor profile keys use (the EDID id, disambiguated
    // for identical panels — serial first, then connector, then device name).
    // Same source as
    // list_monitors' profile_id so the tray/status bar key like the main
    // window. Empty when the monitor could not be matched.
    pub profile_id: String,
    // The monitor's true native (EDID preferred timing) resolution, which is
    // what Windows marks as "(Recommended)". 0 when it could not be read.
    pub native_width: u32,
    pub native_height: u32,
    // Which EDID location supplied the native resolution ("CEA-861",
    // "base EDID", or ""), diagnostic info for the Debug tab.
    pub native_source: String,
    // Full GPU model name of the adapter driving the primary display (DXGI
    // adapter description). Vendor-agnostic; see vendor_info.rs.
    pub gpu_model: String,
    // Physical connector ("HDMI", "DisplayPort", ...) and the current color
    // pipeline (format + bits per channel), queried via the CCD API. Empty
    // strings / 0 when the query failed.
    pub connector: String,
    pub color_format: String,
    pub bits_per_channel: u32,
    pub desktop_bits: u32,
    pub dynamic_range: String,
    pub vrr: String,
    pub reference_mode: bool,
    pub refresh_hz: u32,
    pub refresh_rates: Vec<u32>,
    pub resolutions: Vec<ResolutionInfo>,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DisplayTargetInfo {
    pub device_name: String,
    pub monitor_name: String,
    pub monitor_id: String,
    pub profile_id: String,
}

#[derive(Clone)]
struct DisplayTarget {
    device_name: String,
    monitor_name: String,
    monitor_id: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RecoveryRecord {
    device_name: String,
    #[serde(default)]
    monitor_id: String,
    width: u32,
    height: u32,
    refresh_hz: u32,
    #[serde(default)]
    bits_per_pixel: u32,
    deadline_ms: u64,
    #[serde(default)]
    token: u64,
}

fn recovery_path() -> PathBuf {
    std::env::temp_dir().join("oled-helper-display-mode-recovery.json")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// Pushes the pending transaction's rollback deadline out so the user gets the
// full confirmation window from the moment the dialog is actually visible.
//
// The deadline is computed at apply time, but a cold flyout may take long
// enough to show that the countdown is already nearly over. The frontend
// reports a fresh deadline when the dialog becomes visible. Serialize that
// update with confirm, rollback, and new mode changes so a stale report cannot
// stretch a newer transaction's deadline.
fn confirmation_deadline_extension(
    current_deadline_ms: u64,
    requested_deadline_ms: u64,
    now_ms: u64,
) -> Option<u64> {
    let maximum_deadline = now_ms
        .saturating_add((MODE_CONFIRMATION_SECONDS as u64) * 1000 + 1_000);
    (requested_deadline_ms > current_deadline_ms && requested_deadline_ms <= maximum_deadline)
        .then_some(requested_deadline_ms)
}

pub fn extend_mode_confirmation(token: u64, deadline_ms: u64) -> Option<u64> {
    let _operation_guard = mode_operation_lock().lock().ok()?;
    let entry = pending_mode().lock().ok().and_then(|guard| guard.clone())?;
    if entry.token != token {
        return None;
    }
    let current = ROLLBACK_DEADLINE_MS.load(Ordering::Acquire);
    let Some(extended_deadline) =
        confirmation_deadline_extension(current, deadline_ms, now_ms())
    else {
        return Some(current);
    };
    if extended_tokens()
        .lock()
        .ok()?
        .contains(&entry.token)
    {
        return Some(current);
    }

    let _recovery_guard = lock_recovery_state().ok()?;
    let path = recovery_path();
    let recovery_data = fs::read(&path).ok()?;
    let mut recovery: RecoveryRecord = serde_json::from_slice(&recovery_data).ok()?;
    if recovery.token != entry.token {
        return None;
    }
    recovery.deadline_ms = extended_deadline.saturating_add(RECOVERY_WATCHDOG_GRACE_MS);
    write_recovery_record_value(&recovery).ok()?;
    extended_tokens().lock().ok()?.insert(entry.token);

    ROLLBACK_DEADLINE_MS.store(extended_deadline, Ordering::Release);
    log::info!(
        "[display-info] flyout was shown late; extended the confirmation deadline by {} ms",
        extended_deadline - current
    );
    Some(extended_deadline)
}

fn write_recovery_record_value(record: &RecoveryRecord) -> Result<(), String> {
    let path = recovery_path();
    let temp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    fs::write(&temp, data)
        .map_err(|error| format!("could not save display recovery state: {error}"))?;
    let temp_w = wide(&temp.to_string_lossy());
    let path_w = wide(&path.to_string_lossy());
    unsafe {
        if let Err(error) = MoveFileExW(
            PCWSTR(temp_w.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        ) {
            let _ = fs::remove_file(&temp);
            return Err(format!("could not publish display recovery state: {error}"));
        }
    }
    Ok(())
}

fn write_recovery_record_until(
    target: &DisplayTarget,
    previous: &DEVMODEW,
    deadline_ms: u64,
    token: u64,
) -> Result<(), String> {
    write_recovery_record_value(&RecoveryRecord {
        device_name: target.device_name.clone(),
        monitor_id: target.monitor_id.clone(),
        width: previous.dmPelsWidth,
        height: previous.dmPelsHeight,
        refresh_hz: previous.dmDisplayFrequency,
        bits_per_pixel: previous.dmBitsPerPel,
        deadline_ms,
        token,
    })
}

fn write_recovery_record(
    target: &DisplayTarget,
    previous: &DEVMODEW,
    token: u64,
) -> Result<(), String> {
    // Covers the maximum three-second mode stabilization window plus the
    // fifteen-second user confirmation window, and stays past a late-dialog
    // deadline extension (extend_mode_confirmation) plus a small crash margin,
    // so the external watchdog never restores the mode while the flyout is
    // still legitimately counting down.
    write_recovery_record_until(target, previous, now_ms() + 40_000, token)
}

struct RecoveryMutex {
    handle: windows::Win32::Foundation::HANDLE,
}

impl Drop for RecoveryMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

fn lock_recovery_state() -> Result<RecoveryMutex, String> {
    let name = wide("Local\\OLEDHelperDisplayRecovery");
    let handle = unsafe {
        CreateMutexW(None, false, PCWSTR(name.as_ptr()))
            .map_err(|error| format!("could not create display recovery mutex: {error}"))?
    };
    let result = unsafe { WaitForSingleObject(handle, INFINITE) };
    // WAIT_ABANDONED (0x80) still grants ownership after the previous
    // process died; accepting it lets the new watchdog recover safely.
    if result.0 != 0 && result.0 != 0x80 {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err("could not lock display recovery state".into());
    }
    Ok(RecoveryMutex { handle })
}

fn quarantine_recovery_record() -> Result<(), String> {
    let path = recovery_path();
    if !path.exists() {
        return Ok(());
    }
    let quarantine = path.with_file_name("oled-helper-display-mode-recovery.legacy.json");
    fs::rename(&path, &quarantine).map_err(|error| {
        format!("could not quarantine incomplete display recovery state: {error}")
    })?;
    let _ = fs::remove_file(path.with_extension("json.tmp"));
    Ok(())
}

fn clear_recovery_record_for(token: u64) -> Result<(), String> {
    let path = recovery_path();
    if let Ok(data) = fs::read(&path) {
        let record: RecoveryRecord = serde_json::from_slice(&data)
            .map_err(|error| format!("display recovery state is invalid: {error}"))?;
        if record.token != token {
            return Err("display recovery state belongs to a different mode transaction".into());
        }
    }
    let path = recovery_path();
    let temp = path.with_extension("json.tmp");
    let mut error = None;
    if let Err(err) = fs::remove_file(&path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            error = Some(err);
        }
    }
    if let Err(err) = fs::remove_file(&temp) {
        if err.kind() != std::io::ErrorKind::NotFound && error.is_none() {
            error = Some(err);
        }
    }
    if path.exists() || temp.exists() {
        return Err("display recovery state could not be cleared".into());
    }
    error.map_or(Ok(()), |err| {
        Err(format!(
            "display recovery state could not be cleared: {err}"
        ))
    })
}

fn mode_operation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// The automatic-rollback thread needs to force-hide the confirmation flyout
// when the confirmation window expires without a decision: the frontend
// countdown normally hides it, but a blank/missed flyout-show would
// otherwise leave a transparent always-on-top window blocking clicks on the
// main window underneath. Set once at startup.
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

pub fn set_app_handle(app: AppHandle) {
    let _ = APP_HANDLE.set(app);
}

fn restore_recovery_record(record: &RecoveryRecord) -> Result<(), String> {
    // Restore the EXACT monitor the transaction was for, not whichever one
    // happens to be primary at recovery time. The monitor_id check below still
    // guards against a reused device name pointing at a different physical
    // monitor after a hotplug.
    let detected = display_target_for_device(&record.device_name).ok_or_else(|| {
        format!(
            "recovery could not detect the recorded monitor {}",
            record.device_name
        )
    })?;
    if detected.device_name != record.device_name {
        return Err(format!(
            "recovery target changed from {} to {}",
            record.device_name, detected.device_name
        ));
    }
    if !record.monitor_id.is_empty()
        && !detected.monitor_id.is_empty()
        && !detected.monitor_id.eq_ignore_ascii_case(&record.monitor_id)
    {
        return Err("recovery target monitor identity changed; refusing to apply the old mode to a different monitor".into());
    }
    let current = current_devmode(&detected.device_name)
        .ok_or_else(|| "recovery could not read the current display mode".to_string())?;
    let mode = recovery_mode_for(
        &detected,
        &current,
        record.width,
        record.height,
        record.refresh_hz,
        record.bits_per_pixel,
    )?;
    unsafe {
        change_mode(&detected, &mode, CDS_UPDATEREGISTRY)?;
    }
    wait_for_mode(&detected, &mode)
        .map(|_| ())
        .ok_or_else(|| "recovery mode was not verified after apply".to_string())
}

pub fn run_recovery_watchdog() {
    // A restore the driver keeps rejecting must not retry forever: the
    // recovery record blocks every future mode change (begin_mode_change
    // refuses while it exists), so a permanently rejected restore would wedge
    // the app even across restarts. Give up after RECOVERY_GIVE_UP_MS of
    // continuous failure and quarantine the record so mode changes work again.
    let mut give_up_at: Option<u64> = None;
    loop {
        let path = recovery_path();
        let Ok(data) = fs::read(&path) else {
            return;
        };
        let Ok(record) = serde_json::from_slice::<RecoveryRecord>(&data) else {
            let Ok(_recovery_guard) = lock_recovery_state() else {
                log::error!(
                    "[display-info] recovery watchdog could not lock invalid recovery state"
                );
                return;
            };
            let _ = quarantine_recovery_record();
            log::error!("[display-info] recovery watchdog quarantined invalid recovery state; refusing automatic mode changes");
            return;
        };
        if record.token == 0 || record.monitor_id.is_empty() {
            let Ok(_recovery_guard) = lock_recovery_state() else {
                log::error!(
                    "[display-info] recovery watchdog could not lock legacy recovery state"
                );
                return;
            };
            let _ = quarantine_recovery_record();
            log::error!("[display-info] recovery watchdog quarantined legacy or incomplete recovery state; refusing automatic mode changes");
            return;
        }
        let now = now_ms();
        if now < record.deadline_ms {
            let wait_ms = (record.deadline_ms - now).min(500);
            std::thread::sleep(Duration::from_millis(wait_ms));
            continue;
        }
        // Serialize this check, mode restoration, and cleanup with confirmation
        // and manual rollback. This is a cross-process named mutex, not merely
        // the in-process operation mutex, so a confirmation cannot race the
        // final watchdog decision.
        let Ok(_recovery_guard) = lock_recovery_state() else {
            log::error!("[display-info] recovery watchdog could not lock recovery state");
            continue;
        };
        // The parent may have confirmed the mode after this watchdog read the
        // record. Re-read while holding the named mutex and require the same
        // token/deadline; deletion now cancels recovery atomically with the
        // confirmation path.
        let Ok(latest_data) = fs::read(&path) else {
            return;
        };
        let Ok(latest) = serde_json::from_slice::<RecoveryRecord>(&latest_data) else {
            return;
        };
        if latest.token != record.token {
            return;
        }
        if latest.deadline_ms != record.deadline_ms {
            // The parent extended this same transaction while this watchdog
            // was waiting for the cross-process mutex. Re-read and wait for
            // the updated deadline instead of abandoning crash recovery.
            drop(_recovery_guard);
            continue;
        }
        match restore_recovery_record(&latest) {
            Ok(()) => match clear_recovery_record_for(latest.token) {
                Ok(()) => {
                    log::warn!(
                        "[display-info] recovery watchdog restored the unconfirmed display mode"
                    );
                    return;
                }
                Err(error) => {
                    log::error!("[display-info] recovery watchdog restored the mode but could not clear recovery state: {error}");
                }
            },
            Err(error) => {
                log::error!("[display-info] recovery watchdog failed: {error}");
                let now = now_ms();
                let give_up = *give_up_at.get_or_insert(now + RECOVERY_GIVE_UP_MS);
                if now >= give_up {
                    log::error!(
                        "[display-info] recovery watchdog could not restore the display mode within {} ms; quarantining the recovery record so mode changes are possible again",
                        RECOVERY_GIVE_UP_MS
                    );
                    let _ = quarantine_recovery_record();
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn fresh_devmode() -> DEVMODEW {
    DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    }
}

fn text(value: &[u16]) -> String {
    String::from_utf16_lossy(value)
        .trim_end_matches('\0')
        .trim()
        .to_string()
}

fn primary_display_target() -> Option<DisplayTarget> {
    display_target_matching(true, None)
}

/// Resolve the target monitor for a mode change: the specific monitor when a
/// device name is supplied (per-monitor favorites), else the primary monitor.
/// Centralizes the fallback so callers never duplicate it.
fn resolve_target(device_name: Option<&str>) -> Result<DisplayTarget, String> {
    match device_name {
        Some(name) if !name.trim().is_empty() => display_target_for_device(name)
            .ok_or_else(|| format!("the monitor {name} is not connected")),
        _ => primary_display_target().ok_or_else(|| "no primary monitor was detected".to_string()),
    }
}

/// DisplayTarget for a specific monitor by its GDI device name
/// (e.g. r"\\.\DISPLAY2"). Used by the per-monitor tooltip in the top-bar
/// selector, which needs the same rich info the status bar shows for the
/// primary, but for whichever monitor is hovered.
fn display_target_for_device(device_name: &str) -> Option<DisplayTarget> {
    display_target_matching(false, Some(device_name))
}

/// Shared adapter enumeration: either the primary adapter (when
/// `primary_only`) or the adapter whose DeviceName matches `device_name`.
/// The matched adapter's first monitor becomes the target, mirroring what
/// the status-bar query uses.
fn display_target_matching(primary_only: bool, device_name: Option<&str>) -> Option<DisplayTarget> {
    unsafe {
        let mut adapter = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        for index in 0.. {
            if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut adapter, 0).as_bool() {
                break;
            }
            let candidate_name = text(&adapter.DeviceName);
            if primary_only {
                if (adapter.StateFlags.0 & DISPLAY_DEVICE_PRIMARY_DEVICE.0) == 0 {
                    continue;
                }
            } else if !device_name
                .map_or(true, |wanted| candidate_name.eq_ignore_ascii_case(wanted))
            {
                continue;
            }
            let device_name = candidate_name;
            let device_name_wide = wide(&device_name);
            let mut monitor = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            let mut monitor_found = false;
            for monitor_index in 0.. {
                let mut candidate = DISPLAY_DEVICEW {
                    cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                    ..Default::default()
                };
                if !EnumDisplayDevicesW(
                    PCWSTR(device_name_wide.as_ptr()),
                    monitor_index,
                    &mut candidate,
                    0,
                )
                .as_bool()
                {
                    break;
                }
                monitor = candidate;
                if primary_only && (monitor.StateFlags.0 & DISPLAY_DEVICE_PRIMARY_DEVICE.0) != 0 {
                    monitor_found = true;
                    break;
                }
                if monitor_index == 0 {
                    monitor_found = true;
                }
            }
            let (monitor_name, monitor_id) = if monitor_found {
                let id = text(&monitor.DeviceID);
                let friendly = text(&monitor.DeviceString);
                (
                    if friendly.is_empty() {
                        if id.is_empty() {
                            device_name.clone()
                        } else {
                            id.clone()
                        }
                    } else {
                        friendly
                    },
                    id,
                )
            } else {
                (device_name.clone(), String::new())
            };
            return Some(DisplayTarget {
                device_name,
                monitor_name,
                monitor_id,
            });
        }
    }
    None
}

fn mode_matches_display(mode: &DEVMODEW, current: &DEVMODEW) -> bool {
    mode.dmBitsPerPel == current.dmBitsPerPel
        && mode.dmDisplayFrequency > 0
        && mode.dmPelsWidth > 0
        && mode.dmPelsHeight > 0
}

fn for_each_display_mode(device_name: &str, mut callback: impl FnMut(&DEVMODEW) -> bool) {
    let device_name = wide(device_name);
    for mode_index in 0u32.. {
        let mut mode = fresh_devmode();
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(device_name.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(mode_index),
                &mut mode,
            )
        };
        if !ok.as_bool() || !callback(&mode) {
            break;
        }
    }
}

#[derive(Clone)]
struct CachedModeLists {
    device_name: String,
    monitor_id: String,
    width: u32,
    height: u32,
    bits_per_pixel: u32,
    refresh_rates: Vec<u32>,
    resolutions: Vec<ResolutionInfo>,
    captured_at: Instant,
}

static MODE_CACHE: OnceLock<Mutex<Option<CachedModeLists>>> = OnceLock::new();

fn invalidate_mode_cache() {
    if let Some(cache) = MODE_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
}

fn mode_lists_for(target: &DisplayTarget, current: &DEVMODEW) -> (Vec<u32>, Vec<ResolutionInfo>) {
    let cache = MODE_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref().filter(|cached| {
            cached.device_name == target.device_name
                && cached.monitor_id == target.monitor_id
                && cached.width == current.dmPelsWidth
                && cached.height == current.dmPelsHeight
                && cached.bits_per_pixel == current.dmBitsPerPel
                && cached.captured_at.elapsed() < Duration::from_secs(2)
        }) {
            return (cached.refresh_rates.clone(), cached.resolutions.clone());
        }
    }

    let mut rates = Vec::new();
    let mut resolutions = Vec::new();
    for_each_display_mode(&target.device_name, |mode| {
        if mode_matches_display(mode, current) {
            if mode.dmPelsWidth == current.dmPelsWidth && mode.dmPelsHeight == current.dmPelsHeight
            {
                rates.push(mode.dmDisplayFrequency);
            }
            if !resolutions.iter().any(|resolution: &ResolutionInfo| {
                resolution.width == mode.dmPelsWidth && resolution.height == mode.dmPelsHeight
            }) {
                resolutions.push(ResolutionInfo {
                    width: mode.dmPelsWidth,
                    height: mode.dmPelsHeight,
                });
            }
        }
        true
    });
    if current.dmDisplayFrequency > 0 {
        rates.push(current.dmDisplayFrequency);
    }
    if !resolutions.iter().any(|resolution| {
        resolution.width == current.dmPelsWidth && resolution.height == current.dmPelsHeight
    }) {
        resolutions.push(ResolutionInfo {
            width: current.dmPelsWidth,
            height: current.dmPelsHeight,
        });
    }
    rates.sort_unstable();
    rates.dedup();
    resolutions.sort_unstable_by_key(|resolution| (resolution.width, resolution.height));

    if let Ok(mut guard) = cache.lock() {
        *guard = Some(CachedModeLists {
            device_name: target.device_name.clone(),
            monitor_id: target.monitor_id.clone(),
            width: current.dmPelsWidth,
            height: current.dmPelsHeight,
            bits_per_pixel: current.dmBitsPerPel,
            refresh_rates: rates.clone(),
            resolutions: resolutions.clone(),
            captured_at: Instant::now(),
        });
    }
    (rates, resolutions)
}

// Reads the current display mode, retrying briefly while the display driver
// is mid-transition (sleep/resume, hotplug, mode switches). In those windows
// EnumDisplaySettingsW can succeed with a zeroed DEVMODEW (0 Hz, no modes),
// which would otherwise surface in the UI as an empty refresh-rate dropdown.
// Returns None when the mode never becomes readable so callers keep their
// last known-good values.
fn current_devmode_once(device_name: &str) -> Option<DEVMODEW> {
    let device_name = wide(device_name);
    unsafe {
        let mut devmode = fresh_devmode();
        let ok = EnumDisplaySettingsW(
            PCWSTR(device_name.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut devmode,
        );
        if ok.as_bool()
            && devmode.dmDisplayFrequency > 0
            && devmode.dmPelsWidth > 0
            && devmode.dmPelsHeight > 0
        {
            Some(devmode)
        } else {
            None
        }
    }
}

fn current_devmode(device_name: &str) -> Option<DEVMODEW> {
    for attempt in 0..5 {
        if let Some(devmode) = current_devmode_once(device_name) {
            return Some(devmode);
        }
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    None
}

pub fn query_current_display() -> Option<DisplayInfo> {
    query_display_for(&primary_display_target()?.device_name)
}

/// Rich display info for any connected monitor by GDI device name. The
/// status-bar tooltip queries the primary via `query_current_display`; the
/// top-bar monitor selector uses this per-device variant so hovering any
/// monitor shows the same fields.
pub fn query_display_for(device_name: &str) -> Option<DisplayInfo> {
    let target = display_target_for_device(device_name)?;
    let devmode = current_devmode(&target.device_name)?;
    let (refresh_rates, resolutions) = mode_lists_for(&target, &devmode);
    let (native_width, native_height, native_source) = native_resolution_for(&target)
        .map(|(w, h, source)| (w, h, source.to_string()))
        .unwrap_or((0, 0, String::new()));
    // Prefer the shared monitors module's CCD friendly name (same source the
    // per-monitor table and the CCD output-info match use); fall back to the
    // EDID name when CCD yields nothing.
    let monitor_name = crate::monitors::friendly_name_for_device(&target.device_name)
        .or_else(|| crate::monitors::primary_friendly_name())
        .unwrap_or_else(|| {
            cached_edid_monitor_name(&target).unwrap_or_else(|| target.monitor_name.clone())
        });
    let output = crate::hdr_toggle::query_display_output_info(&monitor_name);
    let profile_id = crate::monitors::profile_id_for_device(&target.device_name);
    Some(DisplayInfo {
        width: devmode.dmPelsWidth,
        height: devmode.dmPelsHeight,
        device_name: target.device_name,
        monitor_name,
        monitor_id: target.monitor_id,
        profile_id,
        native_width,
        native_height,
        native_source,
        gpu_model: output
            .as_ref()
            .map(|o| o.gpu_model.clone())
            .unwrap_or_default(),
        connector: output
            .as_ref()
            .map(|o| o.connector.clone())
            .unwrap_or_default(),
        color_format: output
            .as_ref()
            .map(|o| o.color_format.clone())
            .unwrap_or_default(),
        bits_per_channel: output.as_ref().map(|o| o.bits_per_channel).unwrap_or(0),
        desktop_bits: output.as_ref().map(|o| o.desktop_bits).unwrap_or(0),
        dynamic_range: output
            .as_ref()
            .map(|o| o.dynamic_range.clone())
            .unwrap_or_default(),
        vrr: output.as_ref().map(|o| o.vrr.clone()).unwrap_or_default(),
        reference_mode: output.as_ref().map(|o| o.reference_mode).unwrap_or(false),
        refresh_hz: devmode.dmDisplayFrequency,
        refresh_rates,
        resolutions,
    })
}

// ── Native (recommended) resolution via EDID ──────────────────────────────
// Windows' Display Settings marks a resolution as "(Recommended)" based on
// the monitor's EDID preferred timing descriptor, there is no public API
// that returns "native resolution" directly. Read the primary display's EDID
// from the monitor device's registry key (SetupAPI) and decode the first
// Detailed Timing Descriptor, exactly the data Windows itself uses. Falls
// back to (0, 0) when the EDID can't be read; the frontend then uses the
// largest supported mode as a proxy.

// GUID_CLASS_MONITOR (not exported by the windows crate).
const GUID_CLASS_MONITOR: GUID = GUID::from_u128(0x4d36e96ee32511cebfc108002be10318);

// Decodes the horizontal/vertical active pixels from an 18-byte Detailed
// Timing Descriptor. Returns None when the block is not a video timing
// (pixel clock 0, a monitor descriptor like name/serial/range limits).
fn decode_dtd(dtd: &[u8]) -> Option<(u32, u32)> {
    if dtd.len() < 18 {
        return None;
    }
    let pixel_clock = u16::from_le_bytes([dtd[0], dtd[1]]);
    if pixel_clock == 0 {
        return None;
    }
    let width = dtd[2] as u32 | (((dtd[4] & 0xF0) as u32) << 4);
    let height = dtd[5] as u32 | (((dtd[7] & 0xF0) as u32) << 4);
    if width > 0 && height > 0 {
        Some((width, height))
    } else {
        None
    }
}

// The monitor's friendly Windows name is often stored in EDID descriptor
// 0xFC. Drivers may still expose DeviceString as "Generic PnP Monitor", so
// prefer the EDID product name whenever it is valid and printable.
fn edid_monitor_name(edid: &[u8]) -> Option<String> {
    if edid.len() < 128 {
        return None;
    }
    for offset in [0x36usize, 0x48, 0x5A, 0x6C] {
        let descriptor = &edid[offset..offset + 18];
        if descriptor[0] == 0 && descriptor[1] == 0 && descriptor[2] == 0 && descriptor[3] == 0xFC {
            let name: String = descriptor[5..18]
                .iter()
                .take_while(|&&byte| byte != 0 && byte != b'\n')
                .filter(|&&byte| (0x20..=0x7E).contains(&byte))
                .map(|&byte| byte as char)
                .collect::<String>()
                .trim()
                .to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

// Maps the common CTA-861 VIC values used by modern TVs to active pixels.
// Unknown VICs are ignored rather than guessed; the base EDID DTD remains the
// safe fallback for displays that use a less common timing.
fn resolution_for_vic(vic: u8) -> Option<(u32, u32)> {
    match vic {
        1 => Some((640, 480)),
        2..=3 => Some((720, 480)),
        4..=5 => Some((1280, 720)),
        16 => Some((1920, 1080)),
        31 => Some((1920, 1080)),
        32 => Some((1920, 1080)),
        33..=34 => Some((1920, 1080)),
        95..=96 => Some((3840, 2160)),
        97 => Some((3840, 2160)),
        98..=99 => Some((4096, 2160)),
        100..=101 => Some((3840, 2160)),
        102..=103 => Some((4096, 2160)),
        104 => Some((3840, 2160)),
        _ => None,
    }
}

// Finds a native Short Video Descriptor in a CTA Video Data Block. The
// native bit is bit 7 of the SVD byte; the remaining 7 bits are the VIC.
fn cea_native_svd_resolution(edid: &[u8], block: usize, dtd_offset: usize) -> Option<(u32, u32)> {
    let data_end = if dtd_offset == 0 { 127 } else { dtd_offset };
    let end = block + data_end;
    if data_end < 4 || end > block + 127 {
        return None;
    }
    let mut cursor = block + 4;
    while cursor < end {
        let header = edid[cursor];
        cursor += 1;
        let tag = header >> 5;
        let length = (header & 0x1F) as usize;
        if cursor + length > end {
            return None;
        }
        if tag == 0x02 {
            for &svd in &edid[cursor..cursor + length] {
                if (svd & 0x80) != 0 {
                    if let Some(resolution) = resolution_for_vic(svd & 0x7F) {
                        log_edid(
                            "info",
                            format!(
                                "EDID CTA: selected native SVD VIC={} resolution {}x{}",
                                svd & 0x7F,
                                resolution.0,
                                resolution.1,
                            ),
                        );
                        return Some(resolution);
                    }
                }
            }
        }
        cursor += length;
    }
    None
}

// Looks for a CTA-861 (CEA) extension block and returns its preferred
// timing, a native CTA DTD first, then a native SVD. Many TVs (e.g. LG OLEDs)
// report 4096×2160 as the base-block DTD1 but mark 3840×2160 as the CTA
// preferred timing, which is exactly the resolution Windows shows as
// "(Recommended)".
fn cea_preferred_resolution(edid: &[u8]) -> Option<(u32, u32)> {
    // Byte 126 declares how many extension blocks follow the base block.
    // Walk that count rather than stopping at an unknown/zero-tag block:
    // DisplayID and vendor blocks may precede a CTA block.
    let extension_count = edid[126] as usize;
    let available_extensions = edid.len().saturating_sub(128) / 128;
    let extension_count = extension_count.min(available_extensions);
    let mut block = 128usize;
    for _ in 0..extension_count {
        let tag = edid[block];
        if tag != 0x02 {
            block += 128;
            continue;
        }
        // CTA-861 block layout:
        //   byte 0: extension tag (0x02)
        //   byte 1: revision
        //   byte 2: DTD offset, relative to the START of this block
        //   byte 3: capability flags + native DTD count (low nibble)
        // The DTD area ends at byte 126; byte 127 is the checksum.
        let dtd_offset = edid[block + 2] as usize;
        let header = edid[block + 3];
        let native_dtd_count = (header & 0x0F) as usize;
        log_edid("info", format!(
                "EDID CTA: block_offset={} revision={} dtd_offset={} native_dtd_count={} header=0x{:02X}",
                block, edid[block + 1], dtd_offset, native_dtd_count, header,
            ));

        // Offset 0 means this CTA block has no DTD area. Its data-block
        // collection can still contain a native SVD, so inspect that
        // collection before moving to the next extension.
        if dtd_offset == 0 {
            log_edid("info", "EDID CTA: no DTD area in this block".into());
            if let Some(resolution) = cea_native_svd_resolution(edid, block, 0) {
                return Some(resolution);
            }
            block += 128;
            continue;
        }
        if !(4..127).contains(&dtd_offset) {
            log_edid(
                "warn",
                format!(
                    "EDID CTA: invalid DTD offset {} (expected 0 or 4..126)",
                    dtd_offset,
                ),
            );
            block += 128;
            continue;
        }

        let mut dtd = block + dtd_offset;
        let dtd_end = block + 127; // exclude the extension checksum byte
        let mut dtd_index = 0usize;
        let mut first_native = None;
        let mut first_valid = None;
        while dtd + 18 <= dtd_end {
            let raw = &edid[dtd..dtd + 18];
            let raw_hex = raw
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join("");
            if let Some(res) = decode_dtd(raw) {
                log_edid(
                    "info",
                    format!(
                        "EDID CTA: DTD index={} offset={} native={} raw={} decoded={}x{}",
                        dtd_index,
                        dtd - block,
                        dtd_index < native_dtd_count,
                        raw_hex,
                        res.0,
                        res.1,
                    ),
                );
                if first_valid.is_none() {
                    first_valid = Some(res);
                }
                if dtd_index < native_dtd_count && first_native.is_none() {
                    first_native = Some(res);
                }
            } else {
                log_edid(
                    "info",
                    format!(
                        "EDID CTA: skipped non-video DTD index={} offset={} raw={}",
                        dtd_index,
                        dtd - block,
                        raw_hex,
                    ),
                );
            }
            dtd += 18;
            dtd_index += 1;
        }
        if let Some(res) = first_native.or(first_valid) {
            log_edid(
                "info",
                format!("EDID CTA: selected native timing {}x{}", res.0, res.1,),
            );
            return Some(res);
        }
        if let Some(resolution) = cea_native_svd_resolution(edid, block, dtd_offset) {
            return Some(resolution);
        }
        log_edid(
            "warn",
            format!(
                "EDID CTA: no usable native DTD or SVD found between offsets {} and 126",
                dtd_offset,
            ),
        ); // Keep looking: a later CTA extension may contain the preferred
           // timing even when this one is malformed or contains descriptors
           // only. Base-block DTD fallback happens in the caller.
        block += 128;
    }
    None
}

// Windows' "(Recommended)" label follows the EDID preferred-timing rule:
// the CTA-861 extension's preferred timing wins on TVs (matching the user's
// observed Windows behavior), otherwise the base block's DTD1, otherwise any
// remaining base DTD. Returns the resolution plus which source supplied it.
fn native_resolution_from_edid(edid: &[u8]) -> Option<(u32, u32, &'static str)> {
    if edid.len() < 128 {
        return None;
    }
    if let Some(res) = cea_preferred_resolution(edid) {
        return Some((res.0, res.1, "CEA-861"));
    }
    // Base block DTD slots: 0x36 (preferred), 0x48, 0x5A, 0x6C.
    for offset in [0x36usize, 0x48, 0x5A, 0x6C] {
        if let Some(res) = decode_dtd(&edid[offset..offset + 18]) {
            return Some((res.0, res.1, "base EDID"));
        }
    }
    None
}

struct CachedNative {
    width: u32,
    height: u32,
    source: &'static str,
    display_key: String,
    captured_at: Instant,
}

static NATIVE_CACHE: OnceLock<Mutex<Option<CachedNative>>> = OnceLock::new();

// The friendly monitor name is read from EDID through SetupAPI + a registry
// lookup, the same cost class as the native resolution read. It is called on
// every display poll and at every mode change, so cache it per monitor for the
// same window as the native-resolution cache instead of re-walking SetupAPI
// each time (that burst would otherwise land right in the middle of the
// confirmation flow).
struct CachedMonitorName {
    display_key: String,
    name: String,
    captured_at: Instant,
}

static MONITOR_NAME_CACHE: OnceLock<Mutex<Option<CachedMonitorName>>> = OnceLock::new();

fn cached_edid_monitor_name(target: &DisplayTarget) -> Option<String> {
    let display_key = if target.monitor_id.is_empty() {
        target.device_name.clone()
    } else {
        target.monitor_id.clone()
    };
    let cache = MONITOR_NAME_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref().filter(|cached| {
            let max_age = if cached.name.is_empty() {
                // A transient SetupAPI/registry failure should not re-walk the
                // device tree on every poll, but it must recover promptly after
                // resume or hot-plug.
                Duration::from_secs(5)
            } else {
                Duration::from_secs(60)
            };
            cached.display_key == display_key && cached.captured_at.elapsed() < max_age
        }) {
            return (!cached.name.is_empty()).then(|| cached.name.clone());
        }
    }
    let name = edid_name_for_target(target);
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(CachedMonitorName {
            display_key,
            name: name.clone().unwrap_or_default(),
            captured_at: Instant::now(),
        });
    }
    name
}

fn edid_name_for_target(target: &DisplayTarget) -> Option<String> {
    unsafe {
        let devices = SetupDiGetClassDevsW(
            Some(&GUID_CLASS_MONITOR),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT,
        )
        .ok()?;
        let mut sole_device = None;
        let mut matched = None;
        let mut index = 0u32;
        loop {
            let mut data = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(devices, index, &mut data).is_err() {
                break;
            }
            let mut buffer = [0u16; 256];
            let mut required = 0u32;
            if SetupDiGetDeviceInstanceIdW(devices, &data, Some(&mut buffer), Some(&mut required))
                .is_ok()
            {
                let id = text(&buffer);
                if sole_device.is_none() {
                    sole_device = Some(data);
                } else {
                    sole_device = None;
                }
                if id.eq_ignore_ascii_case(&target.monitor_id)
                    || compatible_monitor_id(&id, &target.monitor_id)
                {
                    matched = Some(data);
                    break;
                }
            }
            index += 1;
        }
        let data = matched.or(sole_device)?;
        let key = SetupDiOpenDevRegKey(
            devices,
            &data,
            DICS_FLAG_GLOBAL.0,
            0,
            DIREG_DEV,
            KEY_QUERY_VALUE.0,
        )
        .ok()?;
        let result = read_name_from_registry_key(key);
        let _ = RegCloseKey(key);
        let _ = SetupDiDestroyDeviceInfoList(devices);
        result
    }
}

fn compatible_monitor_id(a: &str, b: &str) -> bool {
    let manufacturer = |value: &str| value.split('\\').nth(1).map(str::to_ascii_uppercase);
    manufacturer(a).is_some() && manufacturer(a) == manufacturer(b)
}

unsafe fn read_name_from_registry_key(key: HKEY) -> Option<String> {
    read_edid_blob_from_registry_key(key).and_then(|blob| edid_monitor_name(&blob))
}

// Reads the raw EDID blob (128-4096 bytes) from a monitor device's registry
// key, shared by the name, serial and native-resolution readers. The EDID
// value's location varies by driver generation: most store it at the device
// key root (DIREG_DEV), but some put it in the "Device Parameters" subkey, so
// try both before giving up.
unsafe fn read_edid_blob_from_registry_key(key: HKEY) -> Option<Vec<u8>> {
    let read = |key: HKEY| {
        let edid_name = wide("EDID");
        let mut required = 0u32;
        if RegQueryValueExW(
            key,
            PCWSTR(edid_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut required),
        )
        .0 != 0
            || !(128..=4096).contains(&required)
        {
            return None;
        }
        let mut bytes = vec![0u8; required as usize];
        let mut size = required;
        if RegQueryValueExW(
            key,
            PCWSTR(edid_name.as_ptr()),
            None,
            None,
            Some(bytes.as_mut_ptr()),
            Some(&mut size),
        )
        .0 != 0
        {
            return None;
        }
        Some(bytes[..(size as usize).min(bytes.len())].to_vec())
    };
    read(key).or_else(|| {
        let params_name = wide("Device Parameters");
        let mut params = HKEY::default();
        if RegOpenKeyExW(
            key,
            PCWSTR(params_name.as_ptr()),
            Some(0),
            KEY_QUERY_VALUE,
            &mut params,
        )
        .0 == 0
            && !params.is_invalid()
        {
            let result = read(params);
            let _ = RegCloseKey(params);
            result
        } else {
            None
        }
    })
}

// The "Display Product Serial Number" descriptor (tag 0xFF) from a raw EDID
// blob: 13 bytes of ASCII at bytes 5..18 of the first descriptor block that
// carries the tag. The per-unit unique panel id Windows itself derives its
// per-instance monitor UID from, and the one field that stays identical
// across ports, replugs, driver resets and reboots. None when the panel ships
// no serial descriptor (common on budget and some DP panels).
pub(crate) fn edid_serial_from_blob(blob: &[u8]) -> Option<String> {
    if blob.len() < 128 {
        return None;
    }
    // Four 18-byte descriptor blocks live at 54..=125; a non-timing
    // descriptor has a zero pixel clock (bytes 0-1), making byte 3 its tag.
    for base in (54..=108).step_by(18) {
        if blob[base] == 0 && blob[base + 1] == 0 && blob[base + 3] == 0xFF {
            let raw = &blob[base + 5..base + 18];
            let serial: String = raw
                .iter()
                .take_while(|&&b| b != 0 && b != 0x0A)
                .map(|&b| b as char)
                .collect();
            let serial = serial.trim().to_string();
            if !serial.is_empty() {
                return Some(serial);
            }
        }
    }
    None
}

// The EDID serial read walks SetupAPI like the name read, so cache it per
// monitor (keyed by the device instance id, which changes on plug/unplug) for
// the same window instead of re-walking the device tree on every monitor
// enumeration (the monitor list feeds per-slider-event gamma applies).
struct CachedEdidSerial {
    serial: String,
    captured_at: Instant,
}

static EDID_SERIAL_CACHE: OnceLock<Mutex<HashMap<String, CachedEdidSerial>>> = OnceLock::new();

/// EDID "Display Product Serial Number" for the monitor with the given EDID
/// device id (MONITOR\...), the per-unit unique panel identity used to tell
/// identical panels apart. Empty when the panel ships no serial or the EDID
/// can't be read.
pub(crate) fn edid_serial_for_monitor(monitor_id: &str) -> Option<String> {
    if monitor_id.is_empty() {
        return None;
    }
    let cache = EDID_SERIAL_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.get(monitor_id).filter(|cached| {
            let max_age = if cached.serial.is_empty() {
                // A transient SetupAPI/registry failure should not re-walk the
                // device tree on every enumeration, but it must recover
                // promptly after resume or hot-plug.
                Duration::from_secs(5)
            } else {
                Duration::from_secs(60)
            };
            cached.captured_at.elapsed() < max_age
        }) {
            return (!cached.serial.is_empty()).then(|| cached.serial.clone());
        }
    }
    let serial = unsafe { edid_serial_for_monitor_uncached(monitor_id) };
    if let Ok(mut guard) = cache.lock() {
        // Drop stale entries so panels unplugged weeks ago don't accumulate.
        guard.retain(|_, cached| cached.captured_at.elapsed() < Duration::from_secs(300));
        guard.insert(
            monitor_id.to_string(),
            CachedEdidSerial {
                serial: serial.clone().unwrap_or_default(),
                captured_at: Instant::now(),
            },
        );
    }
    serial
}

unsafe fn edid_serial_for_monitor_uncached(monitor_id: &str) -> Option<String> {
    let devices = SetupDiGetClassDevsW(
        Some(&GUID_CLASS_MONITOR),
        PCWSTR::null(),
        None,
        DIGCF_PRESENT,
    )
    .ok()?;
    let mut sole_device = None;
    let mut matched = None;
    let mut index = 0u32;
    loop {
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        if SetupDiEnumDeviceInfo(devices, index, &mut data).is_err() {
            break;
        }
        let mut buffer = [0u16; 256];
        let mut required = 0u32;
        if SetupDiGetDeviceInstanceIdW(devices, &data, Some(&mut buffer), Some(&mut required))
            .is_ok()
        {
            let id = text(&buffer);
            if sole_device.is_none() {
                sole_device = Some(data);
            } else {
                sole_device = None;
            }
            if id.eq_ignore_ascii_case(monitor_id) || compatible_monitor_id(&id, monitor_id) {
                matched = Some(data);
                break;
            }
        }
        index += 1;
    }
    let data = matched.or(sole_device)?;
    let key = SetupDiOpenDevRegKey(
        devices,
        &data,
        DICS_FLAG_GLOBAL.0,
        0,
        DIREG_DEV,
        KEY_QUERY_VALUE.0,
    )
    .ok()?;
    let result =
        read_edid_blob_from_registry_key(key).and_then(|blob| edid_serial_from_blob(&blob));
    let _ = RegCloseKey(key);
    let _ = SetupDiDestroyDeviceInfoList(devices);
    result
}

fn native_resolution_for(target: &DisplayTarget) -> Option<(u32, u32, &'static str)> {
    let display_key = if target.monitor_id.is_empty() {
        target.device_name.clone()
    } else {
        target.monitor_id.clone()
    };
    let cache = NATIVE_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref().filter(|cached| {
            let max_age = if cached.width > 0 && cached.height > 0 {
                Duration::from_secs(60)
            } else {
                // A transient driver/EDID failure should not be retried for
                // every display poll, but it must recover promptly after
                // resume or hot-plug.
                Duration::from_secs(5)
            };
            cached.display_key == display_key && cached.captured_at.elapsed() < max_age
        }) {
            return (cached.width > 0 && cached.height > 0).then_some((
                cached.width,
                cached.height,
                cached.source,
            ));
        }
    }
    let native = query_native_resolution(target);
    if let Ok(mut guard) = cache.lock() {
        let (width, height, source) = native.unwrap_or((0, 0, ""));
        *guard = Some(CachedNative {
            width,
            height,
            source,
            display_key,
            captured_at: Instant::now(),
        });
    }
    native
}

fn query_native_resolution(target: &DisplayTarget) -> Option<(u32, u32, &'static str)> {
    unsafe {
        // The adapter and monitor identity were resolved once by
        // primary_display_target(), which is also used by mode enumeration.
        // Keep EDID lookup on that same target rather than independently
        // assuming monitor index 0.
        let instance_id = target.monitor_id.clone();
        log_edid(
            "info",
            format!(
                "EDID query: target adapter '{}' monitor '{}' instance id '{}'",
                target.device_name, target.monitor_name, instance_id
            ),
        );

        // 3. Enumerate monitor-class devices, find the matching instance id
        //    and read its EDID from the device registry key.
        let devices = SetupDiGetClassDevsW(
            Some(&GUID_CLASS_MONITOR),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT,
        )
        .ok();
        let Some(devices) = devices else {
            log_edid(
                "warn",
                "EDID query: SetupDiGetClassDevsW(GUID_CLASS_MONITOR) failed".into(),
            );
            return None;
        };
        // Reads the EDID registry value for a monitor device and decodes the
        // native resolution from it. Shared by the exact-match path and the
        // single-monitor fallback below.
        //
        // The EDID value's location varies by driver generation: most store it
        // at the device key root (DIREG_DEV), but some put it in the
        // "Device Parameters" subkey, so try both before giving up.
        fn read_edid_from_device(
            devices: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
            dev_data: &SP_DEVINFO_DATA,
        ) -> Option<(u32, u32, &'static str)> {
            unsafe {
                let key = SetupDiOpenDevRegKey(
                    devices,
                    dev_data,
                    DICS_FLAG_GLOBAL.0,
                    0,
                    DIREG_DEV,
                    KEY_QUERY_VALUE.0,
                )
                .ok()?;
                // Size-then-read: TVs report long EDIDs (base block + several
                // CTA-861 extension blocks can total 512+ bytes) and a fixed
                // 256-byte buffer makes the first query fail with
                // ERROR_MORE_DATA, silently losing the native resolution (the
                // UI then falls back to the largest supported mode). Query the
                // real size first, then read.
                unsafe fn read_edid_value(key: HKEY) -> Option<(u32, u32, &'static str)> {
                    let edid_name = wide("EDID");
                    let mut required = 0u32;
                    let size_query = RegQueryValueExW(
                        key,
                        PCWSTR(edid_name.as_ptr()),
                        None,
                        None,
                        None,
                        Some(&mut required),
                    );
                    if size_query.0 == 0 && required > 0 && required <= 4096 {
                        let mut edid = vec![0u8; required as usize];
                        let mut edid_size = required;
                        let query = RegQueryValueExW(
                            key,
                            PCWSTR(edid_name.as_ptr()),
                            None,
                            None,
                            Some(edid.as_mut_ptr()),
                            Some(&mut edid_size),
                        );
                        if query.0 == 0 {
                            // Clamp defensively: a misbehaving driver could
                            // return a larger size than the buffer holds.
                            let clamped = (edid_size as usize).min(edid.len());
                            log_edid(
                                "info",
                                format!(
                                    "EDID query: read {} bytes of EDID from device key",
                                    clamped,
                                ),
                            );
                            return native_resolution_from_edid(&edid[..clamped]);
                        }
                    }
                    None
                }
                let mut result = read_edid_value(key);
                if result.is_none() {
                    // Driver stores the EDID under "Device Parameters", open
                    // that subkey and retry.
                    let params_name = wide("Device Parameters");
                    let mut params = HKEY::default();
                    let open = RegOpenKeyExW(
                        key,
                        PCWSTR(params_name.as_ptr()),
                        Some(0),
                        KEY_QUERY_VALUE,
                        &mut params,
                    );
                    if open.0 == 0 && !params.is_invalid() {
                        result = read_edid_value(params);
                        let _ = RegCloseKey(params);
                    }
                }
                let _ = RegCloseKey(key);
                result
            }
        }

        let mut result = None;
        let mut matched = false;
        let mut only_device = None;
        let mut index = 0u32;
        loop {
            let mut dev_data = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(devices, index, &mut dev_data).is_err() {
                break; // ERROR_NO_MORE_ITEMS
            }
            let mut buf = [0u16; 256];
            let mut required = 0u32;
            if SetupDiGetDeviceInstanceIdW(devices, &dev_data, Some(&mut buf), Some(&mut required))
                .is_ok()
            {
                let id = String::from_utf16_lossy(&buf);
                let id = id.trim_end_matches('\0').to_string();
                log_edid(
                    "info",
                    format!(
                    "EDID query: enumerating monitor device '{id}' (looking for '{instance_id}')"
                ),
                );
                if id.eq_ignore_ascii_case(&instance_id) {
                    matched = true;
                    result = read_edid_from_device(devices, &dev_data);
                    break;
                }
                // Track the sole device seen so far for the single-monitor
                // fallback (instance-id format mismatches, casing differences).
                // The device must be read while the list is still alive, so
                // stash the id + its DevInst for later use.
                match only_device {
                    None => only_device = Some((id, dev_data)),
                    Some(_) => only_device = None, // more than one; not unambiguous
                }
            }
            index += 1;
        }
        if !matched {
            if let Some((id, dev_data)) = only_device {
                log_edid("info", format!(
                    "EDID query: primary monitor ID formats differ; using unambiguous single-monitor fallback '{id}'"
                ));
                result = read_edid_from_device(devices, &dev_data);
            } else {
                log_edid(
                    "warn",
                    format!(
                    "EDID query: no SetupAPI monitor device matched instance id '{instance_id}'"
                ),
                );
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(devices);
        match result {
            Some((w, h, src)) => {
                log_edid("info", format!("EDID query: native = {w}x{h} via {src}"));
            }
            None => log_edid(
                "warn",
                "EDID query: no usable native resolution found".into(),
            ),
        }
        result
    }
}

// The mode-confirmation window and the automatic rollback must agree on the
// same deadline. The absolute wall-clock deadline (not a relative duration)
// is computed once here, used to schedule the rollback thread, and forwarded
// through AppliedMode to the flyout so its countdown reaches zero at the
// exact moment the rollback fires, regardless of how long the show handshake
// took.
const MODE_CONFIRMATION_SECONDS: u32 = 15;

// How long the recovery watchdog keeps retrying a restore the driver rejects
// before quarantining the record. A restore that fails this long can never
// succeed by retrying, and the record would otherwise block every future mode
// change forever.
const RECOVERY_GIVE_UP_MS: u64 = 30_000;
const RECOVERY_WATCHDOG_GRACE_MS: u64 = 10_000;

#[derive(Clone, serde::Serialize)]
pub struct AppliedMode {
    pub token: u64,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub monitor_name: String,
    pub confirmation_seconds: u32,
    pub confirmation_deadline_ms: u64,
    #[serde(default)]
    pub skip_confirmation: bool,
}

#[derive(Clone)]
struct PendingMode {
    token: u64,
    target: DisplayTarget,
    previous: DEVMODEW,
    requested: DEVMODEW,
}

static PENDING_MODE: OnceLock<Mutex<Option<PendingMode>>> = OnceLock::new();
static MODE_TOKEN: AtomicU64 = AtomicU64::new(1);

// The rollback deadline of the transaction currently awaiting confirmation.
// The rollback thread re-reads it in short increments instead of sleeping once
// up to the value computed at apply time, so a deadline extension (a late
// flyout show granting the user the full window, see extend_mode_confirmation)
// is honored rather than rolling back while the dialog is still counting down.
static ROLLBACK_DEADLINE_MS: AtomicU64 = AtomicU64::new(0);
// Transactions whose deadline has already been extended once. Tokens are
// monotonically increasing, so the set only ever grows by one small entry per
// late-shown dialog and is cleared whenever a new transaction begins.
static EXTENDED_TOKENS: OnceLock<Mutex<std::collections::HashSet<u64>>> = OnceLock::new();

fn extended_tokens() -> &'static Mutex<std::collections::HashSet<u64>> {
    EXTENDED_TOKENS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn pending_mode() -> &'static Mutex<Option<PendingMode>> {
    PENDING_MODE.get_or_init(|| Mutex::new(None))
}

// Used by the tray confirmation fallback to avoid reopening the main window
// after the user has already confirmed or reverted the temporary mode.
fn mode_error(code: i32, phase: &str) -> String {
    format!("Windows rejected the {phase} display-mode change (code {code})")
}

fn target_is_current(target: &DisplayTarget) -> Result<(), String> {
    // Re-resolve the SAME monitor (by device name) rather than assuming the
    // primary: pending mode changes can target a non-primary monitor via a
    // per-monitor favorite. Resolving the primary by its device name returns
    // the equivalent target for the primary case.
    let current = display_target_for_device(&target.device_name)
        .ok_or_else(|| format!("the monitor {} is no longer available", target.device_name))?;
    if current.device_name != target.device_name {
        return Err("the display adapter changed while the mode was pending".into());
    }
    if !target.monitor_id.is_empty()
        && !current.monitor_id.is_empty()
        && !current.monitor_id.eq_ignore_ascii_case(&target.monitor_id)
    {
        return Err("the monitor changed while the mode was pending; refusing to apply the mode to a different monitor".into());
    }
    Ok(())
}

unsafe fn change_mode(
    target: &DisplayTarget,
    mode: &DEVMODEW,
    flags: CDS_TYPE,
) -> Result<(), String> {
    let device = wide(&target.device_name);
    let result = ChangeDisplaySettingsExW(
        PCWSTR(device.as_ptr()),
        Some(mode as *const DEVMODEW),
        Some(HWND::default()),
        flags,
        None,
    );
    if result == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        Err(mode_error(
            result.0,
            if flags == CDS_TEST { "test" } else { "apply" },
        ))
    }
}

fn recovery_mode_for(
    target: &DisplayTarget,
    current: &DEVMODEW,
    width: u32,
    height: u32,
    hz: u32,
    preferred_bits_per_pixel: u32,
) -> Result<DEVMODEW, String> {
    // Recovery must not fail merely because the driver now reports a different
    // color depth. Prefer the saved depth, then the current depth, then any
    // enumerated mode with the exact saved timing. Every candidate still comes
    // from Windows' mode list; no arbitrary mode is synthesized.
    let mut candidates = Vec::new();
    for_each_display_mode(&target.device_name, |mode| {
        if mode.dmPelsWidth == width
            && mode.dmPelsHeight == height
            && mode.dmDisplayFrequency == hz
            && mode.dmPelsWidth > 0
            && mode.dmPelsHeight > 0
            && mode.dmDisplayFrequency > 0
        {
            candidates.push(*mode);
        }
        true
    });
    candidates
        .into_iter()
        .min_by_key(|mode| {
            if mode.dmBitsPerPel == preferred_bits_per_pixel {
                0
            } else if mode.dmBitsPerPel == current.dmBitsPerPel {
                1
            } else {
                2
            }
        })
        .map(|mut mode| {
            mode.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
            mode
        })
        .ok_or_else(|| {
            format!(
                "recovery mode {}×{} at {} Hz is no longer available",
                width, height, hz
            )
        })
}

// Rebuilds a complete, driver-accepted DEVMODE for the mode that was active
// before a pending change, instead of replaying the DEVMODE captured when the
// change started. The captured structure can go stale while the confirmation
// window is open (a streaming session ending, a hotplug, a driver re-
// negotiation) and the driver then rejects it with DISP_CHANGE_FAILED even
// though the same resolution and refresh rate are still perfectly valid.
fn previous_mode_for(target: &DisplayTarget, previous: &DEVMODEW) -> Result<DEVMODEW, String> {
    let current = current_devmode(&target.device_name)
        .ok_or_else(|| "unable to re-read the current display mode for rollback".to_string())?;
    recovery_mode_for(
        target,
        &current,
        previous.dmPelsWidth,
        previous.dmPelsHeight,
        previous.dmDisplayFrequency,
        previous.dmBitsPerPel,
    )
    .map_err(|error| format!("the previous mode could not be re-enumerated: {error}"))
}

// Applies the previous mode on rollback, re-enumerating it from the driver's
// current mode list and retrying briefly. The driver can be mid-transition
// when the confirmation expires (streaming session teardown, hotplug), so a
// single immediate apply can fail with DISP_CHANGE_FAILED even though the
// mode is valid; a short pause and a fresh DEVMODE usually succeed once the
// driver settles.
fn apply_previous_mode(target: &DisplayTarget, previous: &DEVMODEW) -> Result<(), String> {
    let mut last_error: Option<String> = None;
    for attempt in 0..3 {
        let applied = previous_mode_for(target, previous).and_then(|mode| {
            unsafe { change_mode(target, &mode, CDS_UPDATEREGISTRY) }?;
            wait_for_mode(target, &mode)
                .map(|_| ())
                .ok_or_else(|| "display driver did not restore the previous mode".to_string())
        });
        match applied {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(400));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "previous mode restore failed".to_string()))
}

fn complete_mode_for(
    target: &DisplayTarget,
    current: &DEVMODEW,
    width: u32,
    height: u32,
    hz: u32,
) -> Result<DEVMODEW, String> {
    let mut exact = None;
    for_each_display_mode(&target.device_name, |mode| {
        if mode_matches_display(mode, current)
            && mode.dmPelsWidth == width
            && mode.dmPelsHeight == height
            && mode.dmDisplayFrequency == hz
        {
            exact = Some(*mode);
            false
        } else {
            true
        }
    });
    let mut mode = exact.ok_or_else(|| {
        format!("{width}×{height} at {hz} Hz is not an exact supported mode on this monitor")
    })?;
    mode.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    Ok(mode)
}

fn wait_for_mode(target: &DisplayTarget, requested: &DEVMODEW) -> Option<DEVMODEW> {
    // Some drivers return from ChangeDisplaySettingsEx before the desktop
    // mode reported by ENUM_CURRENT_SETTINGS has settled. Do not expose a
    // confirmation dialog until the requested mode is actually active.
    for attempt in 0..30 {
        if let Some(active) = current_devmode_once(&target.device_name) {
            if same_mode(&active, requested) {
                return Some(active);
            }
            if attempt == 0 {
                log::info!(
                    "[display-info] mode transition pending: active={}x{}@{}Hz requested={}x{}@{}Hz",
                    active.dmPelsWidth,
                    active.dmPelsHeight,
                    active.dmDisplayFrequency,
                    requested.dmPelsWidth,
                    requested.dmPelsHeight,
                    requested.dmDisplayFrequency,
                );
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn begin_mode_change(
    target: DisplayTarget,
    previous: DEVMODEW,
    mode: DEVMODEW,
) -> Result<AppliedMode, String> {
    // Reserve the entire in-process mode transaction before any slow Win32
    // calls. The guard is intentionally held until the pending transaction is
    // installed, so two concurrent IPC requests cannot both pass the old
    // check-before-apply race. Confirmation/rollback acquire the same lock.
    let _operation_guard = mode_operation_lock()
        .lock()
        .map_err(|_| "display mode operation state was poisoned".to_string())?;
    if pending_mode()
        .lock()
        .map_err(|_| "display mode state was poisoned".to_string())?
        .is_some()
    {
        return Err("another display-mode change is awaiting confirmation".into());
    }
    let token = MODE_TOKEN.fetch_add(1, Ordering::Relaxed);
    let _recovery_guard = lock_recovery_state()?;
    // Check under the same named mutex used by the watchdog and confirmation;
    // never overwrite another process's recovery record between check and write.
    if recovery_path().exists() {
        return Err("a display-mode recovery transaction is still active; resolve it before starting another change".into());
    }
    if target.monitor_id.is_empty() {
        return Err("the current monitor identity could not be determined; refusing a mode change without crash recovery".into());
    }
    target_is_current(&target)?;
    let locked_previous = current_devmode(&target.device_name)
        .ok_or_else(|| "unable to re-read the current display mode".to_string())?;
    if !same_mode(&locked_previous, &previous) {
        return Err("the display mode changed while the request was queued; refusing to use stale rollback state".into());
    }
    // Re-enumerate the requested complete mode after taking both locks. This
    // prevents a queued request from using a candidate selected before a
    // topology or driver-mode change.
    let mode = complete_mode_for(
        &target,
        &locked_previous,
        mode.dmPelsWidth,
        mode.dmPelsHeight,
        mode.dmDisplayFrequency,
    )?;
    unsafe {
        change_mode(&target, &mode, CDS_TEST)?;
    }
    // Persist the old mode before the temporary apply. A child watchdog owns
    // the same recovery record, so a process crash cannot strand the user in a
    // black-screen mode.
    write_recovery_record(&target, &previous, token)?;
    let watchdog = std::env::current_exe()
        .map_err(|error| format!("could not locate recovery watchdog: {error}"))?;
    if let Err(error) = Command::new(watchdog)
        .arg("--display-mode-watchdog")
        .spawn()
    {
        if let Err(cleanup_error) = clear_recovery_record_for(token) {
            return Err(format!(
                "could not start display recovery watchdog: {error}; recovery cleanup also failed: {cleanup_error}"
            ));
        }
        return Err(format!(
            "could not start display recovery watchdog: {error}"
        ));
    }
    // Apply AND persist in one call (CDS_UPDATEREGISTRY), exactly how the
    // CRU tool commits modes. Persisting the mode is a separate
    // ChangeDisplaySettingsExW call that costs ~245ms and makes the shell
    // react system-wide; doing it inside the apply hides that cost inside the
    // mode switch the user is already watching, instead of surfacing it as a
    // delayed hitch after they click Keep (confirm is now a no-op). The
    // watchdog still restores + re-persists the previous mode if the
    // confirmation never arrives, so the registry always matches the screen.
    if let Err(error) = unsafe { change_mode(&target, &mode, CDS_UPDATEREGISTRY) } {
        if let Err(cleanup_error) = clear_recovery_record_for(token) {
            return Err(format!(
                "{error}; recovery cleanup also failed: {cleanup_error}"
            ));
        }
        return Err(error);
    }
    if wait_for_mode(&target, &mode).is_none() {
        // Windows accepted the request, but the driver never made the exact
        // mode active. Restore the known-good mode before returning an error;
        // the frontend must not show a confirmation dialog for a false success.
        let restore = unsafe { change_mode(&target, &previous, CDS_UPDATEREGISTRY) };
        let restored = restore.is_ok() && wait_for_mode(&target, &previous).is_some();
        let cleanup_error = if restored {
            clear_recovery_record_for(token).err()
        } else {
            None
        };
        invalidate_mode_cache();
        return match restore {
            Ok(()) if restored => match cleanup_error {
                Some(cleanup_error) => Err(format!(
                    "display driver did not activate {}×{} at {} Hz; the previous mode was restored, but recovery cleanup failed: {cleanup_error}",
                    mode.dmPelsWidth, mode.dmPelsHeight, mode.dmDisplayFrequency
                )),
                None => Err(format!(
                    "display driver did not activate {}×{} at {} Hz within 3 seconds; the previous mode was restored",
                    mode.dmPelsWidth, mode.dmPelsHeight, mode.dmDisplayFrequency
                )),
            },
            Ok(()) => Err(format!(
                "display driver did not activate {}×{} at {} Hz, and the previous mode could not be verified after restore",
                mode.dmPelsWidth, mode.dmPelsHeight, mode.dmDisplayFrequency
            )),
            Err(error) => Err(format!(
                "display driver did not activate {}×{} at {} Hz, and restoring the previous mode failed: {error}",
                mode.dmPelsWidth, mode.dmPelsHeight, mode.dmDisplayFrequency
            )),
        };
    }
    invalidate_mode_cache();
    {
        let mut guard = pending_mode()
            .lock()
            .map_err(|_| "display mode state was poisoned".to_string())?;
        *guard = Some(PendingMode {
            token,
            target: target.clone(),
            previous,
            requested: mode,
        });
    }
    // Absolute deadline for both the rollback thread and the flyout
    // countdown. The rollback thread re-reads the atomic, which the visible
    // flyout can extend once if its show handshake takes a while.
    let confirmation_deadline_ms = now_ms() + (MODE_CONFIRMATION_SECONDS as u64) * 1000;
    ROLLBACK_DEADLINE_MS.store(confirmation_deadline_ms, Ordering::Release);
    if let Ok(mut guard) = extended_tokens().lock() {
        guard.clear();
    }
    std::thread::spawn(move || {
        // Sleep until the active deadline, re-reading it in short increments.
        // A single sleep to the apply-time value would roll back mid-countdown
        // whenever the show handshake pushed the deadline out.
        loop {
            let now = now_ms();
            let deadline = ROLLBACK_DEADLINE_MS.load(Ordering::Acquire);
            if now >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis((deadline - now).min(200)));
        }
        // Roll back inside a closure so the forced flyout hide below always
        // runs when THIS transaction is still pending.
        //
        // Returns whether this transaction was still awaiting confirmation
        // when the deadline hit. When the user already resolved it (Keep or
        // Revert), the pending entry is gone and the flyout may already be
        // showing a NEWER transaction's confirmation: a stale timeout from an
        // earlier mode change must not hide that newer dialog. Without this
        // gate, resolving one confirmation and immediately starting another
        // let the first transaction's timeout thread yank away the second
        // confirmation with seconds still on its countdown, leaving the user
        // unable to Keep or Revert until the second timeout restored the
        // mode.
        let expired = loop {
            // Decide before taking any lock: if the user already resolved
            // this transaction, do nothing and leave the flyout alone. A
            // poisoned lock is treated as still-pending so the fallback hide
            // still runs.
            let entry_exists = pending_mode()
                .lock()
                .map(|guard| guard.as_ref().is_some_and(|entry| entry.token == token))
                .unwrap_or(true);
            if !entry_exists {
                break false;
            }
            // Serialize the automatic rollback with confirmation and manual
            // rollback. Without this guard, the timeout thread could restore
            // the old mode while the user is simultaneously committing the
            // new one.
            let Ok(_operation_guard) = mode_operation_lock().lock() else {
                log::error!(
                    "[display-info] automatic rollback could not acquire the mode operation lock"
                );
                break true;
            };
            let Ok(_recovery_guard) = lock_recovery_state() else {
                log::error!("[display-info] automatic rollback could not lock recovery state");
                break true;
            };
            let entry = pending_mode()
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().filter(|entry| entry.token == token).cloned());
            let Some(entry) = entry else {
                break false;
            };
            // An extension may have won the operation lock just as this
            // watchdog reached its old deadline. Re-check after acquiring
            // both locks so this thread cannot roll back on a stale timeout.
            if now_ms() < ROLLBACK_DEADLINE_MS.load(Ordering::Acquire) {
                drop(_recovery_guard);
                drop(_operation_guard);
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if let Err(error) = target_is_current(&entry.target) {
                log::error!("[display-info] automatic rollback refused: {error}");
                break true;
            }
            match apply_previous_mode(&entry.target, &entry.previous) {
                Ok(()) => {
                    invalidate_mode_cache();
                    // Keep the in-memory transaction until recovery cleanup
                    // has succeeded. If cleanup fails, the watchdog and manual
                    // retry still retain the only recovery path.
                    if let Err(error) = clear_recovery_record_for(token) {
                        log::error!("[display-info] automatic rollback succeeded but recovery cleanup failed: {error}");
                        break true;
                    }
                    if let Ok(mut guard) = pending_mode().lock() {
                        if guard.as_ref().is_some_and(|current| current.token == token) {
                            guard.take();
                        }
                    }
                    log::warn!(
                        "[display-info] display mode confirmation timed out; restored previous mode"
                    );
                }
                Err(error) => {
                    // The restore was rejected even after re-enumeration
                    // retries, so retrying it again cannot succeed. Leaving
                    // the transaction pending would block every future mode
                    // change with "another display-mode change is awaiting
                    // confirmation". Clear the in-memory transaction and the
                    // on-disk recovery record so the user can start a fresh
                    // change: the unconfirmed mode is still active (it applied
                    // successfully), a known working state, and a recovery
                    // record whose restore the driver refuses can never be
                    // fulfilled anyway.
                    log::error!(
                        "[display-info] automatic display-mode rollback failed: {error}; clearing the stuck mode transaction so further changes are possible"
                    );
                    if let Ok(mut guard) = pending_mode().lock() {
                        if guard.as_ref().is_some_and(|current| current.token == token) {
                            guard.take();
                        }
                    }
                    if let Err(cleanup_error) = clear_recovery_record_for(token) {
                        log::error!(
                            "[display-info] automatic rollback failed and the recovery record could not be cleared: {cleanup_error}"
                        );
                    }
                }
            }
            break true;
        };
        if expired {
            // The confirmation window expired without the user deciding. The
            // frontend countdown normally hides the flyout itself, but when
            // the flyout was shown blank (missed flyout-show) nothing else
            // would hide it, leaving a transparent always-on-top window
            // blocking clicks on the main window underneath. Force-hide it
            // here as the authoritative fallback.
            if let Some(app) = APP_HANDLE.get() {
                crate::flyout_window::hide(app);
                // The main window only learns the confirmation is over
                // through the confirm/rollback commands or this path, and
                // neither runs when the flyout was shown blank or its webview
                // wedged. Emit the completion here too so the main window's
                // busy flags always clear and display info is re-polled, even
                // if the JS countdown never fired. A duplicate emit alongside
                // a command's own is harmless (the handler just clears flags
                // and re-polls). Always confirmed=false: reaching here means
                // the deadline expired and the mode was reverted, never kept.
                // The favorite credit armed for this transaction is dropped
                // with it, so an expired dialog can never make a favorite
                // trusted (see favorite_credit).
                crate::favorite_credit::disarm();
                crate::flyout_window::emit_completed(app, false);
            }
        }
    });
    Ok(AppliedMode {
        token,
        width: mode.dmPelsWidth,
        height: mode.dmPelsHeight,
        refresh_hz: mode.dmDisplayFrequency,
        // Use the same EDID-backed friendly name shown by get_display_info;
        // DeviceString alone may only say "Generic PnP Monitor".
        monitor_name: cached_edid_monitor_name(&target).unwrap_or(target.monitor_name),
        confirmation_seconds: MODE_CONFIRMATION_SECONDS,
        confirmation_deadline_ms,
        skip_confirmation: false,
    })
}

pub fn set_refresh_rate(hz: u32, device_name: Option<String>) -> Result<AppliedMode, String> {
    let target = resolve_target(device_name.as_deref())?;
    let current = current_devmode(&target.device_name)
        .ok_or_else(|| "unable to read the current display mode".to_string())?;
    let mode = complete_mode_for(
        &target,
        &current,
        current.dmPelsWidth,
        current.dmPelsHeight,
        hz,
    )?;
    begin_mode_change(target, current, mode)
}

pub fn set_resolution(
    width: u32,
    height: u32,
    preferred_hz: u32,
    device_name: Option<String>,
    skip_confirmation: bool,
) -> Result<AppliedMode, String> {
    let target = resolve_target(device_name.as_deref())?;
    let current = current_devmode(&target.device_name)
        .ok_or_else(|| "unable to read the current display mode".to_string())?;
    // Validate the resolution and requested refresh rate together against one
    // freshly enumerated complete DEVMODE. Never infer target-resolution Hz
    // support from the rates of the resolution we are leaving.
    let mode = complete_mode_for(&target, &current, width, height, preferred_hz)?;
    let mut applied = begin_mode_change(target, current, mode)?;
    applied.skip_confirmation = skip_confirmation;
    Ok(applied)
}

fn same_mode(a: &DEVMODEW, b: &DEVMODEW) -> bool {
    a.dmPelsWidth == b.dmPelsWidth
        && a.dmPelsHeight == b.dmPelsHeight
        && a.dmDisplayFrequency == b.dmDisplayFrequency
        && a.dmBitsPerPel == b.dmBitsPerPel
}

pub fn confirm_mode_change() -> Result<(), String> {
    // A Keep also ends any pending deadline extension cleanly; nothing below
    // needs the extension state, but the once-flag set is kept small.
    let _operation_guard = mode_operation_lock()
        .lock()
        .map_err(|_| "display mode operation state was poisoned".to_string())?;
    let guard = pending_mode()
        .lock()
        .map_err(|_| "display mode state was poisoned".to_string())?;
    let Some(entry) = guard.as_ref() else {
        return Err("no display mode change is awaiting confirmation".into());
    };
    let target = entry.target.clone();
    let requested = entry.requested;
    let token = entry.token;
    drop(guard);

    // Serialize the entire confirmation against the external watchdog. The
    // watchdog cannot restore the old mode while this guard is held, and the
    // recovery file cannot be deleted by a competing transaction.
    let _recovery_guard = lock_recovery_state()?;
    // The requested mode was already applied AND persisted by begin_mode_change
    // (CDS_UPDATEREGISTRY), so confirm no longer touches the display at all ,
    // there is nothing to persist. It only re-checks identity and that the
    // display is still running the requested mode, then clears the recovery
    // state. The same_mode check keeps the recovery path intact if the mode
    // drifted externally during the confirmation window. All reads happen
    // while the recovery lock is held, avoiding a stale pre-lock race.
    target_is_current(&target)?;
    let active = current_devmode(&target.device_name)
        .ok_or_else(|| "unable to verify the active display mode".to_string())?;
    if !same_mode(&active, &requested) {
        return Err(format!(
            "the monitor is running {}×{} at {} Hz, not the requested {}×{} at {} Hz; the temporary mode was kept for rollback",
            active.dmPelsWidth,
            active.dmPelsHeight,
            active.dmDisplayFrequency,
            requested.dmPelsWidth,
            requested.dmPelsHeight,
            requested.dmDisplayFrequency,
        ));
    }
    // (Persisting here used to be the ~245ms registry write that hitched the
    // desktop after Keep; it now lives inside the apply in begin_mode_change.)
    // Clearing recovery state is part of the commit. If it fails, leave the
    // transaction pending so a retry remains possible; safety is preferable
    // to silently allowing a stale watchdog record to be ignored.
    clear_recovery_record_for(token)?;
    invalidate_mode_cache();
    // Remove the transaction only after verification and persistence succeed;
    // otherwise the watchdog must remain able to restore the old mode.
    let mut guard = pending_mode()
        .lock()
        .map_err(|_| "display mode state was poisoned".to_string())?;
    if guard.as_ref().is_some_and(|entry| entry.token == token) {
        guard.take();
    }
    Ok(())
}

pub fn rollback_mode_change() -> Result<(), String> {
    let _operation_guard = mode_operation_lock()
        .lock()
        .map_err(|_| "display mode operation state was poisoned".to_string())?;
    let entry = pending_mode()
        .lock()
        .map_err(|_| "display mode state was poisoned".to_string())?
        .as_ref()
        .cloned();
    let Some(entry) = entry else {
        return Ok(());
    };
    let _recovery_guard = lock_recovery_state()?;
    // Any failure below must clear the stuck transaction, otherwise a
    // restore the driver keeps rejecting permanently blocks every future
    // mode change (the same wedge the automatic rollback now escapes).
    let result = (|| {
        target_is_current(&entry.target)?;
        apply_previous_mode(&entry.target, &entry.previous)?;
        clear_recovery_record_for(entry.token)?;
        invalidate_mode_cache();
        let mut guard = pending_mode()
            .lock()
            .map_err(|_| "display mode state was poisoned".to_string())?;
        if guard
            .as_ref()
            .is_some_and(|current| current.token == entry.token)
        {
            guard.take();
        }
        Ok(())
    })();
    if let Err(error) = &result {
        log::error!(
            "[display-info] manual rollback failed: {error}; clearing the stuck mode transaction so further changes are possible"
        );
        if let Ok(mut guard) = pending_mode().lock() {
            if guard
                .as_ref()
                .is_some_and(|current| current.token == entry.token)
            {
                guard.take();
            }
        }
        if let Err(cleanup_error) = clear_recovery_record_for(entry.token) {
            log::error!(
                "[display-info] manual rollback failed and the recovery record could not be cleared: {cleanup_error}"
            );
        }
    }
    result
}

pub fn launch_recovery_watchdog_if_needed() {
    let path = recovery_path();
    if !path.exists() {
        return;
    }
    // Keep validation and the decision to launch/quarantine under the same
    // named mutex used by the watchdog and mode transactions. This prevents
    // startup from validating one record while another instance replaces it.
    let Ok(_recovery_guard) = lock_recovery_state() else {
        log::error!("[display-info] could not lock display recovery state at startup");
        return;
    };
    let Ok(data) = fs::read(&path) else {
        return;
    };
    let usable = serde_json::from_slice::<RecoveryRecord>(&data)
        .ok()
        .is_some_and(|record| record.token != 0 && !record.monitor_id.is_empty());
    if !usable {
        // Legacy/incomplete records cannot be safely mapped to a monitor or
        // cancelled with the transaction token. Quarantine the state under
        // the lock, preserving it for diagnosis without launching a watchdog
        // that cannot recover safely.
        if let Err(error) = quarantine_recovery_record() {
            log::error!(
                "[display-info] could not quarantine incomplete display recovery state: {error}"
            );
        } else {
            log::error!("[display-info] quarantined incomplete display recovery state");
        }
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe).arg("--display-mode-watchdog").spawn();
    }
}

pub fn display_target_info() -> Option<DisplayTargetInfo> {
    primary_display_target().map(|target| {
        let profile_id = crate::monitors::profile_id_for_device(&target.device_name);
        DisplayTargetInfo {
            device_name: target.device_name,
            monitor_name: target.monitor_name,
            monitor_id: target.monitor_id,
            profile_id,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // 18-byte Detailed Timing Descriptor for 3840×2160 (533.25 MHz pixel clock).
    fn dtd_3840x2160() -> [u8; 18] {
        let mut dtd = [0u8; 18];
        dtd[0] = 0x4D; // pixel clock 53325 = 0xD04D, little-endian low byte
        dtd[1] = 0xD0; // pixel clock high byte
        dtd[2] = 0x00; // h active low byte (3840 & 0xFF)
        dtd[4] = 0xF0; // h active high nibble (0xF) → 3840
        dtd[5] = 0x70; // v active low byte (2160 & 0xFF)
        dtd[7] = 0x80; // v active high nibble (0x8) → 2160
        dtd
    }

    fn dtd_1920x1080() -> [u8; 18] {
        let mut dtd = [0u8; 18];
        dtd[0] = 0x02; // pixel clock 14850 = 0x3A02, little-endian low byte
        dtd[1] = 0x3A; // pixel clock high byte
        dtd[2] = 0x80; // 1920 & 0xFF
        dtd[4] = 0x70; // (1920 >> 8) & 0xF = 0x7
        dtd[5] = 0x38; // 1080 & 0xFF
        dtd[7] = 0x40; // (1080 >> 8) & 0xF = 0x4
        dtd
    }

    fn base_block(extension_count: u8, dtd1: Option<[u8; 18]>) -> Vec<u8> {
        let mut block = vec![0u8; 128];
        if let Some(dtd) = dtd1 {
            block[0x36..0x36 + 18].copy_from_slice(&dtd);
        }
        block[126] = extension_count;
        block
    }

    #[test]
    fn confirmation_deadline_extension_only_moves_forward() {
        assert_eq!(
            confirmation_deadline_extension(15_000, 20_000, 5_000),
            Some(20_000)
        );
        assert_eq!(confirmation_deadline_extension(15_000, 15_000, 5_000), None);
        assert_eq!(confirmation_deadline_extension(15_000, 10_000, 5_000), None);
        assert_eq!(confirmation_deadline_extension(15_000, 100_000, 5_000), None);
    }

    #[test]
    fn decode_dtd_rejects_zero_pixel_clock() {
        // All-zero DTD = monitor descriptor (name/range), not video timing.
        assert!(decode_dtd(&[0u8; 18]).is_none());
    }

    #[test]
    fn decode_dtd_parses_3840x2160() {
        assert_eq!(decode_dtd(&dtd_3840x2160()), Some((3840, 2160)));
    }

    #[test]
    fn decode_dtd_rejects_truncated_input() {
        assert!(decode_dtd(&[0u8; 17]).is_none());
    }

    #[test]
    fn base_block_dtd1_is_native_fallback() {
        let edid = base_block(0, Some(dtd_3840x2160()));
        assert_eq!(
            native_resolution_from_edid(&edid),
            Some((3840, 2160, "base EDID"))
        );
    }

    #[test]
    fn cta_preferred_dtd_wins_over_base_block() {
        let mut edid = base_block(1, Some(dtd_3840x2160()));
        edid.resize(256, 0);
        // CTA-861 extension block: preferred DTD = 1920×1080 at offset 108.
        edid[128] = 0x02; // extension tag
        edid[129] = 3; // revision
        edid[130] = 108; // DTD offset (relative to block start)
        edid[131] = 0x81; // native DTD count = 1
        edid[236..236 + 18].copy_from_slice(&dtd_1920x1080());
        assert_eq!(
            native_resolution_from_edid(&edid),
            Some((1920, 1080, "CEA-861"))
        );
    }

    #[test]
    fn cta_native_svd_fallback_without_dtd_area() {
        let mut edid = base_block(1, None);
        edid.resize(256, 0);
        // CTA block with no DTD area (offset 0) but a native SVD: VIC 96 = 3840×2160.
        edid[128] = 0x02;
        edid[129] = 3;
        edid[130] = 0; // no DTD area
        edid[131] = 0x81; // native DTD count = 1 (native SVD still honored)
        edid[132] = 0x42; // Video Data Block header: tag 2, length 2
        edid[133] = 0xE0; // native bit (0x80) | VIC 96 (0x60)
        assert_eq!(
            native_resolution_from_edid(&edid),
            Some((3840, 2160, "CEA-861"))
        );
    }

    #[test]
    fn cta_skips_non_video_dtd_and_uses_next_dtd() {
        let mut edid = base_block(1, None);
        edid.resize(256, 0);
        edid[128] = 0x02;
        edid[129] = 3;
        edid[130] = 72; // DTDs at 200 and 218 (both fit within the block)
        edid[131] = 0x81;
        // First DTD slot is a monitor descriptor (zero pixel clock) → skipped.
        edid[200..200 + 18].copy_from_slice(&[0u8; 18]);
        // Second DTD slot is a real video timing.
        edid[218..218 + 18].copy_from_slice(&dtd_3840x2160());
        assert_eq!(
            native_resolution_from_edid(&edid),
            Some((3840, 2160, "CEA-861"))
        );
    }

    #[test]
    fn resolution_for_vic_mapping() {
        assert_eq!(resolution_for_vic(16), Some((1920, 1080)));
        assert_eq!(resolution_for_vic(96), Some((3840, 2160)));
        assert_eq!(resolution_for_vic(98), Some((4096, 2160)));
        assert_eq!(resolution_for_vic(0), None);
        assert_eq!(resolution_for_vic(255), None);
    }

    fn serial_descriptor(serial: &str) -> [u8; 18] {
        let mut d = [0u8; 18];
        d[3] = 0xFF; // Display Product Serial Number tag
        for (i, b) in serial.bytes().take(13).enumerate() {
            d[5 + i] = b;
        }
        d
    }

    #[test]
    fn edid_serial_parses_ff_descriptor() {
        let mut edid = base_block(0, None);
        edid[54..54 + 18].copy_from_slice(&serial_descriptor("ABC1234567890"));
        assert_eq!(
            edid_serial_from_blob(&edid),
            Some("ABC1234567890".to_string())
        );
    }

    #[test]
    fn edid_serial_scans_all_four_descriptor_blocks() {
        let mut edid = base_block(0, None);
        // Serial lives in the third block (offset 90) instead of the first.
        edid[90..90 + 18].copy_from_slice(&serial_descriptor("SN-7788"));
        assert_eq!(edid_serial_from_blob(&edid), Some("SN-7788".to_string()));
    }

    #[test]
    fn edid_serial_trims_padding() {
        let mut edid = base_block(0, None);
        let mut d = serial_descriptor("SN-5   "); // 13 bytes, trailing spaces
        d[12] = 0x0A; // some panels pad with a newline instead of spaces
        edid[54..54 + 18].copy_from_slice(&d);
        assert_eq!(edid_serial_from_blob(&edid), Some("SN-5".to_string()));
    }

    #[test]
    fn edid_serial_absent_returns_none() {
        // Name descriptor (0xFC) and range descriptor (0xFD), no serial.
        let mut edid = base_block(0, None);
        let mut name = serial_descriptor("LG OLED C4");
        name[3] = 0xFC;
        edid[54..54 + 18].copy_from_slice(&name);
        edid[72..72 + 18].copy_from_slice(&serial_descriptor("            ")); // blank serial
        assert_eq!(edid_serial_from_blob(&edid), None);
    }

    #[test]
    fn edid_serial_ignores_dtd_with_tag_byte() {
        // A video timing whose tag byte happens to be 0xFF must not be read as
        // a serial: nonzero pixel clock disqualifies it.
        let mut edid = base_block(0, Some(dtd_3840x2160()));
        edid[57] = 0xFF; // force the tag byte (pixel clock bytes 0-1 nonzero)
        assert_eq!(edid_serial_from_blob(&edid), None);
    }

    #[test]
    fn edid_serial_rejects_truncated_blob() {
        assert_eq!(edid_serial_from_blob(&[0u8; 64]), None);
        assert_eq!(edid_serial_from_blob(&[]), None);
    }
}
