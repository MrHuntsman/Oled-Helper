// power.rs — power saving controls: display/sleep timeouts, the screen saver,
// an instant "turn off display" action, and a screen protector that fades the
// display to black via a fullscreen overlay after a configurable idle timeout
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

use std::fs;
use std::path::PathBuf;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

use serde::Serialize;
use windows::core::{BOOL, GUID, PCWSTR};
use windows::Win32::Foundation::{COLORREF, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, FillRect, GetStockObject, InvalidateRect, BLACK_BRUSH, HBRUSH,
    PAINTSTRUCT,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE,
    REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateCursor, CreateWindowExW, DefWindowProcW, EnumWindows, GetClassNameW, GetSystemMetrics,
    GetTopWindow, GetWindow, GetWindowLongPtrW, GetWindowTextW, IsWindowVisible, PostMessageW,
    RegisterClassW, SetLayeredWindowAttributes, SetSystemCursor, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, SystemParametersInfoW,
    HTTRANSPARENT, HWND_BROADCAST, HWND_TOPMOST, LWA_ALPHA, SC_MONITORPOWER,
    SPI_GETSCREENSAVEACTIVE, SPI_GETSCREENSAVETIMEOUT, SPI_SETSCREENSAVEACTIVE,
    SPI_SETSCREENSAVETIMEOUT, SPI_SETCURSORS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_HIDE, SW_SHOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SPIF_UPDATEINIFILE,
    WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT, WM_SYSCOMMAND, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, GW_HWNDNEXT, GWLP_HWNDPARENT, HCURSOR, SYSTEM_CURSOR_ID,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_ICON, OCR_NO, OCR_NORMAL,
    OCR_SIZENS, OCR_SIZENESW, OCR_SIZENWSE, OCR_SIZE, OCR_SIZEALL, OCR_SIZEWE, OCR_UP, OCR_WAIT,
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
// PowerWriteDCValue export — powrprof only exposes the *ValueIndex write
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
        SystemParametersInfoW(
            SPI_SETSCREENSAVETIMEOUT,
            seconds,
            None,
            update_flags(),
        )
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
            return Err(format!("RegQueryValueExW size query failed: {size_query:?}"));
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
// A background watcher polls the input-idle time (GetLastInputInfo) against a
// user-configured timeout; once the user has been idle that long, the display
// fades to black and stays dimmed until any input arrives, which fades it
// back up. Unlike the old "dim before off", this is its own protector: it
// does not depend on Windows' display-off timeout.
//
// The fade is rendered with a fullscreen always-on-top click-through native
// overlay window (alpha via SetLayeredWindowAttributes), NOT a gamma ramp:
// SetDeviceGammaRamp does not apply under Windows HDR, and the overlay works
// in HDR. Black crush's gamma lift lives underneath the overlay, so the two
// features coexist with no interaction.

const POLL_MS: u64 = 50;
const MIN_SCREEN_PROTECTOR_SECONDS: u32 = 5;
const MAX_SCREEN_PROTECTOR_SECONDS: u32 = 180;
const DEFAULT_SCREEN_PROTECTOR_OPACITY: u32 = 800;
const DEFAULT_SCREEN_PROTECTOR_FADE_IN_MS: u32 = 2000;
const DEFAULT_SCREEN_PROTECTOR_FADE_OUT_MS: u32 = 1000;

// The screen protector rests at a partial dim by default. The target is stored
// as the inverse dim-factor level so opacity remains a simple 0..100% setting
// while the existing eased brightness curve stays unchanged.
static DIM_TARGET_LEVEL: AtomicU32 = AtomicU32::new(290);
static DIM_OPACITY: AtomicU32 = AtomicU32::new(DEFAULT_SCREEN_PROTECTOR_OPACITY);
static DIM_FADE_IN_MS: AtomicU32 = AtomicU32::new(DEFAULT_SCREEN_PROTECTOR_FADE_IN_MS);
static DIM_FADE_OUT_MS: AtomicU32 = AtomicU32::new(DEFAULT_SCREEN_PROTECTOR_FADE_OUT_MS);

static DIM_ENABLED: AtomicBool = AtomicBool::new(false);
static DIM_TIMEOUT_SECONDS: AtomicU32 = AtomicU32::new(30);
// Current dim level, DIM_TARGET_LEVEL (dimmed) .. 1000 (full). Stays 1000
// while idle is below the timeout, so the overlay stays transparent.
static DIM_LEVEL: AtomicU32 = AtomicU32::new(1000);
static DIM_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static DIM_THREAD: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();

fn last_input_ms() -> u32 {
    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if !GetLastInputInfo(&mut lii).as_bool() {
            return 0;
        }
        lii.dwTime
    }
}

