// tray_balloon.rs, classic Windows tray balloon tip (Shell_NotifyIcon
// NIF_INFO), used for the one-time "still running in the tray" notice on the
// first close-to-tray.
//
// Why not tauri-plugin-notification? Its Windows implementation delivers
// WinRT toasts through a registered AppUserModelID, which only installed apps
// have; this app ships as a portable exe, so those toasts silently fail for
// most users. The classic balloon needs no AUMID, no installation and no
// permission, it is the notification that actually displays for a portable
// tray app.
//
// The balloon is attached to the app's REAL tray icon (the one Tauri/tray-icon
// registered), never a second one: the icon is located by enumerating this
// process's windows and probing ids with Shell_NotifyIconGetRect, then
// re-issued with NIM_MODIFY + NIF_INFO. Showing the balloon on a separate
// transient icon would make a duplicate icon appear in the tray.

use std::thread;
use std::time::Duration;

use windows::core::{BOOL, GUID};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIIF_LARGE_ICON, NIIF_USER,
    NIM_MODIFY, NOTIFYICONDATAW, NOTIFYICONDATAW_0, NOTIFYICONIDENTIFIER,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, EnumWindows, GetWindowThreadProcessId, HICON,
};

// Keep the balloon's icon handle alive long enough for the shell to render it,
// then release it.
const BALLOON_ICON_HOLD_MS: u64 = 12_000;

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Shows a balloon attached to the app's existing tray icon. Best-effort: a
/// failure must never disturb the close-to-tray flow, so errors are logged
/// (visible in the Debug tab) rather than propagated.
pub fn show(title: &str, message: &str) {
    unsafe {
        if let Err(error) = show_on_real_icon(title, message) {
            log::warn!("[tray-balloon] could not show tray balloon: {error}");
        }
    }
}

struct TrayIconSearch {
    pid: u32,
    found: Option<(HWND, u32)>,
}

unsafe extern "system" fn enum_tray_icon_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam.0 as *mut TrayIconSearch);
    if search.found.is_some() {
        return BOOL(0);
    }
    let mut window_pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut window_pid));
    if window_pid != search.pid {
        return BOOL(1);
    }
    // tray-icon registered its icon under a hidden window with a small
    // counter-based id; probe ids until Shell_NotifyIconGetRect resolves one.
    // No other window in this process owns a tray icon, so the first match is
    // the app's own.
    for uid in 0..16 {
        let identifier = NOTIFYICONIDENTIFIER {
            cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: hwnd,
            uID: uid,
            guidItem: GUID::zeroed(),
        };
        if Shell_NotifyIconGetRect(&identifier).is_ok() {
            search.found = Some((hwnd, uid));
            return BOOL(0);
        }
    }
    BOOL(1)
}

unsafe fn find_real_tray_icon() -> Option<(HWND, u32)> {
    let mut search = TrayIconSearch {
        pid: std::process::id(),
        found: None,
    };
    let _ = EnumWindows(
        Some(enum_tray_icon_window),
        LPARAM((&mut search as *mut TrayIconSearch) as isize),
    );
    search.found
}

unsafe fn show_on_real_icon(title: &str, message: &str) -> Result<(), String> {
    let (hwnd, uid) =
        find_real_tray_icon().ok_or_else(|| "could not locate the app's tray icon".to_string())?;

    // App icon in the balloon body is a nice-to-have; a missing icon must not
    // suppress the notification, so fall back to the plain info style.
    //
    // The shell validates the balloon icon size against the flag it is paired
    // with (tested empirically): a small icon without NIIF_LARGE_ICON must be
    // SM_CXSMICON-sized (16px at 100% DPI), and anything larger is rejected
    // with ERROR_INCORRECT_SIZE (1462). With NIIF_LARGE_ICON, larger sources
    // are accepted, 32/64/256 all work. So use the large-icon balloon with
    // the biggest ICO frame (256×256): the shell downscales it to the
    // balloon's large-icon slot, which stays crisp at any display DPI. The
    // old small-icon balloon handed the shell a 16px source that it upscaled
    // into that slot, that is what looked blurry.
    let icon = crate::window_icon::materialize_icon_file()
        .and_then(|icon_path| crate::window_icon::load_icon(&icon_path, 256, 256));
    let dw_info_flags = if icon.is_some() {
        NIIF_USER | NIIF_LARGE_ICON
    } else {
        NIIF_INFO
    };
    let h_balloon_icon = icon.unwrap_or(HICON(std::ptr::null_mut()));

    let title_u16 = to_wide(title);
    let message_u16 = to_wide(message);
    let mut sz_info_title = [0u16; 64];
    let mut sz_info = [0u16; 256];
    let title_len = title_u16.len().min(63);
    sz_info_title[..title_len].copy_from_slice(&title_u16[..title_len]);
    let message_len = message_u16.len().min(255);
    sz_info[..message_len].copy_from_slice(&message_u16[..message_len]);

    // uTimeout is only a hint honored by pre-Windows-8 systems; modern
    // Windows manages the display duration itself.
    let mut timeout = NOTIFYICONDATAW_0::default();
    timeout.uTimeout = 8_000;

    let nid = NOTIFYICONDATAW {
        uFlags: NIF_INFO,
        hWnd: hwnd,
        uID: uid,
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        szInfo: sz_info,
        Anonymous: timeout,
        szInfoTitle: sz_info_title,
        dwInfoFlags: dw_info_flags,
        hBalloonIcon: h_balloon_icon,
        ..Default::default()
    };
    if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
        if let Some(icon) = icon {
            let _ = DestroyIcon(icon);
        }
        return Err("Shell_NotifyIconW(NIM_MODIFY) failed".into());
    }

    // The shell references the balloon icon while displaying; release it once
    // the balloon has had time to render and show. (HICON is not Send, so the
    // thread gets a plain integer and rebuilds the handle inside.)
    if let Some(icon) = icon {
        let icon_raw = icon.0 as isize;
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(BALLOON_ICON_HOLD_MS));
            unsafe {
                let _ = DestroyIcon(HICON(icon_raw as *mut core::ffi::c_void));
            }
        });
    }

    Ok(())
}
