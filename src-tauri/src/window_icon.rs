// window_icon.rs — forces Windows to load crisp small/large icons via WM_SETICON,
// working around a Tauri bug where the runtime window icon (titlebar + taskbar while
// running) is blurry because Tauri sets HICON from raw RGBA data instead of letting
// Windows pick the correctly-sized frame out of the .ico the way Explorer does.
//
// Fix: embed icon.ico at compile time, write it to a temp file once, then load it
// with LoadImageW (the same code path Explorer uses — picks the best frame per size
// and handles both legacy BMP and modern PNG-encoded ico frames correctly) and post
// WM_SETICON so the running window's titlebar/taskbar icon is crisp, not just the
// pinned-shortcut icon.
//
// Sizes are queried from the system (SM_CXSMICON / SM_CXICON) rather than hardcoded,
// since these scale with the display's DPI setting — a hardcoded 16px icon gets
// stretched (and looks blurry again) on any display above 100% scaling.

use std::fs;
use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, WPARAM, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageW, LoadImageW, IMAGE_ICON, LR_LOADFROMFILE, WM_SETICON,
    ICON_SMALL, ICON_BIG, HICON, GetSystemMetrics,
    SM_CXSMICON, SM_CYSMICON, SM_CXICON, SM_CYICON,
};
use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW};
use windows::core::PCWSTR;

const ICON_BYTES: &[u8] = include_bytes!("../icons/icon.ico");

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Writes the embedded icon bytes to a stable temp path once, reusing it on
/// subsequent launches instead of rewriting every time.
fn materialize_icon_file() -> Option<PathBuf> {
    let mut path = std::env::temp_dir();
    path.push("oled_helper_app_icon.ico");

    let needs_write = match fs::metadata(&path) {
        Ok(meta) => meta.len() as usize != ICON_BYTES.len(),
        Err(_) => true,
    };

    if needs_write {
        fs::write(&path, ICON_BYTES).ok()?;
    }

    Some(path)
}

unsafe fn load_icon(path: &PathBuf, cx: i32, cy: i32) -> Option<HICON> {
    let wide = to_wide(path.to_str()?);
    let handle = LoadImageW(
        None,
        PCWSTR(wide.as_ptr()),
        IMAGE_ICON,
        cx,
        cy,
        LR_LOADFROMFILE,
    ).ok()?;
    Some(HICON(handle.0))
}

/// Applies crisp small (titlebar/taskbar) and large (alt-tab) icons to the given
/// HWND, bypassing Tauri's blurry default runtime icon. Sizes are read from the
/// system's current DPI-scaled icon metrics rather than hardcoded.
pub fn apply_crisp_window_icon(hwnd_raw: isize) {
    let Some(icon_path) = materialize_icon_file() else {
        log::warn!("apply_crisp_window_icon: failed to materialize icon.ico");
        return;
    };

    unsafe {
        let hwnd = HWND(hwnd_raw as *mut _);

        let small_cx = GetSystemMetrics(SM_CXSMICON);
        let small_cy = GetSystemMetrics(SM_CYSMICON);
        let big_cx = GetSystemMetrics(SM_CXICON);
        let big_cy = GetSystemMetrics(SM_CYICON);

        if let Some(small) = load_icon(&icon_path, small_cx, small_cy) {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(small.0 as isize)),
            );
        }
        if let Some(big) = load_icon(&icon_path, big_cx, big_cy) {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(big.0 as isize)),
            );
        }

        // Force the non-client area (titlebar) to repaint immediately with the
        // new icon instead of waiting for the next natural redraw.
        let _ = RedrawWindow(
            Some(hwnd),
            None,
            None,
            RDW_FRAME | RDW_INVALIDATE | RDW_UPDATENOW,
        );
    }
}