fn idle_seconds() -> u32 {
    let now = unsafe { GetTickCount64() } as u32;
    // dwTime and GetTickCount64 both wrap at ~49.7 days; wrapping_sub
    // makes the difference correct across the wrap.
    now.wrapping_sub(last_input_ms()) / 1000
}

// Map the linear dim level (0..1000) to an eased brightness factor. The raw
// level advances in equal steps, but a straight linear mapping looks steppy and
// jumps at the ends; smoothstep (ease-in-out) starts and settles gently and
// moves fastest through the middle, which reads as one fluid transition.
fn dim_factor(level: u32) -> f32 {
    let t = (level.min(1000) as f32) / 1000.0;
    t * t * (3.0 - 2.0 * t)
}

// Convert the requested black-overlay opacity to the level used by the eased
// brightness curve. A short binary search avoids introducing a second easing
// curve and keeps the displayed opacity numerically consistent.
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

fn transition_steps(duration_ms: u32) -> u32 {
    ((duration_ms as u64 + POLL_MS - 1) / POLL_MS).max(1) as u32
}

fn transition_step(current: u32, target: u32, duration_ms: u32) -> u32 {
    if current == target || duration_ms == 0 {
        return current.abs_diff(target);
    }
    let ticks = transition_steps(duration_ms);
    ((current.abs_diff(target) + ticks - 1) / ticks).max(1)
}

fn configure_screen_protector(
    enabled: bool,
    seconds: u32,
    opacity: f64,
    fade_in_duration_ms: u64,
    fade_out_duration_ms: u64,
) {
    let opacity = opacity.clamp(0.0, 1.0);
    DIM_ENABLED.store(enabled, Ordering::Release);
    DIM_TIMEOUT_SECONDS.store(
        seconds.clamp(MIN_SCREEN_PROTECTOR_SECONDS, MAX_SCREEN_PROTECTOR_SECONDS),
        Ordering::Release,
    );
    DIM_OPACITY.store((opacity * 1000.0).round() as u32, Ordering::Release);
    DIM_TARGET_LEVEL.store(dim_level_for_opacity(opacity), Ordering::Release);
    DIM_FADE_IN_MS.store(fade_in_duration_ms.min(5000) as u32, Ordering::Release);
    DIM_FADE_OUT_MS.store(fade_out_duration_ms.min(5000) as u32, Ordering::Release);
}

// ── Overlay window ──────────────────────────────────────────────────
// A fullscreen WS_POPUP window with WS_EX_LAYERED (alpha-driven), plus
// WS_EX_TRANSPARENT + WS_EX_NOACTIVATE + HTTRANSPARENT so it never steals
// input — any physical mouse/keyboard input still updates the system idle
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

// Render the given dim level through the overlay: alpha = 255 × (1 − eased
// brightness). Level 1000 makes the overlay fully transparent. The window is
// kept alive at alpha 0 instead of hidden, preserving the topmost z-order and
// avoiding a taskbar-dimmer flash when the protector restores. The
// The default 80% overlay opacity is an ~80%-opaque black veil (≈20%
// visible brightness).
fn apply_dim_level(level: u32) {
    let raw = OVERLAY_HWND.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    let alpha = (255.0 * (1.0 - dim_factor(level)))
        .round()
        .clamp(0.0, 255.0) as u8;
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
        if alpha == 0 {
            // Do not hide the window. Removing a topmost window from the z-order
            // can trigger the taskbar dimmer's repair path and briefly flash its
            // overlay. An alpha-zero layered window is visually and functionally
            // inert while keeping the z-order stable.
        } else {
            let rect = virtual_screen_rect();
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS(
                    SWP_NOACTIVATE.0 | SWP_SHOWWINDOW.0,
                ),
            );
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }
}

// Windows shell flyouts (notification center, taskbar previews, ...) are
// topmost popups that can appear AFTER the dim starts. The overlay only
// re-asserts topmost while the dim level is changing, so a flyout opened
// during the hold phase would sit above the veil. While the dim is active a
// periodic repair re-inserts the overlay at the front of the topmost band,
// yielding to the app's own mode-change flyout (like the taskbar dimmer).
const ZORDER_REPAIR_INTERVAL_MS: u64 = 250;

