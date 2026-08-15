// text_rendering.rs — configure Windows font smoothing / ClearType subpixel
// rendering (the same settings BetterClearTypeTuner and the built-in ClearType
// tuner manipulate). Writes only HKCU values (no admin) and applies them via
// the documented SystemParametersInfo actions for the GDI settings, plus
// direct registry writes + a WM_SETTINGCHANGE broadcast for the
// DirectWrite/WPF (Avalon.Graphics) settings that have no SPI equivalent.
//
// Settings mapped here:
//   mode (off / grayscale / rgb / bgr)
//     GDI: SPI_SETFONTSMOOTHING + SPI_SETFONTSMOOTHINGTYPE + SPI_SETFONTSMOOTHINGORIENTATION
//     DW/WPF: HKCU\...\Avalon.Graphics\DISPLAY1 PixelStructure + ClearTypeLevel
//   gamma (1000-2200, default 1400) -> HKCU\Control Panel\Desktop\FontSmoothingGamma
//   contrast (1000-2200) -> GDI FontSmoothingContrast + DirectWrite GammaLevel
//     (both controlled by the same slider)
//
// SPI parameter convention (from the SystemParametersInfoW reference):
//   - SPI_SETFONTSMOOTHING takes the BOOL in uiParam.
//   - SPI_SETFONTSMOOTHINGTYPE / ORIENTATION / CONTRAST take the value
//     directly as pvParam ("The pvParam parameter is ..."), not a pointer to it.
// There is no SPI action for gamma, so it is written to the registry directly.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, LPARAM, WPARAM};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    REG_VALUE_TYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, SystemParametersInfoW, FE_FONTSMOOTHINGCLEARTYPE,
    FE_FONTSMOOTHINGORIENTATIONBGR, FE_FONTSMOOTHINGORIENTATIONRGB, FE_FONTSMOOTHINGSTANDARD,
    HWND_BROADCAST, SPI_SETFONTSMOOTHING, SPI_SETFONTSMOOTHINGCONTRAST,
    SPI_SETFONTSMOOTHINGORIENTATION, SPI_SETFONTSMOOTHINGTYPE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SPIF_UPDATEINIFILE, WM_FONTCHANGE,
    WM_SETTINGCHANGE,
};

const DESKTOP_SUBKEY: &str = r"Control Panel\Desktop";
const AVALON_SUBKEY: &str = r"Software\Microsoft\Avalon.Graphics\DISPLAY1";

const DEFAULT_GAMMA: u32 = 1400;
const DEFAULT_CONTRAST: u32 = 1400;

