// display_info.rs — queries and changes the current display mode via Win32.
// Browser APIs (window.screen) can report resolution but never expose the real
// refresh rate or the complete set of supported display modes.

use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsW, EnumDisplayDevicesW, EnumDisplaySettingsW, DEVMODEW,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, CDS_UPDATEREGISTRY,
    DISP_CHANGE_SUCCESSFUL, DISPLAY_DEVICEW, DISPLAY_DEVICE_PRIMARY_DEVICE,
    DM_DISPLAYFREQUENCY, DM_PELSWIDTH, DM_PELSHEIGHT,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiOpenDevRegKey, SP_DEVINFO_DATA, DIGCF_PRESENT,
    DIREG_DEV, DICS_FLAG_GLOBAL,
};
use windows::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, KEY_QUERY_VALUE};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{GUID, PCWSTR};
use tauri::{AppHandle, Emitter};

// Runtime-log bridge: same channel the taskbar dimmer uses, so EDID
// diagnostics appear in the Debug tab's Runtime Log (via the "runtime-log"
// event mirrored in main.js) and in the on-disk log.
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

pub fn init_app_handle(handle: AppHandle) {
    let _ = APP_HANDLE.set(handle);
}

fn log_edid(level: &str, message: String) {
    match level {
        "warn" => log::warn!("[display-info] {message}"),
        "error" => log::error!("[display-info] {message}"),
        _ => log::info!("[display-info] {message}"),
    }
    if let Some(app) = APP_HANDLE.get() {
        let _ = app.emit("runtime-log", crate::taskbar_dimmer::RuntimeLogEvent {
            level: level.to_string(),
            message: format!("[display-info] {message}"),
        });
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

fn fresh_devmode() -> DEVMODEW {
    DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    }
}

fn mode_matches_display(mode: &DEVMODEW, current: &DEVMODEW) -> bool {
    mode.dmBitsPerPel == current.dmBitsPerPel
        && mode.dmDisplayFrequency > 0
        && mode.dmPelsWidth > 0
        && mode.dmPelsHeight > 0
}

fn for_each_display_mode(mut callback: impl FnMut(&DEVMODEW) -> bool) {
    for mode_index in 0u32.. {
        let mut mode = fresh_devmode();
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR::null(),
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
    width: u32,
    height: u32,
    bits_per_pixel: u32,
    refresh_rates: Vec<u32>,
    resolutions: Vec<ResolutionInfo>,
    captured_at: Instant,
}

static MODE_CACHE: OnceLock<Mutex<Option<CachedModeLists>>> = OnceLock::new();

fn mode_lists_for(current: &DEVMODEW) -> (Vec<u32>, Vec<ResolutionInfo>) {
    let cache = MODE_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref().filter(|cached| {
            cached.width == current.dmPelsWidth
                && cached.height == current.dmPelsHeight
                && cached.bits_per_pixel == current.dmBitsPerPel
                && cached.captured_at.elapsed() < Duration::from_secs(2)
        }) {
            return (cached.refresh_rates.clone(), cached.resolutions.clone());
        }
    }

    let mut rates = Vec::new();
    for_each_display_mode(|mode| {
        // Keep the active resolution and colour depth fixed: changing only
        // the frequency avoids accidentally switching pixel format, which is
        // especially important while HDR is active.
        if mode_matches_display(mode, current)
            && mode.dmPelsWidth == current.dmPelsWidth
            && mode.dmPelsHeight == current.dmPelsHeight
        {
            rates.push(mode.dmDisplayFrequency);
        }
        true
    });
    if current.dmDisplayFrequency > 0 {
        rates.push(current.dmDisplayFrequency);
    }
    rates.sort_unstable();
    rates.dedup();

    let mut resolutions = Vec::new();
    for_each_display_mode(|mode| {
        if mode_matches_display(mode, current)
            && !resolutions.iter().any(|resolution: &ResolutionInfo| {
                resolution.width == mode.dmPelsWidth && resolution.height == mode.dmPelsHeight
            })
        {
            resolutions.push(ResolutionInfo {
                width: mode.dmPelsWidth,
                height: mode.dmPelsHeight,
            });
        }
        true
    });

    if !resolutions.iter().any(|resolution| {
        resolution.width == current.dmPelsWidth && resolution.height == current.dmPelsHeight
    }) {
        resolutions.push(ResolutionInfo {
            width: current.dmPelsWidth,
            height: current.dmPelsHeight,
        });
    }
    resolutions.sort_unstable_by_key(|resolution| (resolution.width, resolution.height));

    if let Ok(mut guard) = cache.lock() {
        *guard = Some(CachedModeLists {
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
fn current_devmode() -> Option<DEVMODEW> {
    for attempt in 0..5 {
        unsafe {
            let mut devmode = fresh_devmode();
            let ok = EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut devmode);
            if ok.as_bool()
                && devmode.dmDisplayFrequency > 0
                && devmode.dmPelsWidth > 0
                && devmode.dmPelsHeight > 0
            {
                return Some(devmode);
            }
        }
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    None
}

pub fn query_current_display() -> Option<DisplayInfo> {
    // PCWSTR::null() targets the primary/default display, matching the
    // existing display-info behaviour in the frontend.
    let devmode = current_devmode()?;
    let (refresh_rates, resolutions) = mode_lists_for(&devmode);
    let (native_width, native_height, native_source) = native_resolution()
        .map(|(w, h, source)| (w, h, source.to_string()))
        .unwrap_or((0, 0, String::new()));
    Some(DisplayInfo {
        width: devmode.dmPelsWidth,
        height: devmode.dmPelsHeight,
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

// Looks for a CTA-861 (CEA) extension block and returns its preferred
// timing — the first video DTD in the block. Many TVs (e.g. LG OLEDs) report
// 4096×2160 as the base-block DTD1 but mark 3840×2160 as the CTA preferred
// timing, which is exactly the resolution Windows shows as "(Recommended)".
fn cea_preferred_resolution(edid: &[u8]) -> Option<(u32, u32)> {
    // Extension blocks start at byte 128, one 128-byte block each.
    let mut block = 128usize;
    while block + 128 <= edid.len() {
        let tag = edid[block];
        if tag == 0x00 {
            break; // end of extension blocks
        }
        if tag == 0x02 {
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

            // Offset 0 means this CTA block has no DTD area. Valid DTDs
            // begin at block + dtd_offset, not block + 4 + dtd_offset.
            if !(4..127).contains(&dtd_offset) {
                log_edid("warn", format!(
                    "EDID CTA: invalid DTD offset {} (expected 4..126)",
                    dtd_offset,
                ));
                return None;
            }

            let mut dtd = block + dtd_offset;
            let dtd_end = block + 127; // exclude the extension checksum byte
            let mut dtd_index = 0usize;
            let mut first_native = None;
            let mut first_valid = None;
            while dtd + 18 <= dtd_end {
                let raw = &edid[dtd..dtd + 18];
                let raw_hex = raw.iter().map(|byte| format!("{byte:02X}")).collect::<Vec<_>>().join("");
                if let Some(res) = decode_dtd(raw) {
                    log_edid("info", format!(
                        "EDID CTA: DTD index={} offset={} native={} raw={} decoded={}x{}",
                        dtd_index,
                        dtd - block,
                        dtd_index < native_dtd_count,
                        raw_hex,
                        res.0,
                        res.1,
                    ));
                    if first_valid.is_none() {
                        first_valid = Some(res);
                    }
                    if dtd_index < native_dtd_count && first_native.is_none() {
                        first_native = Some(res);
                    }
                } else {
                    log_edid("info", format!(
                        "EDID CTA: skipped non-video DTD index={} offset={} raw={}",
                        dtd_index,
                        dtd - block,
                        raw_hex,
                    ));
                }
                dtd += 18;
                dtd_index += 1;
            }
            if let Some(res) = first_native.or(first_valid) {
                log_edid("info", format!(
                    "EDID CTA: selected native timing {}x{}",
                    res.0, res.1,
                ));
                return Some(res);
            }
            log_edid("warn", format!(
                "EDID CTA: no usable DTD found between offsets {} and 126",
                dtd_offset,
            ));
            return None; // CTA block found but no usable timing
        }
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
    captured_at: Instant,
}

static NATIVE_CACHE: OnceLock<Mutex<Option<CachedNative>>> = OnceLock::new();

fn native_resolution() -> Option<(u32, u32, &'static str)> {
    let cache = NATIVE_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard
            .as_ref()
            .filter(|c| c.captured_at.elapsed() < Duration::from_secs(60))
        {
            return Some((cached.width, cached.height, cached.source));
        }
    }
    let native = query_native_resolution();
    if let Ok(mut guard) = cache.lock() {
        if let Some((width, height, source)) = native {
            *guard = Some(CachedNative {
                width,
                height,
                source,
                captured_at: Instant::now(),
            });
        }
    }
    native
}

fn query_native_resolution() -> Option<(u32, u32, &'static str)> {
    unsafe {
        // 1. Find the primary display adapter (the DISPLAY_DEVICE entry with
        //    DISPLAY_DEVICE_PRIMARY_DEVICE set).
        let mut disp = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let mut found_primary = false;
        for index in 0.. {
            let ok = EnumDisplayDevicesW(PCWSTR::null(), index, &mut disp, 0);
            if !ok.as_bool() {
                break;
            }
            if (disp.StateFlags.0 & DISPLAY_DEVICE_PRIMARY_DEVICE.0) != 0 {
                found_primary = true;
                break;
            }
        }
        if !found_primary {
            log_edid("warn", "EDID query: no primary display adapter found".into());
            return None;
        }
        let adapter_name = String::from_utf16_lossy(&disp.DeviceName)
            .trim_end_matches('\0')
            .to_string();
        log_edid("info", format!("EDID query: primary adapter '{adapter_name}'"));

        // 2. Enumerate the monitors attached to the primary adapter. The
        //    monitor's DeviceID is already the SetupAPI device instance id
        //    (DISPLAY\\VID\\INST) — the documented pattern is a plain
        //    enumeration here, NOT an EDD_GET_DEVICE_INTERFACE_NAME call on
        //    the adapter (that returns the adapter's own interface, not the
        //    monitor's). This was the earlier bug: the interface-path
        //    conversion never matched, so the EDID was never read.
        let mut mon = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let dev_name = PCWSTR(disp.DeviceName.as_ptr());
        // The first monitor enumerated on the primary adapter is the primary
        // monitor. Its DeviceID is the SetupAPI device instance id directly.
        if !EnumDisplayDevicesW(dev_name, 0, &mut mon, 0).as_bool() {
            log_edid("warn", "EDID query: no monitor found on primary adapter".into());
            return None;
        }
        let instance_id = String::from_utf16_lossy(&mon.DeviceID)
            .trim_end_matches('\0')
            .to_string();
        let monitor_name = String::from_utf16_lossy(&mon.DeviceName)
            .trim_end_matches('\0')
            .to_string();
        log_edid("info", format!(
            "EDID query: primary monitor '{monitor_name}' instance id '{instance_id}'"
        ));

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
            log_edid("warn", "EDID query: SetupDiGetClassDevsW(GUID_CLASS_MONITOR) failed".into());
            return None;
        };
        // Reads the EDID registry value for a monitor device and decodes the
        // native resolution from it. Shared by the exact-match path and the
        // single-monitor fallback below.
        //
        // The EDID value's location varies by driver generation: most store it
        // at the device key root (DIREG_DEV), but some put it in the
        // "Device Parameters" subkey, so try both before giving up.
        fn read_edid_from_device(devices: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO, dev_data: &SP_DEVINFO_DATA) -> Option<(u32, u32, &'static str)> {
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
                            log_edid("info", format!(
                                "EDID query: read {} bytes of EDID from device key",
                                clamped,
                            ));
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
                log_edid("info", format!(
                    "EDID query: enumerating monitor device '{id}' (looking for '{instance_id}')"
                ));
                if id == instance_id {
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
                    "EDID query: no exact match for '{instance_id}'; using the only present monitor '{id}'"
                ));
                result = read_edid_from_device(devices, &dev_data);
            } else {
                log_edid("warn", format!(
                    "EDID query: no SetupAPI monitor device matched instance id '{instance_id}'"
                ));
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(devices);
        match result {
            Some((w, h, src)) => {
                log_edid("info", format!("EDID query: native = {w}x{h} via {src}"));
            }
            None => log_edid("warn", "EDID query: no usable native resolution found".into()),
        }
        result
    }
}

pub fn set_refresh_rate(hz: u32) -> Result<(), String> {
    let mut devmode =
        current_devmode().ok_or_else(|| "unable to read the current display mode".to_string())?;
    if !mode_lists_for(&devmode).0.contains(&hz) {
        return Err(format!("{hz} Hz is not available at the current resolution"));
    }

    devmode.dmDisplayFrequency = hz;
    devmode.dmFields |= DM_DISPLAYFREQUENCY;
    unsafe { apply_display_mode(&devmode) }
}

pub fn set_resolution(width: u32, height: u32, preferred_hz: u32) -> Result<(), String> {
    let current =
        current_devmode().ok_or_else(|| "unable to read the current display mode".to_string())?;
    if !mode_lists_for(&current).1.iter().any(|resolution| {
        resolution.width == width && resolution.height == height
    }) {
        return Err(format!("{width}×{height} is not available on this display"));
    }

    let mut selected = None;
    let mut fallback = None;
    for_each_display_mode(|mode| {
        if mode_matches_display(mode, &current)
            && mode.dmPelsWidth == width
            && mode.dmPelsHeight == height
        {
            if mode.dmDisplayFrequency == preferred_hz {
                selected = Some(*mode);
                return false;
            }
            if fallback.is_none() {
                fallback = Some(*mode);
            }
        }
        true
    });

    let mut target = selected.or(fallback).ok_or_else(|| {
        format!("no usable display mode found for {width}×{height}")
    })?;
    // The enumerated DEVMODE is a complete mode, but explicitly mark the
    // fields being changed so ChangeDisplaySettingsW applies both the
    // resolution and its selected refresh rate.
    target.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    unsafe { apply_display_mode(&target) }
}

unsafe fn apply_display_mode(devmode: &DEVMODEW) -> Result<(), String> {
    let result = ChangeDisplaySettingsW(
        Some(devmode as *const DEVMODEW),
        CDS_UPDATEREGISTRY,
    );
    if result == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        Err(format!("Windows rejected the display-mode change (code {})", result.0))
    }
}
