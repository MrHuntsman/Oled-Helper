// hdr_toggle.rs — toggles real Windows "HDR" (Settings > System > Display >
// HDR) via the CCD (DisplayConfig) API, the same mechanism used by
// AutoActions/HDRTray. This is NOT the in-app calibration mode — it flips
// the actual Windows Advanced Color state on the display, which is what was
// asked for instead of a synthetic Win+Alt+B keypress (unreliable: needs
// focus, can be remapped, silently no-ops on some setups).
//
// Flow (mirrors GetDisplayConfigBufferSizes -> QueryDisplayConfig ->
// DisplayConfigGetDeviceInfo/SetDeviceInfo from the Win32 docs):
//   1. Ask CCD for the buffer sizes needed for the current topology.
//   2. QueryDisplayConfig fills path/mode arrays for the active paths.
//   3. Walk paths, get advanced color info per target to find current
//      HDR support/enabled state.
//   4. DisplayConfigSetDeviceInfo with DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE
//      flips it on the target(s) that support HDR.
//
// NOTE ON BITFIELDS: the windows-rs bindgen for
// DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO_0_0 / DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE_0_0
// does NOT generate named accessor methods for this crate version — both are
// just a raw `_bitfield: u32` (confirmed against the docs.rs page for this
// exact type). So bits are read/written manually via the union's `value: u32`
// field. Bit layout per wingdi.h and cross-checked against other open-source
// callers (e.g. Kodi's xbmc PR #26140):
//   GET struct — bit 0: advancedColorSupported, bit 1: advancedColorEnabled,
//                bit 2: wideColorEnforced, bit 3: advancedColorForceDisabled
//   SET struct — bit 0: enableAdvancedColor

use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LUID, WIN32_ERROR,
};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    REG_VALUE_TYPE,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, DISPLAY_DEVICEW, DISPLAY_DEVICE_PRIMARY_DEVICE,
};
use windows::Win32::Graphics::Gdi::{
    DISPLAYCONFIG_COLOR_ENCODING, DISPLAYCONFIG_COLOR_ENCODING_INTENSITY,
    DISPLAYCONFIG_COLOR_ENCODING_RGB, DISPLAYCONFIG_COLOR_ENCODING_YCBCR420,
    DISPLAYCONFIG_COLOR_ENCODING_YCBCR422, DISPLAYCONFIG_COLOR_ENCODING_YCBCR444,
};

#[derive(serde::Serialize, Clone, Copy, PartialEq, Eq)]
pub struct HdrCapability {
    pub supported: bool,
    pub enabled: bool,
}

fn query_active_paths(
) -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), String> {
    const MAX_TOPOLOGY_RETRIES: usize = 4;
    unsafe {
        for attempt in 0..=MAX_TOPOLOGY_RETRIES {
            let mut path_count: u32 = 0;
            let mut mode_count: u32 = 0;
            let sizes_result = GetDisplayConfigBufferSizes(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                &mut mode_count,
            );
            if sizes_result != ERROR_SUCCESS {
                return Err(format!(
                    "GetDisplayConfigBufferSizes failed: {sizes_result:?}"
                ));
            }

            let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> =
                vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
                vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];

            let query_result = QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            );

            // Topology can change between the size query and the fetch (e.g. a
            // display waking up); retry once more if the buffer is now too small.
            if query_result == ERROR_INSUFFICIENT_BUFFER {
                if attempt == MAX_TOPOLOGY_RETRIES {
                    return Err("display topology kept changing while querying HDR state".into());
                }
                std::thread::yield_now();
                continue;
            }
            if query_result != ERROR_SUCCESS {
                return Err(format!("QueryDisplayConfig failed: {query_result:?}"));
            }

            paths.truncate(path_count as usize);
            modes.truncate(mode_count as usize);
            return Ok((paths, modes));
        }
        Err("unable to query active display paths".into())
    }
}

fn get_advanced_color_info(
    path: &DISPLAYCONFIG_PATH_INFO,
) -> Option<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO> {
    unsafe {
        let mut info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                size: std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        let result = DisplayConfigGetDeviceInfo(&mut info.header);
        if WIN32_ERROR(result as u32) != ERROR_SUCCESS {
            return None;
        }
        Some(info)
    }
}

