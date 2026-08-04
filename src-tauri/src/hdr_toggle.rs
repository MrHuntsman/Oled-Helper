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
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, WIN32_ERROR};

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