fn overlay_needs_zorder_repair(hwnd: HWND) -> bool {
    let flyout = crate::taskbar_dimmer::active_flyout_hwnd();
    unsafe {
        let Ok(top) = GetTopWindow(None) else {
            return true;
        };
        if top == hwnd {
            // Already at the front of the band.
            return false;
        }
        // The mode-change flyout at the very top is correct only when the
        // overlay sits directly below it.
        if flyout != 0 && top == HWND(flyout as *mut c_void) {
            return !matches!(GetWindow(top, GW_HWNDNEXT), Ok(next) if next == hwnd);
        }
    }
    true
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
    let insert_after = match crate::taskbar_dimmer::active_flyout_hwnd() {
        0 => Some(HWND_TOPMOST),
        raw => Some(HWND(raw as *mut c_void)),
    };
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            insert_after,
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
// full brightness — a static bright cursor on the dimmed screen is exactly the
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

fn dim_watcher_loop() {
    let mut last_zorder_repair = std::time::Instant::now();
    while !DIM_SHUTDOWN.load(Ordering::Acquire) {
        // While the overlay is visible, periodically re-assert the front of
        // the topmost band so Windows flyouts (notification center, taskbar
        // previews) that opened after the dim started stay covered.
        if DIM_LEVEL.load(Ordering::Acquire) < 1000
            && last_zorder_repair.elapsed()
                >= std::time::Duration::from_millis(ZORDER_REPAIR_INTERVAL_MS)
        {
            repair_overlay_zorder();
            last_zorder_repair = std::time::Instant::now();
        }
        let enabled = DIM_ENABLED.load(Ordering::Acquire);
        let timeout = DIM_TIMEOUT_SECONDS.load(Ordering::Acquire);
        let idle = idle_seconds();
        // Fully bright until idle reaches the timeout, then dim to the floor.
        // Input resets idle, so the next tick starts fading back up.
        let target = if enabled && idle >= timeout {
            DIM_TARGET_LEVEL.load(Ordering::Acquire)
        } else {
            1000
        };
        let current = DIM_LEVEL.load(Ordering::Acquire);
        if target == 1000 && current < 1000 {
            // Restore the cursor as soon as input resets the idle timer. Do not
            // wait for the overlay fade to finish.
            sync_cursor(1000);
        }
        if current != target {
            // Use the configured transition duration while preserving the
            // smooth eased brightness curve.
            let duration = if current < target {
                DIM_FADE_OUT_MS.load(Ordering::Acquire)
            } else {
                DIM_FADE_IN_MS.load(Ordering::Acquire)
            };
            let step = transition_step(current, target, duration);
            let next = if current < target {
                current.saturating_add(step).min(target)
            } else {
                current.saturating_sub(step).max(target)
            };
            DIM_LEVEL.store(next, Ordering::Release);
            apply_dim_level(next);
        }
        // During restoration the cursor must remain visible even while the
        // overlay is still fading. During dimming/holding, keep it hidden.
        if target == 1000 {
            sync_cursor(1000);
        } else {
            sync_cursor(DIM_LEVEL.load(Ordering::Acquire));
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
    screen_protector_seconds: u32,
    screen_protector_opacity: f64,
    screen_protector_fade_in_duration_ms: u64,
    screen_protector_fade_out_duration_ms: u64,
) {
    configure_screen_protector(
        screen_protector_enabled,
        screen_protector_seconds,
        screen_protector_opacity,
        screen_protector_fade_in_duration_ms,
        screen_protector_fade_out_duration_ms,
    );
    // The overlay is created eagerly (hidden) so the debug preview works even
    // while the protector is disabled. Must run on the main thread (setup),
    // which it does: the app's message loop pumps its WM_PAINT.
    let _ = create_overlay();
    if screen_protector_enabled {
        ensure_watcher();
    }
}

pub fn set_screen_protector(
    enabled: bool,
    seconds: u32,
    opacity: f64,
    fade_in_duration_ms: u64,
    fade_out_duration_ms: u64,
) {
    configure_screen_protector(enabled, seconds, opacity, fade_in_duration_ms, fade_out_duration_ms);
    // The watcher is cheap (one idle poll per tick) and handles the disabled
    // case by fading back up, so just make sure it is running.
    ensure_watcher();
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
    // The overlay window is intentionally not destroyed here: DestroyWindow
    // must run on the creating (main) thread, and the process is exiting
    // anyway — Windows cleans it up. Restore the cursor before the counters
    // are torn down, in case we are exiting while dimmed.
    DIM_LEVEL.store(1000, Ordering::Release);
    ensure_cursor_visible();
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
        screensaver_enabled: read_screensaver_active().unwrap_or(false) && !screensaver_path.is_empty(),
        screensaver_timeout_minutes: read_screensaver_timeout_seconds().unwrap_or(0) / 60,
        screensaver_path,
        screensavers: list_screensavers(),
        screen_protector_enabled: DIM_ENABLED.load(Ordering::Acquire),
        screen_protector_seconds: DIM_TIMEOUT_SECONDS.load(Ordering::Acquire),
        screen_protector_opacity: DIM_OPACITY.load(Ordering::Acquire) as f64 / 1000.0,
        screen_protector_fade_in_duration_ms: DIM_FADE_IN_MS.load(Ordering::Acquire) as u64,
        screen_protector_fade_out_duration_ms: DIM_FADE_OUT_MS.load(Ordering::Acquire) as u64,
    })
}

#[tauri::command]
pub fn power_set_display_timeout(minutes: u32) -> Result<(), String> {
    powercfg_change("monitor-timeout", minutes)
}

#[tauri::command]
pub fn power_set_sleep_timeout(minutes: u32) -> Result<(), String> {
    powercfg_change("standby-timeout", minutes)
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
    enabled: bool,
    seconds: u32,
    opacity: f64,
    fade_in_duration_ms: u64,
    fade_out_duration_ms: u64,
) -> Result<(), String> {
    set_screen_protector(enabled, seconds, opacity, fade_in_duration_ms, fade_out_duration_ms);
    Ok(())
}

// Debug helper: preview the screen protector without waiting out the real idle
// timeout. Mirrors the watcher exactly — fade to black over ~2 s, HOLD dimmed
// until any input arrives (the click itself gets a short grace), then fade
// back up. Runs on a background thread so the command returns instantly.
#[tauri::command]
pub fn power_dim_preview() -> Result<(), String> {
    std::thread::Builder::new()
        .name("dim-preview".to_string())
        .spawn(move || {
            // The click itself is input; give the mouse-up a short grace so it
            // does not instantly cancel the preview.
            let start_input = last_input_ms();
            let target_level = DIM_TARGET_LEVEL.load(Ordering::Acquire);
            let fade_in_steps = transition_steps(DIM_FADE_IN_MS.load(Ordering::Acquire));
            // Fade down using the current Screen Dimmer opacity and duration.
            for step in 0..=fade_in_steps {
                if last_input_ms().wrapping_sub(start_input) > 150 {
                    apply_dim_level(1000);
                    sync_cursor(1000);
                    log::info!("[power] screen protector preview cancelled by input");
                    return;
                }
                let t = 1.0 - (step as f32 / fade_in_steps as f32);
                let level =
                    (t * (1000 - target_level) as f32 + target_level as f32).round() as u32;
                apply_dim_level(level);
                sync_cursor(level);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            log::info!("[power] screen protector preview dimmed; waiting for input");
            // Hold dimmed until any input arrives, then restore at the same
            // ~1 s pace as the real protector. Keep the overlay above any
            // flyout that opens while the preview holds.
            while last_input_ms().wrapping_sub(start_input) <= 150 {
                repair_overlay_zorder();
                sync_cursor(DIM_TARGET_LEVEL.load(Ordering::Acquire));
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            // Restore the cursor immediately when input is detected, before
            // beginning the overlay's brightness fade.
            sync_cursor(1000);
            let restore_steps = transition_steps(DIM_FADE_OUT_MS.load(Ordering::Acquire));
            for step in 0..=restore_steps {
                let t = step as f32 / restore_steps as f32;
                let level =
                    (t * (1000 - target_level) as f32 + target_level as f32).round() as u32;
                apply_dim_level(level);
                // Keep the cursor visible throughout the overlay fade.
                sync_cursor(1000);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            log::info!("[power] screen protector preview restored");
        })
        .map_err(|e| format!("could not spawn dim preview thread: {e}"))?;
    Ok(())
}