// See the NOTE ON BITFIELDS module comment above for why these read the raw
// `value: u32` instead of calling generated accessor methods (there are none).
fn advanced_color_supported(info: &DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO) -> bool {
    (unsafe { info.Anonymous.value } & 0x1) != 0
}
fn advanced_color_enabled(info: &DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO) -> bool {
    (unsafe { info.Anonymous.value } & 0x2) != 0
}

fn set_advanced_color_state(path: &DISPLAYCONFIG_PATH_INFO, enable: bool) -> Result<(), String> {
    unsafe {
        let mut state = DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
                size: std::mem::size_of::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // bit 0 = enableAdvancedColor — see module-level NOTE ON BITFIELDS.
        state.Anonymous.value = if enable { 0x1 } else { 0x0 };
        let result = DisplayConfigSetDeviceInfo(&state.header);
        if WIN32_ERROR(result as u32) != ERROR_SUCCESS {
            return Err(format!("DisplayConfigSetDeviceInfo failed: {result:?}"));
        }
        Ok(())
    }
}

/// Reports HDR support/enabled state for the first active display target
/// that supports Advanced Color (i.e. the first HDR-capable path found).
/// Returns None if no active display supports it at all.
pub fn query_hdr_capability() -> Result<Option<HdrCapability>, String> {
    let (paths, _modes) = query_active_paths()?;
    for path in &paths {
        if let Some(info) = get_advanced_color_info(path) {
            if advanced_color_supported(&info) {
                return Ok(Some(HdrCapability {
                    supported: true,
                    enabled: advanced_color_enabled(&info),
                }));
            }
        }
    }
    Ok(None)
}

/// Sets Windows Advanced Color (HDR) on/off for every active display target
/// that supports it. Mirrors what Settings > Display > HDR does when you
/// flip the switch. Returns an error string if no HDR-capable display was
/// found or the OS call failed.
pub fn set_windows_hdr(enable: bool) -> Result<(), String> {
    let (paths, _modes) = query_active_paths()?;
    let mut applied_any = false;
    let mut last_err: Option<String> = None;

    for path in &paths {
        let Some(info) = get_advanced_color_info(path) else {
            continue;
        };
        if !advanced_color_supported(&info) {
            continue;
        }
        match set_advanced_color_state(path, enable) {
            Ok(()) => applied_any = true,
            Err(e) => last_err = Some(e),
        }
    }

    if applied_any {
        // The HDR state flip changes which target qualifies as the primary
        // HDR target; drop the cached target so the next SDR white level
        // read re-resolves.
        invalidate_hdr_target_cache();
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| "no HDR-capable display found".to_string()))
    }
}

#[tauri::command]
pub fn get_windows_hdr_status() -> Result<Option<HdrCapability>, String> {
    query_hdr_capability()
}

#[tauri::command]
pub fn set_windows_hdr_enabled(enabled: bool) -> Result<(), String> {
    set_windows_hdr(enabled)
}

#[derive(serde::Serialize, Clone, Copy)]
pub struct SdrWhiteLevel {
    pub raw: u32,
    pub nits: f32,
}

fn text(value: &[u16]) -> String {
    String::from_utf16_lossy(value)
        .trim_end_matches('\0')
        .to_string()
}

fn primary_gdi_device_name() -> Option<String> {
    unsafe {
        for index in 0.. {
            let mut device = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(windows::core::PCWSTR::null(), index, &mut device, 0).as_bool()
            {
                break;
            }
            if (device.StateFlags.0 & DISPLAY_DEVICE_PRIMARY_DEVICE.0) != 0 {
                return Some(text(&device.DeviceName));
            }
        }
    }
    None
}

fn source_gdi_device_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    unsafe {
        let mut source = windows::Win32::Devices::Display::DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<
                    windows::Win32::Devices::Display::DISPLAYCONFIG_SOURCE_DEVICE_NAME,
                >() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        let result = DisplayConfigGetDeviceInfo(&mut source.header);
        if WIN32_ERROR(result as u32) == ERROR_SUCCESS {
            Some(text(&source.viewGdiDeviceName))
        } else {
            None
        }
    }
}