#[derive(serde::Serialize, Clone)]
pub struct TextRenderingState {
    pub smoothing_enabled: bool,
    pub smoothing_type: u32,
    pub orientation: u32,
    pub gamma: u32,
    pub contrast: u32,
    pub cleartype_level: u32,
    pub pixel_structure: u32,
    pub effective_mode: String,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn read_dword(subkey: &str, value: &str) -> Result<Option<u32>, String> {
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
        let mut data: u32 = 0;
        let mut size: u32 = 4;
        let mut value_type = REG_VALUE_TYPE::default();
        let query = RegQueryValueExW(
            key,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut value_type),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if query == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if query != ERROR_SUCCESS || value_type != REG_DWORD {
            return Err(format!("RegQueryValueExW({value}) failed: {query:?}"));
        }
        Ok(Some(data))
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
        if size_query != ERROR_SUCCESS || required == 0 || required > 64 {
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

fn write_dword(subkey: &str, value: &str, data: u32) -> Result<(), String> {
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
        let bytes = data.to_le_bytes();
        let set = RegSetValueExW(
            key,
            PCWSTR(value_name.as_ptr()),
            None,
            REG_DWORD,
            Some(&bytes),
        );
        let _ = RegCloseKey(key);
        if set != ERROR_SUCCESS {
            return Err(format!("RegSetValueExW({value}) failed: {set:?}"));
        }
        Ok(())
    }
}

// Tells the shell and text-rendering subsystems to re-read the changed font
// smoothing settings. SystemParametersInfo already broadcasts when given
// SPIF_SENDCHANGE; this extra broadcast covers the registry-only writes.
//
// Posted asynchronously like broadcast_font_change: a synchronous broadcast
// to every top-level window would block the UI thread on each contrast/gamma
// slider tick, which is the whole reason those controls feel laggy.
// Fire-and-forget is enough — apps pick the change up whenever they next run
// their message loop.
fn broadcast_settings_change() {
    unsafe {
        let msg = wide("Windows");
        let _ = PostMessageW(
            Some(HWND_BROADCAST),
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(msg.as_ptr() as isize),
        );
    }
}

// WM_FONTCHANGE tells renderers (Chromium included) that font rasterization
// settings changed, so they flush their glyph caches. Sent alongside the
// WM_SETTINGCHANGE broadcast on mode changes only — a full cache flush on
// every gamma/contrast slider tick would be wasteful.
//
// Posted asynchronously (PostMessage) instead of sent synchronously: a
// synchronous broadcast to every top-level window on the desktop blocks the
// UI thread until each one answers, which is what makes mode switching feel
// sluggish. The notification is fire-and-forget — other apps pick it up
// whenever they next run their message loop — so the command returns as soon
// as it is queued.
fn broadcast_font_change() {
    unsafe {
        let _ = PostMessageW(
            Some(HWND_BROADCAST),
            WM_FONTCHANGE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

// The mode-changing SPI calls persist with SPIF_UPDATEINIFILE but skip
// SPIF_SENDCHANGE: that flag makes SystemParametersInfo synchronously
// broadcast WM_SETTINGCHANGE to every top-level window, and a mode click
// would fire three of those broadcasts back-to-back, which is the main
// reason switching modes feels sluggish. The native preview repaints through
// its own PREVIEW_UPDATE message, other renderers pick up the change via the
// async WM_FONTCHANGE notification from apply_mode, and everything else in
// the app re-reads these values at startup anyway.
fn update_flags() -> SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS {
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(SPIF_UPDATEINIFILE.0)
}

fn set_smoothing_enabled(enabled: bool) -> Result<(), String> {
    unsafe {
        SystemParametersInfoW(
            SPI_SETFONTSMOOTHING,
            if enabled { 2 } else { 0 },
            None,
            update_flags(),
        )
        .map_err(|e| format!("SPI_SETFONTSMOOTHING failed: {e}"))
    }
}

fn set_smoothing_type(cleartype: bool) -> Result<(), String> {
    let value = if cleartype {
        FE_FONTSMOOTHINGCLEARTYPE
    } else {
        FE_FONTSMOOTHINGSTANDARD
    };
    unsafe {
        SystemParametersInfoW(
            SPI_SETFONTSMOOTHINGTYPE,
            0,
            Some(value as usize as *mut core::ffi::c_void),
            update_flags(),
        )
        .map_err(|e| format!("SPI_SETFONTSMOOTHINGTYPE failed: {e}"))
    }
}

fn set_orientation(rgb: bool) -> Result<(), String> {
    let value = if rgb {
        FE_FONTSMOOTHINGORIENTATIONRGB
    } else {
        FE_FONTSMOOTHINGORIENTATIONBGR
    };
    unsafe {
        SystemParametersInfoW(
            SPI_SETFONTSMOOTHINGORIENTATION,
            0,
            Some(value as usize as *mut core::ffi::c_void),
            update_flags(),
        )
        .map_err(|e| format!("SPI_SETFONTSMOOTHINGORIENTATION failed: {e}"))
    }
}

fn set_contrast(contrast: u32) -> Result<(), String> {
    unsafe {
        SystemParametersInfoW(
            SPI_SETFONTSMOOTHINGCONTRAST,
            0,
            Some(contrast as usize as *mut core::ffi::c_void),
            update_flags(),
        )
        .map_err(|e| format!("SPI_SETFONTSMOOTHINGCONTRAST failed: {e}"))?;
    }
    // DirectWrite uses this separate Avalon.Graphics value for its gamma.
    // Keep it on the same slider as the GDI contrast to give users one
    // consistent lightness control across both rendering paths.
    write_dword(AVALON_SUBKEY, "GammaLevel", contrast)?;
    broadcast_settings_change();
    log_smoothing_state(&format!("contrast set to {contrast}"));
    Ok(())
}

fn apply_directwrite(mode: &str) -> Result<(), String> {
    // PixelStructure: 0 = flat (grayscale), 1 = RGB, 2 = BGR.
    // ClearTypeLevel: 0 = grayscale edges, 50 = medium, 100 = full color.
    let (pixel_structure, cleartype_level) = match mode {
        "off" | "grayscale" => (0u32, 0u32),
        "rgb" => (1u32, 100u32),
        "bgr" => (2u32, 100u32),
        _ => return Err(format!("unknown mode: {mode}")),
    };
    write_dword(AVALON_SUBKEY, "PixelStructure", pixel_structure)?;
    write_dword(AVALON_SUBKEY, "ClearTypeLevel", cleartype_level)?;
    // No explicit broadcast here: apply_mode issues the async WM_FONTCHANGE
    // notify for the whole desktop, and every SPI call in the mode path skips
    // SPIF_SENDCHANGE so mode clicks never block on synchronous broadcasts.
    Ok(())
}

fn apply_mode(mode: &str) -> Result<(), String> {
    match mode {
        "off" => {
            set_smoothing_enabled(false)?;
        }
        "grayscale" => {
            set_smoothing_enabled(true)?;
            set_smoothing_type(false)?;
        }
        "rgb" => {
            set_smoothing_enabled(true)?;
            set_smoothing_type(true)?;
            set_orientation(true)?;
        }
        "bgr" => {
            set_smoothing_enabled(true)?;
            set_smoothing_type(true)?;
            set_orientation(false)?;
        }
        _ => return Err(format!("unknown mode: {mode}")),
    }
    apply_directwrite(mode)?;
    // Chromium (WebView2) caches ClearType rendering at the process level and
    // does not re-read it at runtime, so OLED Helper's own text needs a real
    // process restart to re-render (text_rendering_restart_app). The broadcast
    // below still lets GDI apps and other DirectWrite apps pick the change up
    // immediately.
    broadcast_font_change();
    log_smoothing_state(&format!("mode '{mode}' applied"));
    Ok(())
}

fn derive_effective_mode(smoothing_enabled: bool, smoothing_type: u32, orientation: u32) -> String {
    if !smoothing_enabled {
        return "off".to_string();
    }
    match smoothing_type {
        1 => "grayscale".to_string(),
        2 => {
            if orientation == 1 {
                "rgb".to_string()
            } else {
                "bgr".to_string()
            }
        }
        _ => "off".to_string(),
    }
}

// Read back the full GDI + DirectWrite smoothing state from the registry and
// log it. This is how we verify writes actually landed and let users compare
// what the app sets against what Explorer (a known ClearType holdout) visibly
// honors — the registry values update, but Explorer's text may not.
fn log_smoothing_state(context: &str) {
    let smoothing_enabled = read_string(DESKTOP_SUBKEY, "FontSmoothing")
        .ok()
        .flatten()
        .map(|v| v.trim() != "0")
        .unwrap_or(true);
    let smoothing_type = read_dword(DESKTOP_SUBKEY, "FontSmoothingType")
        .ok()
        .flatten()
        .unwrap_or(2);
    let orientation = read_dword(DESKTOP_SUBKEY, "FontSmoothingOrientation")
        .ok()
        .flatten()
        .unwrap_or(1);
    let gdi_gamma = read_dword(DESKTOP_SUBKEY, "FontSmoothingGamma")
        .ok()
        .flatten()
        .unwrap_or(DEFAULT_GAMMA);
    let pixel_structure = read_dword(AVALON_SUBKEY, "PixelStructure")
        .ok()
        .flatten()
        .unwrap_or(1);
    let cleartype_level = read_dword(AVALON_SUBKEY, "ClearTypeLevel")
        .ok()
        .flatten()
        .unwrap_or(100);
    let dw_gamma = read_dword(AVALON_SUBKEY, "GammaLevel")
        .ok()
        .flatten()
        .unwrap_or(1900);
    log::info!(
        "[text-rendering] {context}: GDI smoothing={smoothing_enabled} type={smoothing_type} orientation={orientation} gamma={gdi_gamma} | DirectWrite pixel_structure={pixel_structure} cleartype_level={cleartype_level} gamma_level={dw_gamma}"
    );
}

// A small native child window used by the Text tab as a live, real-Windows
// rendering sample. WebView2 caches its own text parameters, but a GDI child
// can be explicitly invalidated after each registry change and will repaint
// without restarting the app.
#[cfg(target_os = "windows")]
mod native_preview {
    use std::sync::{Mutex, OnceLock};
    use tauri::{AppHandle, Manager};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC,
        CreateFontIndirectW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
        FW_NORMAL, HDC, InvalidateRect, LOGFONTW,
        SelectObject, SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, UpdateWindow,
        COLORONCOLOR, SRCCOPY,
        DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, PAINTSTRUCT, TRANSPARENT,
    };    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetWindowPos, ShowWindow,
        WNDCLASSW, HWND_TOP, SW_HIDE, SWP_NOCOPYBITS, SWP_NOACTIVATE, SWP_SHOWWINDOW, WM_APP,
        WM_ERASEBKGND, GetClientRect, WM_NCHITTEST, WM_PAINT, WM_SIZE, WS_EX_NOACTIVATE,

        WS_EX_TOOLWINDOW, WS_POPUP,
    };

    const CLASS_NAME: &str = "OledHelperNativeTextPreview";
    const PREVIEW_UPDATE: u32 = WM_APP + 0x52;

    #[derive(Clone)]
    struct PreviewState {
        hwnd: isize,
        mode: String,
        gamma: u32,
        contrast: u32,
    }

    static STATE: OnceLock<Mutex<Option<PreviewState>>> = OnceLock::new();

    fn state() -> &'static Mutex<Option<PreviewState>> {
        STATE.get_or_init(|| Mutex::new(None))
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn class_name() -> Vec<u16> {
        wide(CLASS_NAME)
    }

    fn font_for_size(size: i32, dpi: u32) -> windows::Win32::Graphics::Gdi::HFONT {
        let mut logfont = LOGFONTW::default();
        logfont.lfHeight = -((size as i64 * dpi as i64 / 96).max(1) as i32);
        logfont.lfWeight = FW_NORMAL.0 as i32;
        let face: Vec<u16> = "Segoe UI".encode_utf16().collect();
        let count = face.len().min(logfont.lfFaceName.len());
        logfont.lfFaceName[..count].copy_from_slice(&face[..count]);
        unsafe { CreateFontIndirectW(&logfont) }
    }

    unsafe fn draw_raster_zoom_row(
        hdc: HDC,
        target: RECT,
        text: &[u16],
        size: i32,
        dpi: u32,
        zoom_factor: i32,
    ) {
        let source_width = ((target.right - target.left) / zoom_factor).max(1);
        let source_height = ((target.bottom - target.top) / zoom_factor).max(1);
        let source_dc = CreateCompatibleDC(Some(hdc));
        let source_bitmap = CreateCompatibleBitmap(hdc, source_width, source_height);
        let old_bitmap = SelectObject(source_dc, source_bitmap.into());
        let background = CreateSolidBrush(COLORREF(0x00181414));
        let source_rect = RECT {
            left: 0,
            top: 0,
            right: source_width,
            bottom: source_height,
        };
        let _ = FillRect(source_dc, &source_rect, background);
        let _ = DeleteObject(background.into());

        let font = font_for_size(size, dpi);
        let old_font = SelectObject(source_dc, font.into());
        let _ = SetBkMode(source_dc, TRANSPARENT);
        let _ = SetTextColor(source_dc, COLORREF(0x00ffffff));
        let mut source_text = text.to_vec();
        let _ = DrawTextW(
            source_dc,
            &mut source_text,
            &mut RECT {
                left: 0,
                top: 0,
                right: source_width,
                bottom: source_height,
            },
            DT_LEFT | DT_NOPREFIX | DT_SINGLELINE | DT_VCENTER,
        );
        let _ = SelectObject(source_dc, old_font);
        let _ = DeleteObject(font.into());

        let _ = SetStretchBltMode(hdc, COLORONCOLOR);
        let _ = StretchBlt(
            hdc,
            target.left,
            target.top,
            target.right - target.left,
            target.bottom - target.top,
            Some(source_dc),
            0,
            0,
            source_width,
            source_height,
            SRCCOPY,
        );
        let _ = SelectObject(source_dc, old_bitmap);
        let _ = DeleteObject(source_bitmap.into());
        let _ = windows::Win32::Graphics::Gdi::DeleteDC(source_dc);
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> LRESULT {
        match message {
            PREVIEW_UPDATE | WM_PAINT => {
                if message == PREVIEW_UPDATE {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else {
                    let mut paint = PAINTSTRUCT::default();
                    let hdc = BeginPaint(hwnd, &mut paint);
                    let mut client = RECT::default();
                    let _ = GetClientRect(hwnd, &mut client);

                    // Match the Text tab's panel background (rgba(0,0,0,0.12)
                    // over --panel) while keeping this HWND opaque, so the
                    // WebView underneath cannot bleed through.
                    let background = CreateSolidBrush(COLORREF(0x00181414));
                    let _ = FillRect(hdc, &client, background);
                    let _ = DeleteObject(background.into());

                    let preview_ready = state()
                        .lock()
                        .ok()
                        .map(|value| value.is_some())
                        .unwrap_or(false);
                    if preview_ready {
                        // Two comparison columns: actual size on the left and a
                        // 400% raster-zoomed version on the right. The left
                        // column has four sentence rows at increasing sizes
                        // (8, 10, 12, 15); the right column zooms the last two
                        // sizes (12, 15) 4x. The "400% zoom" caption lives in
                        // the app HTML above the preview.
                        let text: Vec<u16> =
                            "The quick brown fox jumps over the lazy dog"
                                .encode_utf16()
                                .collect();
                        let row_sizes = [8i32, 10, 12, 15];
                        let right_sizes = [12i32, 15];
                        let zoom_factor = 4i32;
                        let dpi = GetDpiForWindow(hwnd).max(96);
                        let top_padding = ((2u32 * dpi) / 96).max(1) as i32;
                        let horizontal_margin = ((10u32 * dpi) / 96).max(1) as i32;
                        let column_gap = ((12u32 * dpi) / 96).max(1) as i32;
                        let row_gap = ((6u32 * dpi) / 96).max(1) as i32;
                        let split_x = client.left + (client.right - client.left) / 2;
                        let content_top = client.top + top_padding;
                        let _ = SetBkMode(hdc, TRANSPARENT);
                        let _ = SetTextColor(hdc, COLORREF(0x00ffffff));

                        // Left column: lines stack at their own height plus a
                        // small gap, vertically centered in the space below the
                        // top padding so it balances against the taller
                        // right column.
                        let left_height: i32 = row_sizes
                            .iter()
                            .map(|&size| (((size as u32 * 3 / 2) * dpi) / 96).max(1) as i32)
                            .sum::<i32>()
                            + row_gap * (row_sizes.len() as i32 - 1);
                        let left_area = (client.bottom - client.top - top_padding).max(0);
                        let mut left_cursor = content_top + (left_area - left_height).max(0) / 2;
                        // Right column: zoomed rows, stacked with no gap.
                        let mut right_top = content_top;

                        unsafe fn draw_left_row(
                            hdc: HDC,
                            rect: &mut RECT,
                            text: &[u16],
                            size: i32,
                            dpi: u32,
                        ) {
                            let font = font_for_size(size, dpi);
                            let old_font = SelectObject(hdc, font.into());
                            let mut buf = text.to_vec();
                            let _ = DrawTextW(
                                hdc,
                                &mut buf,
                                rect,
                                DT_LEFT | DT_NOPREFIX | DT_SINGLELINE | DT_VCENTER,
                            );
                            let _ = SelectObject(hdc, old_font);
                            let _ = DeleteObject(font.into());
                        }

                        // Left column: sentence rows at increasing sizes.
                        for size in row_sizes {
                            // Taller line box (1.5x the em) so descenders
                            // like 'g'/'j'/'p' aren't clipped at the bottom.
                            let base_height = (((size as u32 * 3 / 2) * dpi) / 96).max(1) as i32;
                            let mut left_rect = RECT {
                                left: client.left + horizontal_margin,
                                top: left_cursor,
                                right: split_x - column_gap,
                                bottom: (left_cursor + base_height).min(client.bottom - 1),
                            };
                            draw_left_row(hdc, &mut left_rect, &text, size, dpi);
                            left_cursor += base_height + row_gap;
                        }

                        // Right column: the last two sentence sizes at 4x zoom.
                        for size in right_sizes {
                            let base_height = (((size + 2) as u32 * dpi) / 96).max(1) as i32;
                            let zoom_height = base_height * 4;
                            let right_rect = RECT {
                                left: split_x + column_gap,
                                top: right_top,
                                right: client.right - horizontal_margin,
                                bottom: (right_top + zoom_height).min(client.bottom - 1),
                            };
                            draw_raster_zoom_row(hdc, right_rect, &text, size, dpi, zoom_factor);
                            right_top += zoom_height;
                        }
                    }
                    let _ = EndPaint(hwnd, &paint);
                    return LRESULT(0);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            // Let mouse and keyboard input pass through to the WebView below.
            WM_NCHITTEST => LRESULT(-1), // HTTRANSPARENT
            _ => DefWindowProcW(hwnd, message, _wparam, _lparam),
        }
    }

    fn register_class() {
        let name = class_name();
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: PCWSTR(name.as_ptr()),
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }
    }

    fn create(parent: HWND) -> Result<HWND, String> {
        register_class();
        let class = class_name();
        let title = wide("OLED Helper native text preview");
        unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                1,
                1,
                Some(parent),
                None,
                None,
                None,
            )
            .map_err(|error| format!("could not create native text preview: {error}"))
        }
    }

    // These commands are synchronous Tauri commands and already run on the
    // Tauri UI thread. Do not call run_on_main_thread here: queueing work back
    // to the same thread and waiting for it would deadlock until the timeout,
    // leaving the preview window uncreated or unpositioned.
    pub fn show(app: &AppHandle, mode: String, gamma: u32, contrast: u32) -> Result<(), String> {
        let main = app
            .get_webview_window("main")
            .ok_or_else(|| "main window is unavailable".to_string())?;
        let parent = main
            .hwnd()
            .map_err(|error| format!("could not get main HWND: {error}"))?;
        let parent = HWND(parent.0);
        let hwnd = {
            let mut current = state()
                .lock()
                .map_err(|_| "native preview state is poisoned".to_string())?;
            if let Some(existing) = current.as_ref() {
                HWND(existing.hwnd as *mut std::ffi::c_void)
            } else {
                let created = create(parent)?;
                *current = Some(PreviewState {
                    hwnd: created.0 as isize,
                    mode: mode.clone(),
                    gamma,
                    contrast,
                });
                created
            }
        };
        // Stay hidden until set_bounds receives the final DOM rectangle;
        // otherwise the first frame could flash at the temporary 1×1
        // creation position in the top-left of the main window.
        update_state(hwnd, mode, gamma, contrast)
    }

    fn update_state(hwnd: HWND, mode: String, gamma: u32, contrast: u32) -> Result<(), String> {
        if let Ok(mut current) = state().lock() {
            if let Some(current) = current.as_mut() {
                current.mode = mode;
                current.gamma = gamma;
                current.contrast = contrast;
            }
        }
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(hwnd),
                PREVIEW_UPDATE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        Ok(())
    }

    pub fn update(_app: &AppHandle, mode: String, gamma: u32, contrast: u32) -> Result<(), String> {
        let hwnd = state()
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|value| HWND(value.hwnd as *mut std::ffi::c_void)));
        if let Some(hwnd) = hwnd {
            update_state(hwnd, mode, gamma, contrast)?;
        }
        Ok(())
    }

    pub fn set_bounds(app: &AppHandle, x: i32, y: i32, width: i32, height: i32) -> Result<(), String> {
        let hwnd = state()
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|value| HWND(value.hwnd as *mut std::ffi::c_void)));
        if let Some(hwnd) = hwnd {
            let main = app
                .get_webview_window("main")
                .ok_or_else(|| "main window is unavailable".to_string())?;
            let parent = main
                .hwnd()
                .map_err(|error| format!("could not get main HWND: {error}"))?;
            let parent = HWND(parent.0);
            let mut screen_point = POINT { x, y };
            unsafe {
                if !ClientToScreen(parent, &mut screen_point).as_bool() {
                    return Err("could not convert preview coordinates to screen space".to_string());
                }
                SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    screen_point.x,
                    screen_point.y,
                    width.max(1),
                    height.max(1),
                    SWP_NOCOPYBITS | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )
                .map_err(|error| format!("could not position native text preview: {error}"))?;
                // Resizing a popup can leave copied client bits on screen until
                // the normal message loop gets around to WM_PAINT. Invalidate
                // and flush the repaint now so the raster zoom is regenerated
                // from the new bounds instead of showing stale or stretched
                // contents during a main-window resize.
                let _ = InvalidateRect(Some(hwnd), None, false);
                let _ = UpdateWindow(hwnd);
            }
        }
        Ok(())
    }

    pub fn hide(_app: &AppHandle) -> Result<(), String> {
        if let Some(hwnd) = state()
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|value| HWND(value.hwnd as *mut std::ffi::c_void)))
        {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
        Ok(())
    }

    // Destroy the preview window entirely (not just hide it). Used when the
    // main window minimizes: the preview is an owned popup, and Windows
    // automatically re-shows owned windows that were visible when their owner
    // is restored — mid restore-animation, bypassing set_bounds. With the
    // HWND gone there is nothing for Windows to re-show, so the text sample
    // can never appear over the transition. The frontend recreates it (via
    // text_preview_show) once the window is back and settled.
    pub fn destroy(_app: &AppHandle) -> Result<(), String> {
        // Capture the HWND before clearing the state, then destroy it.
        let hwnd = state()
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|value| HWND(value.hwnd as *mut std::ffi::c_void)));
        // Clear the state so a later show() creates a fresh window and any
        // in-flight set_bounds/update calls become no-ops.
        if let Ok(mut current) = state().lock() {
            *current = None;
        }
        if let Some(hwnd) = hwnd {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        Ok(())
    }
}

