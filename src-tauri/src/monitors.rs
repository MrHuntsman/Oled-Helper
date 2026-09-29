// monitors.rs, shared per-monitor enumeration for the whole app.
//
// One list of connected monitors with everything a tab or the Explorer hook
// needs:
//   • friendly_name, the CCD/EDID name (e.g. "LG OLED C4") for UI labels
//   • sig, the device-caps signature (resolution + physical size + DPI), the
//     stable key per-monitor assignments are persisted against and what the
//     hook matches at draw time
//   • rect + primary, desktop geometry and role
//   • hmonitor, the system-wide monitor handle the hook matches window DCs
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
    GetMonitorInfoW, ReleaseDC, DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP,
    ENUM_CURRENT_SETTINGS, GET_DEVICE_CAPS_INDEX, HDC, HMONITOR, HORZRES, HORZSIZE, LOGPIXELSX,
    LOGPIXELSY, MONITORINFO, VERTRES, VERTSIZE,
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
    /// GDI display device name (e.g. r"\\.\DISPLAY1"), what CreateDCW needs
    /// for per-monitor gamma. Empty when the device could not be matched.
    pub device_name: String,
    /// EDID device ID (e.g. r"MONITOR\LGD05XX\{...}"), the frontend's
    /// per-monitor profile keys use this. Empty when unavailable. Two
    /// identical panels (same model, no unique EDID serial) can share it,
    /// which is why profiles key off `profile_id` instead.
    pub monitor_id: String,
    /// CCD connector instance (DISPLAYCONFIG_TARGET_DEVICE_NAME
    /// connectorInstance): the OS's per-connector unique id that tells two
    /// identical panels apart. 0 when CCD is unavailable.
    pub connector_instance: u32,
    /// EDID "Display Product Serial Number" (descriptor tag 0xFF), the
    /// per-unit unique panel id: stable across ports, replugs, driver resets
    /// and reboots. Empty when the panel ships no serial or the EDID can't be
    /// read. This is the first-choice disambiguator for identical panels; the
    /// connector instance and device name are the fallbacks.
    pub edid_serial: String,
    /// The identity per-monitor profiles are keyed against. Same as
    /// monitor_id (or the sig) normally; when two connected monitors share
    /// the same model (EDID base), the panel's serial wins as the suffix
    /// ("<base>@serial=<serial>"), with the connector instance ("<id>@<hex>") or
    /// GDI device name ("<id>@dev=<name>") as fallbacks, so each identical
    /// panel resolves as its own profile instead of sharing one.
    pub profile_id: String,
}