fn primary_hdr_path() -> Result<Option<DISPLAYCONFIG_PATH_INFO>, String> {
    let (paths, _modes) = query_active_paths()?;
    let primary = primary_gdi_device_name();
    Ok(paths.into_iter().find(|path| {
        let is_primary = primary
            .as_deref()
            .zip(source_gdi_device_name(path).as_deref())
            .map(|(want, actual)| want.eq_ignore_ascii_case(actual))
            .unwrap_or(false);
        is_primary
            && get_advanced_color_info(path)
                .map(|info| advanced_color_supported(&info) && advanced_color_enabled(&info))
                .unwrap_or(false)
    }))
}

// The primary-HDR target (adapterId + target id) is what the SDR white level
// GET/SET calls address, and it only changes when the display topology or HDR
// state changes. Resolving it via primary_hdr_path() re-walks the whole
// display configuration (GetDisplayConfigBufferSizes + QueryDisplayConfig +
// device enumeration + per-path DisplayConfigGetDeviceInfo) — real driver
// work that the Windows HDR tab's 500ms poll was paying on EVERY tick even
// though the target never moved. Cache the resolved target and only re-walk
// when it goes stale or the display topology changes.
struct CachedHdrTarget {
    captured_at: Instant,
    // None = HDR is off / no HDR-capable primary target. The timestamp is
    // cached even for the negative result so an HDR-off session doesn't
    // re-walk the topology on every 500ms poll — the frontend's refresh()
    // calls get_sdr_white_level unconditionally while the tab is open.
    target: Option<(LUID, u32)>,
}

static HDR_TARGET_CACHE: OnceLock<Mutex<Option<CachedHdrTarget>>> = OnceLock::new();
// The target is stable in practice; the TTL is a safety net for a missed
// display-change event, and lib.rs's notify_display_changed invalidates it
// eagerly on any topology change.
const HDR_TARGET_CACHE_TTL: Duration = Duration::from_secs(5);

// Returns the cached primary-HDR (adapterId, targetId), resolving it on cache
// miss. Ok(None) means HDR is off / no HDR-capable primary target.
fn cached_primary_hdr_target() -> Result<Option<(LUID, u32)>, String> {
    let cache = HDR_TARGET_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(cached) = guard
        .as_ref()
        .filter(|cached| cached.captured_at.elapsed() < HDR_TARGET_CACHE_TTL)
    {
        return Ok(cached.target);
    }
    let resolved = primary_hdr_path()?
        .map(|path| (path.targetInfo.adapterId, path.targetInfo.id));
    *guard = Some(CachedHdrTarget {
        captured_at: Instant::now(),
        target: resolved,
    });
    Ok(resolved)
}

pub fn invalidate_hdr_target_cache() {
    if let Some(cache) = HDR_TARGET_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
}

fn query_sdr_white_level() -> Result<Option<SdrWhiteLevel>, String> {
    let Some((adapter_id, target_id)) = cached_primary_hdr_target()? else {
        return Ok(None);
    };
    unsafe {
        let mut level = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: adapter_id,
                id: target_id,
            },
            ..Default::default()
        };
        let result = DisplayConfigGetDeviceInfo(&mut level.header);
        if WIN32_ERROR(result as u32) != ERROR_SUCCESS {
            // A stale cached target (HDR toggled off / display removed) makes
            // the read fail; drop the cache so the next poll re-resolves.
            invalidate_hdr_target_cache();
            return Err(format!(
                "DisplayConfigGetDeviceInfo(SDR white level) failed: {result:?}"
            ));
        }
        Ok(Some(SdrWhiteLevel {
            raw: level.SDRWhiteLevel,
            nits: level.SDRWhiteLevel as f32 * 80.0 / 1000.0,
        }))
    }
}

// Undocumented CCD device info type for SET SDR white level. The standard
// windows-rs crate only exposes the GET variant; this SET constant (matching
// the ledoge/set_maxtml reference implementation) is committed through
// DisplayConfigSetDeviceInfo.
const DISPLAYCONFIG_DEVICE_INFO_SET_SDR_WHITE_LEVEL_I: i32 = -18; // 0xFFFFFFEE

// The SET form carries an extra `finalValue` byte beyond the GET struct
// (DISPLAYCONFIG_SDR_WHITE_LEVEL). Must be #[repr(C)] so the FFI call sees
// the expected layout.
#[repr(C)]
#[allow(non_snake_case)]
struct DISPLAYCONFIG_SET_SDR_WHITE_LEVEL_INTERNAL {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    SDRWhiteLevel: u32,
    finalValue: u8,
}

