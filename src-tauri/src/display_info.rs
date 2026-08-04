// display_info.rs — queries and changes the current display mode via Win32.
// Browser APIs (window.screen) can report resolution but never expose the real
// refresh rate or the complete set of supported display modes.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiOpenDevRegKey, DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DEV,
    SP_DEVINFO_DATA,
};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_UPDATEREGISTRY,
    DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_PRIMARY_DEVICE, DISP_CHANGE_SUCCESSFUL,
    DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
    ENUM_DISPLAY_SETTINGS_MODE,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, KEY_QUERY_VALUE,
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
    // Hot path first: the cache is keyed by the current mode's width/height/
    // bits-per-pixel, which the caller reads fresh from EnumDisplaySettingsW
    // each time. A display-key check would force a full EnumDisplayDevicesW
    // walk on every call, so the 2s TTL relies on the mode itself: a monitor
    // swap almost always changes the mode, and any residual miss self-corrects
    // on the next poll.
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

fn primary_display_key() -> String {
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
            let adapter_name = PCWSTR(adapter.DeviceName.as_ptr());
            let mut monitor = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if EnumDisplayDevicesW(adapter_name, 0, &mut monitor, 0).as_bool() {
                let id = String::from_utf16_lossy(&monitor.DeviceID);
                return id.trim_end_matches('\0').to_string();
            }
            let name = String::from_utf16_lossy(&adapter.DeviceName);
            return name.trim_end_matches('\0').to_string();
        }
    }
    String::new()
}

fn native_resolution() -> Option<(u32, u32, &'static str)> {
    let display_key = primary_display_key();
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
    let native = query_native_resolution();
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
            log_edid(
                "warn",
                "EDID query: no primary display adapter found".into(),
            );
            return None;
        }
        let adapter_name = String::from_utf16_lossy(&disp.DeviceName)
            .trim_end_matches('\0')
            .to_string();
        log_edid(
            "info",
            format!("EDID query: primary adapter '{adapter_name}'"),
        );

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
            log_edid(
                "warn",
                "EDID query: no monitor found on primary adapter".into(),
            );
            return None;
        }
        let instance_id = String::from_utf16_lossy(&mon.DeviceID)
            .trim_end_matches('\0')
            .to_string();
        let monitor_name = String::from_utf16_lossy(&mon.DeviceName)
            .trim_end_matches('\0')
            .to_string();
        log_edid(
            "info",
            format!("EDID query: primary monitor '{monitor_name}' instance id '{instance_id}'"),
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

pub fn set_refresh_rate(hz: u32) -> Result<(), String> {
    let mut devmode =
        current_devmode().ok_or_else(|| "unable to read the current display mode".to_string())?;
    if !mode_lists_for(&devmode).0.contains(&hz) {
        return Err(format!(
            "{hz} Hz is not available at the current resolution"
        ));
    }

    devmode.dmDisplayFrequency = hz;
    devmode.dmFields |= DM_DISPLAYFREQUENCY;
    unsafe { apply_display_mode(&devmode) }
}

pub fn set_resolution(width: u32, height: u32, preferred_hz: u32) -> Result<(), String> {
    let current =
        current_devmode().ok_or_else(|| "unable to read the current display mode".to_string())?;
    if !mode_lists_for(&current)
        .1
        .iter()
        .any(|resolution| resolution.width == width && resolution.height == height)
    {
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

    let mut target = selected
        .or(fallback)
        .ok_or_else(|| format!("no usable display mode found for {width}×{height}"))?;
    // The enumerated DEVMODE is a complete mode, but explicitly mark the
    // fields being changed so ChangeDisplaySettingsW applies both the
    // resolution and its selected refresh rate.
    target.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    unsafe { apply_display_mode(&target) }
}

unsafe fn apply_display_mode(devmode: &DEVMODEW) -> Result<(), String> {
    let result = ChangeDisplaySettingsW(Some(devmode as *const DEVMODEW), CDS_UPDATEREGISTRY);
    if result == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        Err(format!(
            "Windows rejected the display-mode change (code {})",
            result.0
        ))
    }
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
