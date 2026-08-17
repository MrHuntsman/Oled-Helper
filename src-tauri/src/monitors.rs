// monitors.rs — shared per-monitor enumeration for the whole app.
//
// One list of connected monitors with everything a tab or the Explorer hook
// needs:
//   • friendly_name — the CCD/EDID name (e.g. "LG OLED C4") for UI labels
//   • sig — the device-caps signature (resolution + physical size + DPI), the
//     stable key per-monitor assignments are persisted against and what the
//     hook matches at draw time
//   • rect + primary — desktop geometry and role
//   • hmonitor — the system-wide monitor handle the hook matches window DCs
//
// The CCD helpers (query_active_paths, target_device_name, target_monitor_name)
// were moved here from hdr_toggle.rs so future per-monitor tabs share one
// monitor list instead of re-enumerating differently.

use std::collections::HashMap;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{LPARAM, RECT, WIN32_ERROR};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplayMonitors, EnumDisplaySettingsW, GetDC, GetDeviceCaps,
    GetMonitorInfoW, HDC, HORZRES, HORZSIZE, LOGPIXELSX, LOGPIXELSY, MONITORINFO, ReleaseDC,
    VERTRES, VERTSIZE, DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP,
    ENUM_CURRENT_SETTINGS, HMONITOR, GET_DEVICE_CAPS_INDEX,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

/// One connected monitor, as seen by the app.
#[derive(Clone)]
pub struct AppMonitor {
    /// System-wide monitor handle (0 when unavailable). The hook matches
    /// window-DC draws against this.
    pub hmonitor: u32,
    /// Device resolution in pixels (GetDeviceCaps HORZRES/VERTRES).
    pub width: u32,
    pub height: u32,
    /// Physical panel size in millimetres (HORZSIZE/VERTSIZE).
    pub phys_width: u32,
    pub phys_height: u32,
    /// Device DPI (LOGPIXELSX/Y).
    pub dpi_x: u32,
    pub dpi_y: u32,
    pub primary: bool,
    /// Monitor bounds in virtual-desktop coordinates (EnumDisplayMonitors).
    pub rect: RECT,
    /// CCD/EDID friendly name (e.g. "LG OLED C4"); empty when CCD is
    /// unavailable.
    pub friendly_name: String,
    /// GDI display device name (e.g. r"\\.\DISPLAY1") — what CreateDCW needs
    /// for per-monitor gamma. Empty when the device could not be matched.
    pub device_name: String,
    /// EDID device ID (e.g. r"MONITOR\LGD05XX\{...}") — the frontend's
    /// per-monitor profile keys use this. Empty when unavailable.
    pub monitor_id: String,
}

impl AppMonitor {
    /// Stable across sessions so config assignments survive restarts and
    /// monitor reconnects: resolution + physical size + DPI identify a panel.
    /// The format is load-bearing — persisted per-monitor keys use it.
    pub fn sig(&self) -> String {
        format!(
            "{}x{}|{}x{}|{}x{}",
            self.width, self.height, self.phys_width, self.phys_height, self.dpi_x, self.dpi_y
        )
    }
}

unsafe extern "system" fn monitor_enum_proc(
    hmon: HMONITOR,
    hdc: HDC,
    rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    if hmon.0.is_null() || hdc.is_invalid() || rect.is_null() {
        return BOOL(1);
    }
    // With hdc = NULL passed to EnumDisplayMonitors the callback's hdc is
    // ALSO NULL (documented), so GetDeviceCaps here would be meaningless —
    // the caller must pass the screen DC (see all_monitors).
    let list = &mut *(lparam.0 as *mut Vec<AppMonitor>);
    let caps = |index: GET_DEVICE_CAPS_INDEX| GetDeviceCaps(Some(hdc), index).max(0) as u32;
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let primary = GetMonitorInfoW(hmon, &mut info).as_bool()
        && (info.dwFlags & MONITORINFOF_PRIMARY) != 0;
    list.push(AppMonitor {
        hmonitor: hmon.0 as usize as u32,
        width: caps(HORZRES),
        height: caps(VERTRES),
        phys_width: caps(HORZSIZE),
        phys_height: caps(VERTSIZE),
        dpi_x: caps(LOGPIXELSX),
        dpi_y: caps(LOGPIXELSY),
        primary,
        rect: *rect,
        friendly_name: String::new(),
        device_name: String::new(),
        monitor_id: String::new(),
    });
    BOOL(1)
}

/// All connected monitors, with CCD friendly names joined in by desktop rect.
pub fn all_monitors() -> Vec<AppMonitor> {
    let mut list: Vec<AppMonitor> = Vec::new();
    // SAFETY: GetDC(NULL) yields the virtual-screen DC so the callback
    // receives each monitor's own display DC (hdcMonitor is NULL when the
    // enumeration hdc is NULL); GetDC/ReleaseDC are paired, and lparam
    // carries the collect list.
    unsafe {
        let screen = GetDC(None);
        if !screen.is_invalid() {
            let _ = EnumDisplayMonitors(
                Some(screen),
                None,
                Some(monitor_enum_proc),
                LPARAM(&mut list as *mut Vec<AppMonitor> as isize),
            );
            let _ = ReleaseDC(None, screen);
        }
    }
    let names = ccd_names_by_rect();
    for m in &mut list {
        let key = (
            m.rect.left,
            m.rect.top,
            m.rect.right - m.rect.left,
            m.rect.bottom - m.rect.top,
        );
        if let Some(name) = names.get(&key) {
            m.friendly_name = name.clone();
        }
    }
    let devices = display_devices_by_position();
    for m in &mut list {
        if let Some((name, id)) = devices.get(&(m.rect.left, m.rect.top)) {
            m.device_name = name.clone();
            m.monitor_id = id.clone();
        }
    }
    list
}