/// Sets the SDR white level through the proper CCD (Connecting and Configuring
/// Display) API — the same mechanism Windows Settings uses. This updates the
/// full display pipeline including hardware video planes (MPO/overlays),
/// unlike DwmpSDRToHDRBoost which only updates DWM's composition surface.
///
/// The CCD API is clamped to roughly 480 nits (matching Windows Settings);
/// for values above that the caller should fall back to the DWM ordinal.
fn set_sdr_white_level_ccd(nits: f32) -> Result<SdrWhiteLevel, String> {
    if !nits.is_finite() || nits < 80.0 {
        return Err("SDR white level must be at least 80 nits".into());
    }
    let Some((adapter_id, target_id)) = cached_primary_hdr_target()? else {
        return Err("Windows HDR is not enabled on the primary display".into());
    };
    let raw = (nits * 1000.0 / 80.0).round() as u32;
    unsafe {
        let params = DISPLAYCONFIG_SET_SDR_WHITE_LEVEL_INTERNAL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_TYPE(
                    DISPLAYCONFIG_DEVICE_INFO_SET_SDR_WHITE_LEVEL_I,
                ),
                size: std::mem::size_of::<DISPLAYCONFIG_SET_SDR_WHITE_LEVEL_INTERNAL>() as u32,
                adapterId: adapter_id,
                id: target_id,
            },
            SDRWhiteLevel: raw,
            finalValue: 1,
        };
        let result = DisplayConfigSetDeviceInfo(&params.header);
        if WIN32_ERROR(result as u32) != ERROR_SUCCESS {
            invalidate_hdr_target_cache();
            return Err(format!(
                "DisplayConfigSetDeviceInfo(SDR white level) failed: {result:?}"
            ));
        }
    }
    Ok(SdrWhiteLevel { raw, nits })
}

#[tauri::command]
pub fn get_sdr_white_level() -> Result<Option<SdrWhiteLevel>, String> {
    query_sdr_white_level()
}

/// Sets the SDR white level through the CCD (Connecting and Configuring
/// Display) API — the same mechanism Windows Settings uses. This commits
/// through the proper display config pipeline, so DWM AND hardware video
/// planes (MPO/overlays) both pick up the new SDR white level.
/// Capped at 480 nits, matching Windows Settings' own range.
pub fn set_sdr_white_level(nits: f32) -> Result<SdrWhiteLevel, String> {
    if !nits.is_finite() || !(80.0..=480.0).contains(&nits) {
        return Err("SDR white level must be between 80 and 480 nits".into());
    }
    if cached_primary_hdr_target()?.is_none() {
        return Err("Windows HDR is not enabled on the primary display".into());
    }
    set_sdr_white_level_ccd(nits)
}

#[tauri::command]
pub fn set_sdr_brightness_nits(nits: f32) -> Result<SdrWhiteLevel, String> {
    set_sdr_white_level(nits)
}

#[derive(serde::Serialize, Clone, Copy)]
pub struct HdrLuminance {
    pub max_nits: Option<f32>,
}

// Queries HDR luminance capability for the PRIMARY display (matching the
// PCWSTR::null() primary-display target used elsewhere in the app) via DXGI:
//   IDXGIFactory1::EnumAdapters1 -> IDXGIAdapter1::EnumOutputs ->
//   IDXGIOutput::GetDesc (to match the monitor handle) ->
//   IDXGIOutput6::GetDesc1 (DXGI_OUTPUT_DESC1, luminance in nits directly).
//
// NOTE: the CCD/DisplayConfig API used by the HDR toggle above does NOT
// expose nit values (DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO_2 in wingdi.h has
// no luminance fields) — DXGI_OUTPUT_DESC1 is the correct source.
// ── Output info (connector + color format/depth) ─────────────────────────
// The status-bar tooltip on the monitor chip shows how the primary display
// is physically connected and what color pipeline it is running. Both come
// from the same CCD path walk used for the HDR toggle above:
//   • connector   — DISPLAYCONFIG_TARGET_DEVICE_NAME.outputTechnology
//   • color       — DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO.colorEncoding +
//                   bitsPerColorChannel (total RGB bits → per channel / 3)
// Output dynamic range (Full/Limited) and VRR/G-Sync state are NOT exposed
// by any public Win32 API (driver-internal), so they are intentionally
// omitted rather than guessed.
#[derive(serde::Serialize, Clone, Default)]
pub struct DisplayOutputInfo {
    // Full GPU model name of the adapter driving the primary display (DXGI
    // adapter description, e.g. "NVIDIA GeForce RTX 3080"). Vendor-agnostic
    // and available without a driver SDK — see vendor_info.rs.
    pub gpu_model: String,
    pub connector: String,
    pub color_format: String,
    pub bits_per_channel: u32,
    // Desktop color depth as Windows' classic Display Properties reports it:
    // GetDeviceCaps(BITSPIXEL) on the screen DC — 32 on virtually every
    // modern setup (True Color), regardless of the panel's signal bit depth.
    pub desktop_bits: u32,
    // Output dynamic range — driver-internal setting (NVIDIA/AMD control
    // panel). Queried via vendor DLLs (NVAPI/ADL/IGCL). Empty string when
    // the vendor DLL couldn't be loaded or the query failed.
    pub dynamic_range: String,
    // Variable refresh rate status — G-Sync/FreeSync/Adaptive-Sync.
    // Queried via vendor DLLs. Empty string on failure.
    pub vrr: String,
    // NVIDIA-only driver control state. False for non-NVIDIA GPUs and when
    // the optional vendor query/registry key is unavailable.
    pub reference_mode: bool,
}