impl AppMonitor {
    /// Stable across sessions so config assignments survive restarts and
    /// monitor reconnects: resolution + physical size + DPI identify a panel.
    /// The format is load-bearing, persisted per-monitor keys use it.
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
    // ALSO NULL (documented), so GetDeviceCaps here would be meaningless -
    // the caller must pass the screen DC (see all_monitors).
    let list = &mut *(lparam.0 as *mut Vec<AppMonitor>);
    let caps = |index: GET_DEVICE_CAPS_INDEX| GetDeviceCaps(Some(hdc), index).max(0) as u32;
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let primary =
        GetMonitorInfoW(hmon, &mut info).as_bool() && (info.dwFlags & MONITORINFOF_PRIMARY) != 0;
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
        connector_instance: 0,
        edid_serial: String::new(),
        profile_id: String::new(),
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
    let ccd = ccd_info_by_rect();
    for m in &mut list {
        let key = (
            m.rect.left,
            m.rect.top,
            m.rect.right - m.rect.left,
            m.rect.bottom - m.rect.top,
        );
        if let Some(info) = ccd.get(&key) {
            m.friendly_name = info.name.clone();
        }
    }
    let devices = display_devices_by_position();
    for m in &mut list {
        if let Some((name, id)) = devices.get(&(m.rect.left, m.rect.top)) {
            m.device_name = name.clone();
            m.monitor_id = id.clone();
        }
    }
    // EDID serials: read for every monitor (cached per device instance id in
    // display_info). The identity uses them only for duplicate-model groups,
    // but every monitor carries one so the frontend/backend matchers can
    // re-key a serial-based entry when a duplicated panel ends up alone (one
    // of a pair unplugged) and for diagnostics.
    for m in &mut list {
        if !m.monitor_id.is_empty() {
            m.edid_serial =
                crate::display_info::edid_serial_for_monitor(&m.monitor_id).unwrap_or_default();
        }
    }
    // Profile identities, serial-first. Identical panels share the EDID base
    // (the MONITOR\<model> part of the id, and in the worst case the whole
    // id), so group by base and disambiguate each group jointly: the EDID
    // serial (per-unit unique, stable across ports/replugs) wins when every
    // member has its own; the CCD connector instance distinguishes same-type
    // connectors next; the GDI device name (port-ordered but unique while
    // connected) is the last resort. Only duplicate-model panels get a
    // suffix, so single-monitor profiles keep their exact keys (no
    // migration). A group whose disambiguation fails (e.g. both panels
    // report the same serial) falls back to the per-panel ids rather than
    // merging two panels into one profile.
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, m) in list.iter().enumerate() {
        if !m.monitor_id.is_empty() {
            groups
                .entry(monitor_base(&m.monitor_id))
                .or_default()
                .push(i);
        }
    }
    for members in groups.values() {
        if members.len() < 2 {
            continue;
        }
        let serials: Vec<&str> = members
            .iter()
            .filter_map(|&i| {
                let serial = list[i].edid_serial.as_str();
                (!serial.is_empty()).then_some(serial)
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        let serials_distinct =
            serials.len() == members.len() && serials.iter().all(|s| seen.insert(*s));
        for &i in members {
            let m = &mut list[i];
            let key = (
                m.rect.left,
                m.rect.top,
                m.rect.right - m.rect.left,
                m.rect.bottom - m.rect.top,
            );
            m.connector_instance = ccd.get(&key).map(|c| c.connector_instance).unwrap_or(0);
            let base = m.monitor_id.clone();
            if serials_distinct {
                m.profile_id = format!("{}@serial={}", monitor_base(&m.monitor_id), m.edid_serial);
            } else if m.connector_instance != 0 {
                m.profile_id = format!("{base}@{:x}", m.connector_instance);
            } else if !m.device_name.is_empty() {
                m.profile_id = format!("{base}@dev={}", m.device_name);
            } else {
                m.profile_id = base;
            }
        }
    }
    // Monitors outside a duplicate-model group keep their exact identity
    // keys: the bare EDID id when available, else the sig. The duplicate
    // loop above only assigns profile_id for grouped (same-model) panels, so
    // without this a single monitor would resolve to an EMPTY key and every
    // per-monitor profile lookup (taskbar dimmer, screen dimmer, gamma)
    // would miss the frontend's EDID-id keys and silently fall back to
    // defaults — sliders that do nothing, and a dimmer that ignores live
    // seconds changes.
    for m in &mut list {
        if m.profile_id.is_empty() {
            m.profile_id = if !m.monitor_id.is_empty() {
                m.monitor_id.clone()
            } else {
                m.sig()
            };
        }
    }
    list
}

// The first two components of a monitor device id (MONITOR\<EDID base>): the
// model identity shared by identical panels. e.g. r"MONITOR\LGD05XX\5&...&UID1"
// -> r"MONITOR\LGD05XX".
fn monitor_base(monitor_id: &str) -> String {
    let mut parts = monitor_id.split('\\');
    match (parts.next(), parts.next()) {
        (Some(a), Some(b)) => format!("{a}\\{b}"),
        _ => monitor_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::monitor_base;

    #[test]
    fn monitor_base_extracts_model_prefix() {
        assert_eq!(
            monitor_base(r"MONITOR\LGD05XX\5&2b2d6f29&0&UID43567"),
            r"MONITOR\LGD05XX"
        );
        // Two components only (no instance part) is its own base.
        assert_eq!(monitor_base(r"MONITOR\LGD05XX"), r"MONITOR\LGD05XX");
        // Malformed / empty ids pass through untouched.
        assert_eq!(monitor_base(""), "");
        assert_eq!(monitor_base("SINGLE"), "SINGLE");
    }
}

/// The connected monitors for the frontend monitor selector. `sig` is the
/// stable key per-monitor settings are persisted against; `profile_id` is the
/// identity per-monitor profile keys use (the EDID id, disambiguated for
/// identical panels — serial first, then connector, then device name — so
/// each resolves as its own profile); `monitor_id` is the bare EDID id.
#[derive(serde::Serialize)]
pub struct ListedMonitor {
    pub sig: String,
    pub friendly_name: String,
    pub primary: bool,
    pub width: u32,
    pub height: u32,
    pub monitor_id: String,
    pub device_name: String,
    /// EDID serial (per-unit unique panel id), empty when the panel ships
    /// none. Diagnostics: the identity itself is `profile_id`.
    pub edid_serial: String,
    pub profile_id: String,
    /// Current refresh rate in Hz (0 when the device is not queryable), so
    /// the frontend can key per-monitor profiles by the monitor's OWN rate
    /// instead of the primary's.
    pub refresh_hz: u32,
}

#[tauri::command]
pub fn list_monitors() -> Vec<ListedMonitor> {
    all_monitors()
        .into_iter()
        .map(|m| ListedMonitor {
            refresh_hz: current_refresh_hz(&m.device_name),
            sig: m.sig(),
            friendly_name: m.friendly_name,
            primary: m.primary,
            width: m.width,
            height: m.height,
            monitor_id: m.monitor_id,
            device_name: m.device_name,
            edid_serial: m.edid_serial,
            profile_id: m.profile_id,
        })
        .collect()
}

// Current refresh rate (Hz) of a GDI display device, from its current-mode
// DEVMODE. 0 when the device name is empty or the query fails.
fn current_refresh_hz(device_name: &str) -> u32 {
    if device_name.is_empty() {
        return 0;
    }
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let name_wide: Vec<u16> = device_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe {
        EnumDisplaySettingsW(PCWSTR(name_wide.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool()
    };
    if !ok {
        return 0;
    }
    dm.dmDisplayFrequency as u32
}

/// The resolution-independent runtime key for a monitor: its profile_id (the
/// EDID device id, disambiguated for identical panels), falling back to the
/// sig. Matches the frontend's key builders and the
/// persisted per-monitor profile keying, so the gamma table behaves like
/// every other per-monitor feature instead of embedding the current
/// resolution (which orphans entries on every mode switch).
pub(crate) fn monitor_key(m: &AppMonitor) -> String {
    m.profile_id.clone()
}

/// Resolve a persisted per-monitor key to its current canonical profile id:
/// match the connected monitors by profile id, EDID id, sig, or the
/// resolution-independent physical-size+DPI portion of a legacy sig, and
/// return the monitor's profile_id. This migrates entries saved under a bare
/// EDID id (before identical panels were disambiguated) or an old sig to the
/// identity the live monitors now use; unresolved keys pass through
/// unchanged. For a duplicated id the first matching monitor wins, so a
/// previously shared entry follows one panel and the other starts at the
/// defaults.
pub(crate) fn canonical_profile_id(key: &str) -> String {
    let monitors = all_monitors();
    // Which EDID ids are shared by more than one connected monitor: an
    // @-suffixed old key embedding a SHARED id is ambiguous between the
    // siblings, so it only migrates when the panel is no longer duplicated.
    let mut id_counts: HashMap<&str, u32> = HashMap::new();
    for m in &monitors {
        if !m.monitor_id.is_empty() {
            *id_counts.entry(m.monitor_id.as_str()).or_insert(0) += 1;
        }
    }
    if let Some(monitor) = monitors.iter().find(|m| {
        m.profile_id == key
            || (!m.monitor_id.is_empty() && m.monitor_id == key)
            || m.sig() == key
            // A serial-keyed identity ("<base>@serial=<serial>") matches the
            // panel whose serial it carries — the serial is per-unit, so the
            // match is precise even when the panel is currently alone (one of
            // a duplicated pair unplugged) or re-keyed differently.
            || (!m.monitor_id.is_empty()
                && !m.edid_serial.is_empty()
                && key == format!("{}@serial={}", monitor_base(&m.monitor_id), m.edid_serial))
            // A stale @-suffixed variant ("<id>@connector" / "<id>@dev=...")
            // from before a duplicate-count change. Only matches while this
            // monitor is NOT itself disambiguated (profile_id == monitor_id)
            // or its id is not shared, so a still-duplicated sibling cannot
            // steal its entry.
            || (!m.monitor_id.is_empty()
                && key.starts_with(&format!("{}@", m.monitor_id))
                && (m.profile_id == m.monitor_id
                    || id_counts.get(m.monitor_id.as_str()).copied().unwrap_or(0) <= 1))
    }) {
        return monitor.profile_id.clone();
    }
    let stable = stable_sig_part(key);
    monitors
        .into_iter()
        .find(|m| stable_sig_part(&m.sig()) == stable)
        .map(|m| m.profile_id)
        .unwrap_or_else(|| key.to_string())
}

/// Re-key a persisted per-monitor profile map to the monitors' current
/// profile identities. Called when the display topology changes so a
/// duplicate-count shift (one of two identical panels unplugged, or a second
/// one plugged in) cannot strand the runtime maps on stale keys.
pub(crate) fn rekey_profile_map<T>(map: &mut HashMap<String, T>) {
    if map.is_empty() {
        return;
    }
    let rekeyed: Vec<(String, T)> = map
        .drain()
        .map(|(key, value)| (canonical_profile_id(&key), value))
        .collect();
    map.extend(rekeyed);
}

// The resolution-independent portion of a sig ("WxH|PxP|DxD" -> "PxP|DxD"):
// physical size + DPI survive mode switches, so a legacy sig key still
// identifies its panel after a resolution change.
fn stable_sig_part(sig: &str) -> String {
    let parts: Vec<&str> = sig.split('|').collect();
    if parts.len() >= 3 {
        format!("{}|{}", parts[1], parts[2])
    } else {
        sig.to_string()
    }
}

/// The profile identity for the monitor on the given GDI device
/// (e.g. r"\\.\DISPLAY1"), so display-info consumers (the tray, the status
/// bar) key by the same identity the monitor list uses. Empty when the
/// device is not connected.
pub fn profile_id_for_device(device_name: &str) -> String {
    all_monitors()
        .into_iter()
        .find(|m| m.device_name.eq_ignore_ascii_case(device_name))
        .map(|m| m.profile_id)
        .unwrap_or_default()
}

/// Resolve the monitor a per-monitor gamma apply targets. The GDI device
/// name (r"\\.\DISPLAY1") uniquely identifies a physical monitor even when
/// two identical panels share the same EDID id AND the same device-caps
/// signature (same resolution, same physical size, same DPI), so it wins
/// when the caller has it; the sig/id match is the fallback for callers
/// without a device name. A stale sig (the frontend racing a mode change)
/// does not match, so the caller gets an error rather than misapplying a
/// value to an identically-sized panel.
pub(crate) fn resolve_monitor_for_apply(
    sig: &str,
    device_name: Option<&str>,
) -> Option<AppMonitor> {
    let monitors = all_monitors();
    if let Some(device) = device_name {
        if let Some(monitor) = monitors
            .iter()
            .find(|m| m.device_name.eq_ignore_ascii_case(device))
        {
            return Some(monitor.clone());
        }
    }
    monitors
        .into_iter()
        .find(|m| m.sig() == sig || (!m.monitor_id.is_empty() && m.monitor_id == sig))
}

// GDI display devices (adapter device name + monitor EDID id) keyed by the
// monitor's current-mode desktop position, so they can be joined to the
// monitor rects: the position from EnumDisplaySettingsW is the same
// virtual-desktop space as the EnumDisplayMonitors rect, so (left, top) pins
// each monitor to its device. The adapter's DeviceName is what CreateDCW
// needs for gamma writes; only the monitor devices carry the EDID id
// (MONITOR\...) the frontend persists per-monitor profiles against, the
// adapter's own DeviceID is its PCI instance id, not a monitor identity.
fn display_devices_by_position() -> HashMap<(i32, i32), (String, String)> {
    let mut map = HashMap::new();
    // SAFETY: EnumDisplayDevicesW(NULL) enumerates the display adapters, then
    // each attached adapter enumerates the monitors attached to it. Per the
    // docs the monitor loop starts at iDevNum 0 and DeviceName changes on
    // each call, so the adapter name is saved separately. Only attached
    // devices (those on the desktop) get their current-mode position.
    unsafe {
        let mut index = 0u32;
        loop {
            let mut adapter = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut adapter, 0).as_bool() {
                break;
            }
            index += 1;
            if (adapter.StateFlags.0 & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP.0) == 0 {
                continue;
            }
            let adapter_name = trim_utf16(&adapter.DeviceName);
            let adapter_name_wide: Vec<u16> = adapter_name
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            for monitor_index in 0u32.. {
                let mut monitor = DISPLAY_DEVICEW {
                    cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                    ..Default::default()
                };
                if !EnumDisplayDevicesW(
                    PCWSTR(adapter_name_wide.as_ptr()),
                    monitor_index,
                    &mut monitor,
                    0,
                )
                .as_bool()
                {
                    break;
                }
                if (monitor.StateFlags.0 & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP.0) == 0 {
                    continue;
                }
                // The monitor's own device name (e.g. \\.\DISPLAY1\Monitor0)
                // resolves its per-monitor current-mode position; fall back to
                // the adapter's position (identical for the usual
                // one-monitor-per-adapter case) when the monitor name is not
                // queryable.
                let monitor_name = trim_utf16(&monitor.DeviceName);
                let pos = current_mode_position(&monitor_name)
                    .or_else(|| current_mode_position(&adapter_name));
                let Some((x, y)) = pos else { continue };
                map.insert(
                    (x, y),
                    (adapter_name.clone(), trim_utf16(&monitor.DeviceID)),
                );
            }
        }
    }
    map
}

// Current-mode desktop position (virtual-desktop coordinates, the same space
// as the EnumDisplayMonitors rects) of a display device, an adapter name
// (\\.\DISPLAY1) or a monitor device name (\\.\DISPLAY1\Monitor0).
fn current_mode_position(device_name: &str) -> Option<(i32, i32)> {
    // EnumDisplaySettingsW expects dmSize set to sizeof(DEVMODEW), matching
    // display_info.rs's fresh_devmode.
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let name_wide: Vec<u16> = device_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe {
        EnumDisplaySettingsW(PCWSTR(name_wide.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool()
    };
    if !ok {
        return None;
    }
    // DEVMODEW models the union-typed fields through Anonymous1/2;
    // dmPosition lives in the second anonymous member. The unsafe union read
    // is valid after EnumDisplaySettingsW filled the struct.
    // SAFETY: the union member is read only after a successful call that
    // populates it.
    unsafe {
        let pos = dm.Anonymous1.Anonymous2.dmPosition;
        Some((pos.x, pos.y))
    }
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

/// Friendly name (CCD/EDID) of the monitor on the given GDI device
/// (e.g. r"\\.\DISPLAY2"), for the per-monitor tooltip in the top-bar
/// selector. Falls back to the primary's name when the device is the
/// primary, so both paths agree on the same source.
pub fn friendly_name_for_device(device_name: &str) -> Option<String> {
    all_monitors()
        .into_iter()
        .find(|m| m.device_name.eq_ignore_ascii_case(device_name))
        .map(|m| m.friendly_name)
        .filter(|n| !n.is_empty())
}

// CCD per-target info (friendly name + connector instance) keyed by
// (left, top, width, height) of the monitor's desktop rect, so they can be
// joined to the EnumDisplayMonitors pass (the source-mode position/size is
// the same virtual-desktop coordinate space as the monitor rect).
struct CcdTargetInfo {
    name: String,
    connector_instance: u32,
}

fn ccd_info_by_rect() -> HashMap<(i32, i32, i32, i32), CcdTargetInfo> {
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
        let key = (
            src.position.x,
            src.position.y,
            src.width as i32,
            src.height as i32,
        );
        map.insert(
            key,
            CcdTargetInfo {
                name: target_monitor_name(&name),
                connector_instance: name.connectorInstance,
            },
        );
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
                    return Err(
                        "display topology kept changing while querying display paths".into(),
                    );
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

pub(crate) fn target_device_name(
    path: &DISPLAYCONFIG_PATH_INFO,
) -> Option<DISPLAYCONFIG_TARGET_DEVICE_NAME> {
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
