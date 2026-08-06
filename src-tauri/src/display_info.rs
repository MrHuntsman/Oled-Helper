// display_info.rs — queries and changes the current display mode via Win32.
// Browser APIs (window.screen) can report resolution but never expose the real
// refresh rate or the complete set of supported display modes.

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
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_NORESET, CDS_TEST,
    CDS_TYPE, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_PRIMARY_DEVICE,
    DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, KEY_QUERY_VALUE,
};
use windows::Win32::System::Threading::{
    CreateMutexW, ReleaseMutex, WaitForSingleObject, INFINITE,
};

use windows::core::{GUID, PCWSTR};

// Every record goes through the standard `log` facade. The tauri-plugin-log
// targets fan that same record out to stdout, the rotating file, and the
// frontend `log://log` event consumed by the Debug tab and DevTools bridge.
fn log_edid(level: &str, message: String) {
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
    // The monitor's true native (EDID preferred timing) resolution, which is
    // what Windows marks as "(Recommended)". 0 when it could not be read.
    pub native_width: u32,
    pub native_height: u32,
    // Which EDID location supplied the native resolution ("CEA-861",
    // "base EDID", or "") — diagnostic info for the Debug tab.
    pub native_source: String,
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

fn write_recovery_record_until(
    target: &DisplayTarget,
    previous: &DEVMODEW,
    deadline_ms: u64,
    token: u64,
) -> Result<(), String> {
    let record = RecoveryRecord {
        device_name: target.device_name.clone(),
        monitor_id: target.monitor_id.clone(),
        width: previous.dmPelsWidth,
        height: previous.dmPelsHeight,
        refresh_hz: previous.dmDisplayFrequency,
        bits_per_pixel: previous.dmBitsPerPel,
        deadline_ms,
        token,
    };
    let path = recovery_path();
    let temp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
    fs::write(&temp, data)
        .map_err(|error| format!("could not save display recovery state: {error}"))?;
    fs::rename(&temp, &path)
        .map_err(|error| format!("could not publish display recovery state: {error}"))?;
    Ok(())
}

fn write_recovery_record(
    target: &DisplayTarget,
    previous: &DEVMODEW,
    token: u64,
) -> Result<(), String> {
    // Covers the maximum three-second mode stabilization window plus the
    // fifteen-second user confirmation window and a small crash margin.
    write_recovery_record_until(target, previous, now_ms() + 22_000, token)
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

fn restore_recovery_record(record: &RecoveryRecord) -> Result<(), String> {
    let detected = primary_display_target()
        .ok_or_else(|| "recovery could not detect the primary monitor".to_string())?;
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
        change_mode(&detected, &mode, CDS_TYPE(0))?;
    }
    wait_for_mode(&detected, &mode)
        .map(|_| ())
        .ok_or_else(|| "recovery mode was not verified after apply".to_string())
}

pub fn run_recovery_watchdog() {
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
        if latest.token != record.token || latest.deadline_ms != record.deadline_ms {
            return;
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
    unsafe {
        let mut adapter = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        for index in 0.. {
            if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut adapter, 0).as_bool() {
                break;
            }
            if (adapter.StateFlags.0 & DISPLAY_DEVICE_PRIMARY_DEVICE.0) == 0 {
                continue;
            }
            let device_name = text(&adapter.DeviceName);
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
                if (monitor.StateFlags.0 & DISPLAY_DEVICE_PRIMARY_DEVICE.0) != 0 {
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
    let target = primary_display_target()?;
    let devmode = current_devmode(&target.device_name)?;
    let (refresh_rates, resolutions) = mode_lists_for(&target, &devmode);
    let (native_width, native_height, native_source) = native_resolution()
        .map(|(w, h, source)| (w, h, source.to_string()))
        .unwrap_or((0, 0, String::new()));
    let monitor_name = edid_name_for_target(&target).unwrap_or(target.monitor_name);
    Some(DisplayInfo {
        width: devmode.dmPelsWidth,
        height: devmode.dmPelsHeight,
        device_name: target.device_name,
        monitor_name,
        monitor_id: target.monitor_id,
        native_width,
        native_height,
        native_source,
        refresh_hz: devmode.dmDisplayFrequency,
        refresh_rates,
        resolutions,
    })
}

// ── Native (recommended) resolution via EDID ──────────────────────────────
// Windows' Display Settings marks a resolution as "(Recommended)" based on
// the monitor's EDID preferred timing descriptor — there is no public API
// that returns "native resolution" directly. Read the primary display's EDID
// from the monitor device's registry key (SetupAPI) and decode the first
// Detailed Timing Descriptor, exactly the data Windows itself uses. Falls
// back to (0, 0) when the EDID can't be read; the frontend then uses the
// largest supported mode as a proxy.

// GUID_CLASS_MONITOR (not exported by the windows crate).
const GUID_CLASS_MONITOR: GUID = GUID::from_u128(0x4d36e96ee32511cebfc108002be10318);

// Decodes the horizontal/vertical active pixels from an 18-byte Detailed
// Timing Descriptor. Returns None when the block is not a video timing
// (pixel clock 0 — a monitor descriptor like name/serial/range limits).
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
// timing — a native CTA DTD first, then a native SVD. Many TVs (e.g. LG OLEDs)
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
        edid_monitor_name(&bytes[..(size as usize).min(bytes.len())])
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

fn native_resolution() -> Option<(u32, u32, &'static str)> {
    // Use the same target discovery as mode enumeration and the status bar;
    // native EDID data must never silently come from a different monitor.
    let target = primary_display_target()?;
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
    let native = query_native_resolution(&target);
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
                    // Driver stores the EDID under "Device Parameters" — open
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
                    Some(_) => only_device = None, // more than one — not unambiguous
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

#[derive(Clone, serde::Serialize)]
pub struct AppliedMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub monitor_name: String,
    pub confirmation_seconds: u32,
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

fn pending_mode() -> &'static Mutex<Option<PendingMode>> {
    PENDING_MODE.get_or_init(|| Mutex::new(None))
}

// Used by the tray confirmation fallback to avoid reopening the main window
// after the user has already confirmed or reverted the temporary mode.
fn mode_error(code: i32, phase: &str) -> String {
    format!("Windows rejected the {phase} display-mode change (code {code})")
}

fn target_is_current(target: &DisplayTarget) -> Result<(), String> {
    let current = primary_display_target()
        .ok_or_else(|| "the primary monitor is no longer available".to_string())?;
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
    // Apply without CDS_UPDATEREGISTRY. The mode is only committed after the
    // frontend confirms it, and a watchdog restores the previous mode if the
    // confirmation never arrives.
    // The old transaction was already rolled back; the current mode is
    // still the `previous` mode captured immediately before this attempt.
    if let Err(error) = unsafe { change_mode(&target, &mode, CDS_TYPE(0)) } {
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
        let restore = unsafe { change_mode(&target, &previous, CDS_TYPE(0)) };
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
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(15));
        // Serialize the automatic rollback with confirmation and manual
        // rollback. Without this guard, the timeout thread could restore the
        // old mode while the user is simultaneously committing the new one.
        let Ok(_operation_guard) = mode_operation_lock().lock() else {
            log::error!(
                "[display-info] automatic rollback could not acquire the mode operation lock"
            );
            return;
        };
        let Ok(_recovery_guard) = lock_recovery_state() else {
            log::error!("[display-info] automatic rollback could not lock recovery state");
            return;
        };
        let entry = pending_mode()
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().filter(|entry| entry.token == token).cloned());
        if let Some(entry) = entry {
            if let Err(error) = target_is_current(&entry.target) {
                log::error!("[display-info] automatic rollback refused: {error}");
                return;
            }
            match unsafe { change_mode(&entry.target, &entry.previous, CDS_TYPE(0)) } {
                Ok(()) if wait_for_mode(&entry.target, &entry.previous).is_some() => {
                    invalidate_mode_cache();
                    // Keep the in-memory transaction until recovery cleanup has
                    // succeeded. If cleanup fails, the watchdog and manual
                    // retry still retain the only recovery path.
                    if let Err(error) = clear_recovery_record_for(token) {
                        log::error!("[display-info] automatic rollback succeeded but recovery cleanup failed: {error}");
                        return;
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
                Ok(()) => {
                    log::error!("[display-info] automatic display-mode rollback did not reach the previous mode; keeping recovery state");
                }
                Err(error) => {
                    // Keep the pending transaction available for a manual
                    // retry instead of silently losing the only recovery mode.
                    log::error!("[display-info] automatic display-mode rollback failed: {error}");
                }
            }
        }
    });
    Ok(AppliedMode {
        width: mode.dmPelsWidth,
        height: mode.dmPelsHeight,
        refresh_hz: mode.dmDisplayFrequency,
        // Use the same EDID-backed friendly name shown by get_display_info;
        // DeviceString alone may only say "Generic PnP Monitor".
        monitor_name: edid_name_for_target(&target).unwrap_or(target.monitor_name),
        confirmation_seconds: 15,
    })
}

pub fn set_refresh_rate(hz: u32) -> Result<AppliedMode, String> {
    let target =
        primary_display_target().ok_or_else(|| "no primary monitor was detected".to_string())?;
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

pub fn set_resolution(width: u32, height: u32, preferred_hz: u32) -> Result<AppliedMode, String> {
    let target =
        primary_display_target().ok_or_else(|| "no primary monitor was detected".to_string())?;
    let current = current_devmode(&target.device_name)
        .ok_or_else(|| "unable to read the current display mode".to_string())?;
    // Validate the resolution and requested refresh rate together against one
    // freshly enumerated complete DEVMODE. Never infer target-resolution Hz
    // support from the rates of the resolution we are leaving.
    let mode = complete_mode_for(&target, &current, width, height, preferred_hz)?;
    begin_mode_change(target, current, mode)
}

fn same_mode(a: &DEVMODEW, b: &DEVMODEW) -> bool {
    a.dmPelsWidth == b.dmPelsWidth
        && a.dmPelsHeight == b.dmPelsHeight
        && a.dmDisplayFrequency == b.dmDisplayFrequency
        && a.dmBitsPerPel == b.dmBitsPerPel
}

pub fn confirm_mode_change() -> Result<(), String> {
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
    // Test the requested mode, then verify the actual active mode before making
    // the registry update permanent. All reads happen while the recovery lock
    // is held, avoiding a stale pre-lock validation race.
    target_is_current(&target)?;
    unsafe {
        change_mode(&target, &requested, CDS_TEST)?;
    }
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
    unsafe {
        // The requested mode is already active (verified above), so persist it
        // to the registry WITHOUT re-applying it. A plain CDS_UPDATEREGISTRY
        // call re-sets the same mode, which on HDMI/TV displays forces a
        // full signal resync that freezes the entire desktop (including the
        // cursor) for a moment. CDS_NORESET writes the settings to the
        // registry but does not take effect, so nothing on screen changes.
        change_mode(&target, &active, CDS_UPDATEREGISTRY | CDS_NORESET)?;
    }
    // Verify the persistent commit too. Keep the recovery record and pending
    // transaction if the driver does not settle on the committed mode. Since
    // CDS_NORESET did not change the mode, this confirms the previously
    // verified mode is still active.
    if wait_for_mode(&target, &active).is_none() {
        return Err("the display mode was committed but could not be verified afterward; recovery state was kept".into());
    }
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
    target_is_current(&entry.target)?;
    unsafe {
        change_mode(&entry.target, &entry.previous, CDS_TYPE(0))?;
    }
    if wait_for_mode(&entry.target, &entry.previous).is_none() {
        return Err("display driver did not restore the previous mode".into());
    }
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
    primary_display_target().map(|target| DisplayTargetInfo {
        device_name: target.device_name,
        monitor_name: target.monitor_name,
        monitor_id: target.monitor_id,
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
}