fn connector_label(tech: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> &'static str {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_USB_TUNNEL,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_MIRACAST,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDTVDONGLE, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL,
    };
    match tech {
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15 => "VGA",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI => "DVI",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI => "HDMI",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS => "LVDS",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL => "DisplayPort",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED => "eDP",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_USB_TUNNEL => "USB-C DisplayPort",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL => "UDI",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED => "Embedded UDI",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDTVDONGLE => "TV dongle",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_MIRACAST => "Miracast",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL => "Internal",
        _ => "Unknown",
    }
}

fn color_encoding_label(enc: DISPLAYCONFIG_COLOR_ENCODING) -> &'static str {
    match enc {
        DISPLAYCONFIG_COLOR_ENCODING_RGB => "RGB",
        DISPLAYCONFIG_COLOR_ENCODING_YCBCR444 => "YCbCr 4:4:4",
        DISPLAYCONFIG_COLOR_ENCODING_YCBCR422 => "YCbCr 4:2:2",
        DISPLAYCONFIG_COLOR_ENCODING_YCBCR420 => "YCbCr 4:2:0",
        DISPLAYCONFIG_COLOR_ENCODING_INTENSITY => "Intensity",
        _ => "Unknown",
    }
}

// Friendly monitor name from a CCD target (trimmed UTF-16, no padding).
fn target_monitor_name(name: &DISPLAYCONFIG_TARGET_DEVICE_NAME) -> String {
    let end = name
        .monitorFriendlyDeviceName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(name.monitorFriendlyDeviceName.len());
    String::from_utf16_lossy(&name.monitorFriendlyDeviceName[..end])
}

fn target_device_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<DISPLAYCONFIG_TARGET_DEVICE_NAME> {
    unsafe {
        let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        let result = DisplayConfigGetDeviceInfo(&mut name.header);
        if WIN32_ERROR(result as u32) != ERROR_SUCCESS {
            return None;
        }
        Some(name)
    }
}

// Desktop color depth via the screen DC — matches the classic Windows
// Display Properties "32-bit" entry. Fallback 0 when the DC can't be made.
fn query_desktop_bits() -> u32 {
    use windows::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC, BITSPIXEL};
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;
    unsafe {
        let dc = GetDC(Some(GetDesktopWindow()));
        if dc.0.is_null() {
            return 0;
        }
        let bits = GetDeviceCaps(Some(dc), BITSPIXEL) as u32;
        let _ = ReleaseDC(Some(GetDesktopWindow()), dc);
        bits
    }
}