/// The connected monitors for the frontend monitor selector. `sig` is the
/// stable key per-monitor settings are persisted against; `monitor_id` is the
/// EDID device id the Black Crush profile keys use.
#[derive(serde::Serialize)]
pub struct ListedMonitor {
    pub sig: String,
    pub friendly_name: String,
    pub primary: bool,
    pub width: u32,
    pub height: u32,
    pub monitor_id: String,
    pub device_name: String,
}

#[tauri::command]
pub fn list_monitors() -> Vec<ListedMonitor> {
    all_monitors()
        .into_iter()
        .map(|m| ListedMonitor {
            sig: m.sig(),
            friendly_name: m.friendly_name,
            primary: m.primary,
            width: m.width,
            height: m.height,
            monitor_id: m.monitor_id,
            device_name: m.device_name,
        })
        .collect()
}

/// Maps each connected monitor's stable signature to its GDI display device
/// name (r"\\.\DISPLAY1"), for per-monitor gamma writes. Monitors whose
/// device could not be matched are omitted.
pub fn monitor_device_names() -> HashMap<String, String> {
    all_monitors()
        .into_iter()
        .filter(|m| !m.device_name.is_empty())
        .map(|m| (m.sig(), m.device_name))
        .collect()
}

// GDI display devices (device name + EDID id) keyed by the device's
// current-mode desktop position, so they can be joined to the monitor rects:
// the position from EnumDisplaySettingsW is the same virtual-desktop space as
// the EnumDisplayMonitors rect, so (left, top) pins each monitor to its device.
fn display_devices_by_position() -> HashMap<(i32, i32), (String, String)> {
    let mut map = HashMap::new();
    // SAFETY: EnumDisplayDevicesW(NULL) enumerates the display devices; only
    // attached devices (those on the desktop) get their current-mode position.
    unsafe {
        let mut index = 0u32;
        loop {
            let mut dev = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut dev, 0).as_bool() {
                break;
            }
            index += 1;
            if (dev.StateFlags.0 & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP.0) == 0 {
                continue;
            }
            let name = trim_utf16(&dev.DeviceName);
            let mut dm = DEVMODEW::default();
            let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            if !EnumDisplaySettingsW(PCWSTR(name_wide.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm)
                .as_bool()
            {
                continue;
            }
            // DEVMODEW models the union-typed fields through Anonymous1/2;
            // dmPosition lives in the second anonymous member. The unsafe
            // union read is already covered by the enclosing unsafe block.
            let pos = dm.Anonymous1.Anonymous2.dmPosition;
            map.insert((pos.x, pos.y), (name, trim_utf16(&dev.DeviceID)));
        }
    }
    map
}

fn trim_utf16(raw: &[u16]) -> String {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end]).trim().to_string()
}

/// Friendly name of the primary monitor (CCD/EDID, e.g. "LG OLED C4"), when
/// CCD provides one. The Display tab's status bar uses this so the name shown
/// and the CCD name matched for output info come from the same source.
pub fn primary_friendly_name() -> Option<String> {
    all_monitors()
        .into_iter()
        .find(|m| m.primary)
        .map(|m| m.friendly_name)
        .filter(|n| !n.is_empty())
}

// CCD friendly names keyed by (left, top, width, height) of the monitor's
// desktop rect, so they can be joined to the EnumDisplayMonitors pass (the
// source-mode position/size is the same virtual-desktop coordinate space as
// the monitor rect).
fn ccd_names_by_rect() -> HashMap<(i32, i32, i32, i32), String> {
    let mut map = HashMap::new();
    let Ok((paths, modes)) = query_active_paths() else {
        return map;
    };
    for path in &paths {
        let Some(name) = target_device_name(path) else {
            continue;
        };
        // SAFETY: modeInfoIdx indexes the mode array returned with the paths;
        // the active source mode is the union's sourceMode variant.
        let idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
        if idx >= modes.len() {
            continue;
        }
        let src = unsafe { modes[idx].Anonymous.sourceMode };
        let key = (src.position.x, src.position.y, src.width as i32, src.height as i32);
        map.insert(key, target_monitor_name(&name));
    }
    map
}

// ── CCD path helpers (moved from hdr_toggle.rs, shared) ─────────────────

pub(crate) fn query_active_paths(
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
            if sizes_result != WIN32_ERROR(0) {
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
            if query_result == windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER {
                if attempt == MAX_TOPOLOGY_RETRIES {
                    return Err("display topology kept changing while querying display paths".into());
                }
                std::thread::yield_now();
                continue;
            }
            if query_result != windows::Win32::Foundation::ERROR_SUCCESS {
                return Err(format!("QueryDisplayConfig failed: {query_result:?}"));
            }

            paths.truncate(path_count as usize);
            modes.truncate(mode_count as usize);
            return Ok((paths, modes));
        }
        Err("unable to query active display paths".into())
    }
}

pub(crate) fn target_device_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<DISPLAYCONFIG_TARGET_DEVICE_NAME> {
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
        if WIN32_ERROR(result as u32) != windows::Win32::Foundation::ERROR_SUCCESS {
            return None;
        }
        Some(name)
    }
}

/// Friendly monitor name from a CCD target (trimmed UTF-16, no padding).
pub(crate) fn target_monitor_name(name: &DISPLAYCONFIG_TARGET_DEVICE_NAME) -> String {
    let end = name
        .monitorFriendlyDeviceName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(name.monitorFriendlyDeviceName.len());
    String::from_utf16_lossy(&name.monitorFriendlyDeviceName[..end])
}