#[tauri::command]
pub fn text_preview_show(
    app: tauri::AppHandle,
    mode: String,
    gamma: u32,
    contrast: u32,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        return native_preview::show(&app, mode, gamma, contrast);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, mode, gamma, contrast);
        Err("native text preview is only available on Windows".to_string())
    }
}

#[tauri::command]
pub fn text_preview_update(
    app: tauri::AppHandle,
    mode: String,
    gamma: u32,
    contrast: u32,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        return native_preview::update(&app, mode, gamma, contrast);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, mode, gamma, contrast);
        Ok(())
    }
}

#[tauri::command]
pub fn text_preview_set_bounds(
    app: tauri::AppHandle,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        return native_preview::set_bounds(&app, x, y, width, height);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, x, y, width, height);
        Ok(())
    }
}

#[tauri::command]
pub fn text_preview_hide(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        return native_preview::hide(&app);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        Ok(())
    }
}

// Backend-facing destroy entry point, used by the main-window subclass when
// the window minimizes (see display_change_events in lib.rs) as well as the
// frontend. Idempotent: destroying an already-destroyed (or never-created)
// preview is a no-op.
pub(crate) fn destroy_text_preview(app: &tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        native_preview::destroy(app)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        Ok(())
    }
}

#[tauri::command]
pub fn text_preview_destroy(app: tauri::AppHandle) -> Result<(), String> {
    destroy_text_preview(&app)
}