// Queries connector + color format/depth for the primary display, matching
// the CCD target against the primary monitor's EDID-backed friendly name
// (the same string the app shows in the status bar). Falls back to the
// first active path when the name lookup fails (e.g. single-display setups,
// or a mismatch between CCD and the EDID name). Returns None only when
// there are no active paths at all.
pub fn query_display_output_info(primary_monitor_name: &str) -> Option<DisplayOutputInfo> {
    let (paths, _modes) = query_active_paths().ok()?;
    let mut selected: Option<&DISPLAYCONFIG_PATH_INFO> = None;
    for path in &paths {
        let Some(name) = target_device_name(path) else {
            continue;
        };
        if !primary_monitor_name.is_empty() && target_monitor_name(&name) == primary_monitor_name {
            selected = Some(path);
            break;
        }
    }
    let path = selected.or_else(|| paths.first())?;
    let mut info = DisplayOutputInfo::default();
    if let Some(name) = target_device_name(path) {
        info.connector = connector_label(name.outputTechnology).to_string();
    }
    if let Some(aci) = get_advanced_color_info(path) {
        info.color_format = color_encoding_label(aci.colorEncoding).to_string();
        // bitsPerColorChannel is already bits-per-channel (10 for a 10-bit
        // panel) on Windows 11 24H2+ — report it as-is. (The older
        // DISPLAYCONFIG_COLOR_DEPTH values 24/30/36/48 were total RGB bits,
        // but the field in DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO is per
        // channel, confirmed against a 10-bit panel returning 10.)
        info.bits_per_channel = aci.bitsPerColorChannel;
    }
    info.desktop_bits = query_desktop_bits();
    let vendor = crate::vendor_info::query_vendor_display_info();
    info.gpu_model = vendor.gpu_model;
    info.dynamic_range = vendor.dynamic_range;
    info.vrr = vendor.vrr;
    info.reference_mode = vendor.reference_mode;
    Some(info)
}

pub fn query_hdr_luminance() -> Option<HdrLuminance> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
    use windows::Win32::Graphics::Gdi::{MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    unsafe {
        let primary: HMONITOR = MonitorFromWindow(GetDesktopWindow(), MONITOR_DEFAULTTOPRIMARY);
        let Ok(factory): windows::core::Result<IDXGIFactory1> = CreateDXGIFactory1() else {
            return None;
        };
        for adapter_index in 0u32.. {
            let Ok(adapter) = factory.EnumAdapters1(adapter_index) else {
                break;
            };
            for output_index in 0u32.. {
                let Ok(output) = adapter.EnumOutputs(output_index) else {
                    break;
                };
                // Only the output that is actually the primary monitor.
                let Ok(desc) = output.GetDesc() else {
                    continue;
                };
                if desc.Monitor != primary {
                    continue;
                }
                let Ok(output6) = output.cast::<IDXGIOutput6>() else {
                    return None;
                };
                let Ok(desc1) = output6.GetDesc1() else {
                    return None;
                };
                // DXGI reports 0.0 for fields the EDID/driver didn't provide.
                let val = |n: f32| if n > 0.0 { Some(n) } else { None };
                return Some(HdrLuminance {
                    max_nits: val(desc1.MaxLuminance),
                });
            }
        }
        None
    }
}

#[tauri::command]
pub fn get_hdr_luminance() -> Option<HdrLuminance> {
    query_hdr_luminance()
}

// ── Auto HDR (system-wide) ─────────────────────────────────────────────────
// Windows 11's system-wide Auto HDR switch is not exposed by any public Win32
// API. The Settings app stores it as a flag in the DirectX user GPU
// preferences value:
//
//   HKCU\Software\Microsoft\DirectX\UserGpuPreferences
//     DirectXUserGlobalSettings (REG_SZ) =
//       "AutoHDREnable=0;SwapEffectUpgradeEnable=1;VRROptimizeEnable=0;"
//
// The value is a semicolon-separated list of "Name=Value;" toggles (the same
// list that holds the "Optimizations for windowed games" flags). We rewrite
// only the AutoHDREnable token and preserve every other flag. Per-app
// overrides live in the same key as per-exe values and are left untouched.
//
// Caveat: the flag is read when an app starts, so the change only affects
// games launched afterwards — running games need a restart. It also only has
// an effect while Windows HDR is enabled on the display.
const AUTO_HDR_PREFS_SUBKEY: &str = r"Software\Microsoft\DirectX\UserGpuPreferences";
const AUTO_HDR_SETTINGS_VALUE: &str = "DirectXUserGlobalSettings";
const AUTO_HDR_FLAG_PREFIX: &str = "AutoHDREnable=";

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

