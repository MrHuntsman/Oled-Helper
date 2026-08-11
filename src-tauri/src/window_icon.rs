// window_icon.rs ,  forces Windows to load crisp small/large icons via WM_SETICON,
// working around a Tauri bug where the runtime window icon (titlebar + taskbar while
// running) is blurry because Tauri sets HICON from raw RGBA data instead of letting
// Windows pick the correctly-sized frame out of the .ico the way Explorer does.
//
// The same native ICO loading path is also used for the tray icon. Tauri's public
// TrayIcon API accepts an RGBA Image rather than an HICON, so the DPI-sized native
// HICON is converted to RGBA only after Windows has selected the correct ICO frame.

use std::fs;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, HGDIOBJ, RGBQUAD,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetIconInfo, GetSystemMetrics, LoadImageW, SendMessageW, ICONINFO, ICON_BIG,
    ICON_SMALL, IMAGE_ICON, LR_LOADFROMFILE, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON,
    WM_SETICON,
};

pub const ICON_BYTES: &[u8] = include_bytes!("../icons/icon.ico");

static INSTALLED_ICONS: OnceLock<Mutex<(isize, isize)>> = OnceLock::new();

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Writes the embedded icon bytes to a stable temp path once, reusing it on
/// subsequent launches instead of rewriting every time.
pub(crate) fn materialize_icon_file() -> Option<PathBuf> {
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

pub(crate) unsafe fn load_icon(
    path: &Path,
    cx: i32,
    cy: i32,
) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    let wide = to_wide(path.to_str()?);
    let handle = LoadImageW(
        None,
        PCWSTR(wide.as_ptr()),
        IMAGE_ICON,
        cx,
        cy,
        LR_LOADFROMFILE,
    )
    .ok()?;
    Some(windows::Win32::UI::WindowsAndMessaging::HICON(handle.0))
}

/// Loads the system-DPI-sized small ICO frame through Win32 and converts that
/// already-selected native frame into the RGBA Image required by Tauri's tray API.
/// This avoids decoding the ICO through a single arbitrary frame and then scaling
/// it in the tray implementation.
#[cfg(target_os = "windows")]
pub fn crisp_tray_image() -> Option<tauri::image::Image<'static>> {
    let icon_path = materialize_icon_file()?;

    unsafe {
        let width = GetSystemMetrics(SM_CXSMICON).max(1) as u32;
        let height = GetSystemMetrics(SM_CYSMICON).max(1) as u32;
        let hicon = load_icon(&icon_path, width as i32, height as i32)?;

        let mut icon_info: ICONINFO = std::mem::zeroed();
        if GetIconInfo(hicon, &mut icon_info).is_err() {
            let _ = DestroyIcon(hicon);
            return None;
        }

        let hdc = GetDC(None);
        if hdc.0.is_null() {
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmColor.0));
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmMask.0));
            let _ = DestroyIcon(hicon);
            return None;
        }

        let mut bitmap: BITMAP = std::mem::zeroed();
        let bitmap_size = size_of::<BITMAP>() as i32;
        let got_bitmap = GetObjectW(
            HGDIOBJ(icon_info.hbmColor.0),
            bitmap_size,
            Some((&mut bitmap as *mut BITMAP).cast()),
        );

        let bitmap_width = bitmap.bmWidth.max(1) as u32;
        let bitmap_height = bitmap.bmHeight.max(1) as u32;
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: bitmap_width as i32,
                // Negative height requests top-down rows, matching RGBA order.
                biHeight: -(bitmap_height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD::default()],
        };

        let mut bgra = vec![0u8; bitmap_width as usize * bitmap_height as usize * 4];
        let copied = if got_bitmap > 0 {
            GetDIBits(
                hdc,
                icon_info.hbmColor,
                0,
                bitmap_height,
                Some(bgra.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };

        if copied <= 0 {
            let _ = ReleaseDC(None, hdc);
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmColor.0));
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmMask.0));
            let _ = DestroyIcon(hicon);
            return None;
        }

        let mut alpha_has_content = false;
        let mut rgba = vec![0u8; bgra.len()];
        for (src, dst) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
            alpha_has_content |= src[3] != 0;
        }

        // Legacy ICO frames can carry transparency in the 1-bit mask instead
        // of the color bitmap's alpha channel. Read that mask before releasing
        // the HDC and bitmap handles.
        if !alpha_has_content {
            let mask_width = bitmap_width;
            let mask_height = (bitmap_height / 2).max(1);
            let mask_row_bytes = mask_width.div_ceil(32) * 4;
            let mut mask_info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: mask_width as i32,
                    biHeight: -(mask_height as i32),
                    biPlanes: 1,
                    biBitCount: 1,
                    biCompression: 0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [RGBQUAD::default()],
            };
            let mut mask_bits = vec![0u8; mask_row_bytes as usize * mask_height as usize];
            let mask_copied = GetDIBits(
                hdc,
                icon_info.hbmMask,
                0,
                mask_height,
                Some(mask_bits.as_mut_ptr().cast()),
                &mut mask_info,
                DIB_RGB_COLORS,
            );
            if mask_copied > 0 {
                for y in 0..bitmap_height as usize {
                    let mask_y = y.min(mask_height as usize - 1);
                    let row = &mask_bits[mask_y * mask_row_bytes as usize..];
                    for x in 0..bitmap_width as usize {
                        let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
                        rgba[(y * bitmap_width as usize + x) * 4 + 3] =
                            if bit == 0 { 255 } else { 0 };
                    }
                }
            } else {
                for pixel in rgba.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
            }
        }

        let _ = ReleaseDC(None, hdc);
        let _ = DeleteObject(HGDIOBJ(icon_info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(icon_info.hbmMask.0));
        let _ = DestroyIcon(hicon);

        Some(tauri::image::Image::new_owned(
            rgba,
            bitmap_width,
            bitmap_height,
        ))
    }
}

#[cfg(not(target_os = "windows"))]
pub fn crisp_tray_image() -> Option<tauri::image::Image<'static>> {
    tauri::image::Image::from_bytes(ICON_BYTES).ok()
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

        let installed = INSTALLED_ICONS.get_or_init(|| Mutex::new((0, 0)));
        let mut installed = installed.lock().ok();
        if let Some(small) = load_icon(&icon_path, small_cx, small_cy) {
            if let Some(ref mut icons) = installed {
                if icons.0 != 0 {
                    let _ = DestroyIcon(windows::Win32::UI::WindowsAndMessaging::HICON(
                        icons.0 as *mut _,
                    ));
                }
            }
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(small.0 as isize)),
            );
            if let Some(ref mut icons) = installed {
                icons.0 = small.0 as isize;
            }
        }
        if let Some(big) = load_icon(&icon_path, big_cx, big_cy) {
            if let Some(ref mut icons) = installed {
                if icons.1 != 0 {
                    let _ = DestroyIcon(windows::Win32::UI::WindowsAndMessaging::HICON(
                        icons.1 as *mut _,
                    ));
                }
            }
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(big.0 as isize)),
            );
            if let Some(ref mut icons) = installed {
                icons.1 = big.0 as isize;
            }
        }

        // Force the non-client area (titlebar) to repaint immediately with the
        // new icon instead of waiting for the next natural redraw.
        let _ = windows::Win32::Graphics::Gdi::RedrawWindow(
            Some(hwnd),
            None,
            None,
            windows::Win32::Graphics::Gdi::RDW_FRAME
                | windows::Win32::Graphics::Gdi::RDW_INVALIDATE
                | windows::Win32::Graphics::Gdi::RDW_UPDATENOW,
        );
    }
}