// ── Restore-animation settle detection ──────────────────────────────────
// Windows exposes no API signal for when the DWM minimize→restore animation
// ends: the window rect is at its final position immediately (the animation
// is a DWM-side transform), the window is uncloaked at animation start, and
// no message storm marks the end. The only exact signal is the visual one —
// the window's pixels change every frame while the animation plays and stop
// when it completes. So after a genuine restore-from-minimized, capture the
// window's region at ~30 fps, hash it, and emit `window-restore-settled` the
// moment it stops changing (or after a hard timeout as a safety net). The
// Text tab uses that event to show its native preview exactly when the
// animation ends instead of waiting a fixed number of milliseconds.
mod restore_settle {
    use std::sync::atomic::{AtomicU64, Ordering};
    use tauri::{AppHandle, Emitter};

    static GENERATION: AtomicU64 = AtomicU64::new(0);

    const POLL_MS: u64 = 25;
    // ~1.2 s of polling: a restore animation lasts a few hundred ms at most,
    // so this is a generous cap before the fallback emit fires.
    const MAX_FRAMES: u32 = 48;
    // Two consecutive identical captures (~50 ms of stillness) means the
    // animation has finished.
    const STABLE_FRAMES: u32 = 2;

    // Downscaled capture of the window's screen region. Returns a cheap hash
    // of the pixels; None if the capture failed (window destroyed etc.).
    fn capture_hash(hwnd: isize) -> Option<u64> {
        unsafe {
            use windows::Win32::Foundation::{HWND, RECT};
            use windows::Win32::Graphics::Gdi::{
                BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
                GetDIBits, ReleaseDC, SelectObject, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
                DIB_RGB_COLORS, SRCCOPY,
            };
            use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

            let hwnd = HWND(hwnd as *mut std::ffi::c_void);
            let mut rect = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rect);
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            if w <= 0 || h <= 0 {
                return None;
            }
            const CAP_W: i32 = 48;
            const CAP_H: i32 = 36;
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let mem = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, CAP_W, CAP_H);
            let old = SelectObject(mem, bmp.into());
            let _ = BitBlt(
                mem,
                0,
                0,
                CAP_W,
                CAP_H,
                Some(screen),
                rect.left,
                rect.top,
                SRCCOPY,
            );
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: CAP_W,
                    biHeight: -CAP_H,
                    biPlanes: 1,
                    biBitCount: 24,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut buf = vec![0u8; (CAP_W * CAP_H * 4) as usize];
            let got = GetDIBits(
                mem,
                bmp,
                0,
                CAP_H as u32,
                Some(buf.as_mut_ptr() as *mut _),
                &mut info,
                DIB_RGB_COLORS,
            );
            let _ = SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(None, screen);
            if got == 0 {
                return None;
            }
            let mut hash: u64 = 1469598103934665603;
            let mut i = 0;
            while i < buf.len() {
                hash = hash.wrapping_mul(1099511628211).wrapping_add(buf[i] as u64);
                i += 4;
            }
            Some(hash)
        }
    }

    fn emit_settled(app: &AppHandle) {
        let _ = app.emit("window-restore-settled", ());
    }

    // Called from the main-window subclass on a genuine minimize→restore.
    // Spawns a short-lived watcher that emits `window-restore-settled` once
    // the window's visual stops changing (or after the timeout). A generation
    // counter makes a superseded watcher (a newer restore started) exit
    // silently instead of emitting a stale event.
    pub fn start(app: &AppHandle, hwnd: isize) {
        let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        let app = app.clone();
        std::thread::spawn(move || {
            let mut previous: Option<u64> = None;
            let mut stable = 0u32;
            for _ in 0..MAX_FRAMES {
                if GENERATION.load(Ordering::SeqCst) != generation {
                    return; // superseded by a newer restore
                }
                match capture_hash(hwnd) {
                    Some(hash) => {
                        if previous == Some(hash) {
                            stable += 1;
                        } else {
                            stable = 0;
                        }
                        previous = Some(hash);
                        if stable >= STABLE_FRAMES {
                            emit_settled(&app);
                            return;
                        }
                    }
                    None => {
                        // Capture failed; keep polling — the timeout below is
                        // the safety net.
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
            }
            // Timeout: never settled (e.g. something else was animating over
            // the window). Emit anyway so the preview is never stuck hidden.
            emit_settled(&app);
        });
    }
}

pub(crate) fn start_restore_settle_watch(app: &tauri::AppHandle, hwnd: isize) {
    #[cfg(target_os = "windows")]
    {
        restore_settle::start(app, hwnd);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, hwnd);
    }
}