// Reads the DirectXUserGlobalSettings string. Ok(None) when the key or value
// does not exist yet (fresh user profile) — callers treat that as "all flags
// off". A genuine registry failure is an Err so a write is never based on a
// silently-failed read (which would clobber the user's other flags).
fn read_directx_global_settings() -> Result<Option<String>, String> {
    unsafe {
        let mut key = HKEY::default();
        let open = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(wide(AUTO_HDR_PREFS_SUBKEY).as_ptr()),
            Some(0),
            KEY_QUERY_VALUE,
            &mut key,
        );
        if open == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if open != ERROR_SUCCESS || key.is_invalid() {
            return Err(format!(
                "RegOpenKeyExW({AUTO_HDR_PREFS_SUBKEY}) failed: {open:?}"
            ));
        }

        let value_name = wide(AUTO_HDR_SETTINGS_VALUE);
        let mut required = 0u32;
        let mut value_type = REG_VALUE_TYPE::default();
        let size_query = RegQueryValueExW(
            key,
            windows::core::PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut value_type),
            None,
            Some(&mut required),
        );
        if size_query == ERROR_FILE_NOT_FOUND {
            let _ = RegCloseKey(key);
            return Ok(None);
        }
        if size_query != ERROR_SUCCESS || required == 0 || required > 4096 {
            let _ = RegCloseKey(key);
            return Err(format!(
                "RegQueryValueExW size query failed: {size_query:?}"
            ));
        }

        // required is the byte count (UTF-16, null-terminated); the buffer is
        // sized in u16 units so a trailing partial unit stays in bounds.
        let mut buf = vec![0u16; (required as usize + 1) / 2];
        let mut read = required;
        let query = RegQueryValueExW(
            key,
            windows::core::PCWSTR(value_name.as_ptr()),
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

fn write_directx_global_settings(settings: &str) -> Result<(), String> {
    unsafe {
        let mut key = HKEY::default();
        // Create the key if this is a fresh profile; opening an existing key
        // behaves identically.
        let create = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(wide(AUTO_HDR_PREFS_SUBKEY).as_ptr()),
            None,
            windows::core::PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut key,
            None,
        );
        if create != ERROR_SUCCESS || key.is_invalid() {
            return Err(format!(
                "RegCreateKeyExW({AUTO_HDR_PREFS_SUBKEY}) failed: {create:?}"
            ));
        }

        // REG_SZ payload: UTF-16LE bytes including the null terminator.
        let mut data: Vec<u8> = Vec::with_capacity((settings.len() + 1) * 2);
        for unit in settings.encode_utf16() {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        data.extend_from_slice(&0u16.to_le_bytes());

        let value_name = wide(AUTO_HDR_SETTINGS_VALUE);
        let set = RegSetValueExW(
            key,
            windows::core::PCWSTR(value_name.as_ptr()),
            None,
            REG_SZ,
            Some(&data),
        );
        let _ = RegCloseKey(key);
        if set != ERROR_SUCCESS {
            return Err(format!(
                "RegSetValueExW({AUTO_HDR_SETTINGS_VALUE}) failed: {set:?}"
            ));
        }
        Ok(())
    }
}

fn parse_auto_hdr_enabled(settings: &str) -> bool {
    settings
        .split(';')
        .map(str::trim)
        .any(|token| token == "AutoHDREnable=1")
}

// Replaces the AutoHDREnable flag in place (or prepends it when the flag is
// absent), preserving every other flag in the value.
fn set_auto_hdr_flag(settings: &str, enabled: bool) -> String {
    let flag = format!("{AUTO_HDR_FLAG_PREFIX}{}", if enabled { 1 } else { 0 });
    let tokens: Vec<&str> = settings.split(';').filter(|t| !t.is_empty()).collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len() + 1);
    let mut replaced = false;
    for token in tokens {
        let token = token.trim();
        if token.starts_with(AUTO_HDR_FLAG_PREFIX) {
            out.push(flag.clone());
            replaced = true;
        } else {
            out.push(token.to_string());
        }
    }
    if !replaced {
        out.insert(0, flag);
    }
    format!("{};", out.join(";"))
}

#[tauri::command]
pub fn get_auto_hdr_enabled() -> Result<bool, String> {
    Ok(read_directx_global_settings()?
        .map(|s| parse_auto_hdr_enabled(&s))
        .unwrap_or(false))
}

#[tauri::command]
pub fn set_auto_hdr_enabled(enabled: bool) -> Result<(), String> {
    let existing = read_directx_global_settings()?.unwrap_or_default();
    write_directx_global_settings(&set_auto_hdr_flag(&existing, enabled))
}