#[tauri::command]
pub fn text_rendering_get_state() -> Result<TextRenderingState, String> {
    let smoothing_enabled = match read_string(DESKTOP_SUBKEY, "FontSmoothing") {
        Ok(Some(value)) => value.trim() != "0",
        Ok(None) => true,
        Err(e) => {
            log::warn!("[text-rendering] could not read FontSmoothing: {e}");
            true
        }
    };
    let smoothing_type = read_dword(DESKTOP_SUBKEY, "FontSmoothingType")
        .unwrap_or(None)
        .unwrap_or(2);
    let orientation = read_dword(DESKTOP_SUBKEY, "FontSmoothingOrientation")
        .unwrap_or(None)
        .unwrap_or(1);
    let gamma = read_dword(DESKTOP_SUBKEY, "FontSmoothingGamma")
        .unwrap_or(None)
        .unwrap_or(DEFAULT_GAMMA);
    let contrast = read_dword(DESKTOP_SUBKEY, "FontSmoothingContrast")
        .unwrap_or(None)
        .unwrap_or(DEFAULT_CONTRAST);
    let cleartype_level = read_dword(AVALON_SUBKEY, "ClearTypeLevel")
        .unwrap_or(None)
        .unwrap_or(100);
    let pixel_structure = read_dword(AVALON_SUBKEY, "PixelStructure")
        .unwrap_or(None)
        .unwrap_or(1);

    Ok(TextRenderingState {
        smoothing_enabled,
        smoothing_type,
        orientation,
        gamma,
        contrast,
        cleartype_level,
        pixel_structure,
        effective_mode: derive_effective_mode(smoothing_enabled, smoothing_type, orientation),
    })
}

#[tauri::command]
pub fn text_rendering_set_mode(mode: String) -> Result<(), String> {
    apply_mode(&mode)
}

#[tauri::command]
pub fn text_rendering_set_gamma(gamma: u32) -> Result<(), String> {
    if !(1000..=2200).contains(&gamma) {
        return Err(format!("gamma out of range: {gamma}"));
    }
    write_dword(DESKTOP_SUBKEY, "FontSmoothingGamma", gamma)?;
    broadcast_settings_change();
    log_smoothing_state(&format!("gamma set to {gamma}"));
    Ok(())
}

#[tauri::command]
pub fn text_rendering_set_contrast(contrast: u32) -> Result<(), String> {
    if !(1000..=2200).contains(&contrast) {
        return Err(format!("contrast out of range: {contrast}"));
    }
    set_contrast(contrast)
}

// DirectWrite/WPF-only: controls the color intensity of subpixel antialiasing
// (0 = grayscale edges, 100 = full color). GDI ignores it, so the native
// preview does not change; DirectWrite apps read it at startup.
#[tauri::command]
pub fn text_rendering_set_cleartype_level(level: u32) -> Result<(), String> {
    if level > 100 {
        return Err(format!("cleartype level out of range: {level}"));
    }
    write_dword(AVALON_SUBKEY, "ClearTypeLevel", level)?;
    log_smoothing_state(&format!("cleartype level set to {level}"));
    Ok(())
}

#[tauri::command]
pub fn text_rendering_reset() -> Result<(), String> {
    apply_mode("rgb")?;
    write_dword(DESKTOP_SUBKEY, "FontSmoothingGamma", DEFAULT_GAMMA)?;
    set_contrast(DEFAULT_CONTRAST)?;
    Ok(())
}

// Chromium (WebView2) caches ClearType rendering at the process level and
// cannot be made to re-read it live, so a real process restart is the only
// way to make OLED Helper's own text re-render. Reuses the tray-restart path
// and forwards --open-tab so the user lands back on the tab they were using.
#[tauri::command]
pub fn text_rendering_restart_app(app: tauri::AppHandle, tab: Option<String>) -> Result<(), String> {
    crate::request_restart(&app, tab.as_deref());
    Ok(())
}
