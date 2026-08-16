//! explorer-hook.dll — inject a GDI text-rendering replacement into the shell
//! and the classic Control Panel hosts via a global WH_CBT hook.
//!
//! Instead of rewriting the font-quality flag at CreateFontIndirectW (which
//! only affected fonts created after a window was built), we hook the text
//! OUTPUT path (ExtTextOutW) and rasterize each draw with `swash` — a pure-Rust
//! glyph rasterizer — so a mode change is picked up on the very next repaint
//! (live). Mode 0 forwards straight to GDI (RGB/BGR keep Windows ClearType);
//! modes 1/2 resolve the selected font, rasterize the run with swash, and
//! alpha-blend the coverage into the HDC using the current text color.
//!
//! Crash safety (explorer.exe restarts itself if we crash it):
//! 1. The inline patch is applied while every OTHER thread of the process is
//!    suspended, so no thread can execute the function entry while the jump
//!    bytes are half-written (ExtTextOutW runs on many threads at once).
//! 2. The detour is installed lazily from the first hook callback, NOT in
//!    DllMain, keeping the loader lock free of patching work.
//! 3. The hook never returns failure spuriously: before the detour is stored
//!    it calls the original pointer directly, and any rasterization error
//!    falls back to GDI untouched.

mod detour;
mod font;

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use detour::Detour;
use swash::scale::image::Content;
use windows::core::{s, w, BOOL, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LPARAM, LRESULT, WPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject,
    GetCurrentObject, GetObjectW, GetTextAlign, GetTextColor, HDC, LOGFONTW, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    DIB_RGB_COLORS, ETO_OPTIONS, ETO_GLYPH_INDEX, ETO_PDY, OBJ_FONT, SRCCOPY, TA_BASELINE,
    TA_BOTTOM, TA_CENTER, TA_RIGHT,
};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleW, GetProcAddress, LoadLibraryW,
};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE,
};
use windows::Win32::System::SystemServices::{
    DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, DLL_THREAD_ATTACH, DLL_THREAD_DETACH,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
};
use windows::Win32::UI::WindowsAndMessaging::CallNextHookEx;

// Shared memory layout (24 bytes): the app writes the mode at offset 0 and
// the three render knobs (filter strength, crosstalk, boldness) at 12/16/20;
// the DLL writes its own state back at offsets 4 and 8 so the app can verify
// the hook is really installed in the CURRENT explorer.exe instead of
// trusting that the module merely loaded.
const STATE_MODE: usize = 0;
const STATE_STATUS: usize = 4;
const STATE_PID: usize = 8;
const STATE_LCD_STRENGTH: usize = 12;
const STATE_CROSSTALK: usize = 16;
const STATE_BOLDNESS: usize = 20;
const STATE_SIZE: usize = 24;
// Status bit written by the DLL once the ExtTextOutW detour is installed.
const STATUS_HOOK_INSTALLED: u32 = 1;

#[allow(improper_ctypes_definitions)]
type ExtTextOutWFn = unsafe extern "system" fn(
    HDC,
    i32,
    i32,
    ETO_OPTIONS,
    *const RECT,
    PCWSTR,
    u32,
    *const i32,
) -> BOOL;

// Shared mode (0 = passthrough, 1 = grayscale, 2 = crisp) + the installed
// detour. Both are set once and live for the process lifetime. STATE stores
// the mapped view's raw pointer as a usize (raw pointers are not Send/Sync,
// so we carry the address in an integer type instead). ORIGINAL keeps the
// pristine entry point so the hook can fall back to a direct call before the
// detour is stored.
static STATE: OnceLock<usize> = OnceLock::new();
static DETOUR: OnceLock<Detour> = OnceLock::new();
static ORIGINAL: OnceLock<ExtTextOutWFn> = OnceLock::new();
// True once the deferred detour install has been attempted; reset on failure
// so the next hook callback retries (the callback fires on every window
// creation, so a transient failure self-heals).
static INSTALL_ATTEMPTED: AtomicBool = AtomicBool::new(false);
// Cached in DllMain so the hook callback doesn't re-query the process image
// path on every window-creation event.
static IS_TARGET: AtomicBool = AtomicBool::new(false);

// ── File logging (%TEMP%\OledHelperHook.log) ───────────────────────────
// The hook runs inside explorer.exe and friends, where OutputDebugStringW is
// invisible to the app. An append-only file survives a process crash (the OS
// flushes the handle on exit), so the last line before a crash is the clue.
static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static LOG_START: OnceLock<Instant> = OnceLock::new();
static PROC_NAME: OnceLock<String> = OnceLock::new();
static RENDER_COUNT: AtomicU32 = AtomicU32::new(0);

fn proc_name() -> &'static str {
    PROC_NAME.get_or_init(process_base_name).as_str()
}

fn log(msg: &str) {
    let cell = LOG_FILE.get_or_init(|| {
        let path = std::env::temp_dir().join("OledHelperHook.log");
        let file = OpenOptions::new().create(true).append(true).open(path).ok();
        Mutex::new(file)
    });
    let ts = LOG_START.get_or_init(Instant::now).elapsed().as_millis();
    if let Ok(mut guard) = cell.lock() {
        if let Some(f) = guard.as_mut() {
            let _ = writeln!(
                f,
                "[{ts:>8}ms][{} pid={}] {msg}",
                proc_name(),
                std::process::id()
            );
            let _ = f.flush();
        }
    }
}

fn state_mode() -> u32 {
    // SAFETY: STATE is written once in map_state; the mapped view stays valid
    // for the lifetime of the process.
    unsafe {
        STATE.get()
            .map(|addr| *((addr + STATE_MODE) as *const u32))
            .unwrap_or(0)
    }
}

// Read a u32 knob from shared memory with a default, clamped to 0..=100.
fn state_knob(offset: usize, default: u32) -> u8 {
    // SAFETY: STATE is written once in map_state; the mapped view stays valid
    // for the lifetime of the process.
    let v = unsafe {
        STATE.get()
            .map(|addr| *((addr + offset) as *const u32))
            .unwrap_or(default)
    };
    v.min(100) as u8
}

// Per-draw render options for the current draw. The LCD filter is applied at
// its strength, where 0 = off; crosstalk and boldness pass straight through.
// Defaults are all off/no-op when the app has not written the knobs yet.
fn state_render_options() -> text_raster::RenderOptions {
    text_raster::RenderOptions {
        lcd_strength: state_knob(STATE_LCD_STRENGTH, 0),
        crosstalk: state_knob(STATE_CROSSTALK, 0),
        boldness: state_knob(STATE_BOLDNESS, 0),
    }
}

fn report_state(status: u32) {
    let Some(addr) = STATE.get() else {
        return;
    };
    // SAFETY: the mapped view is valid for the process lifetime; both fields
    // are plain u32s at fixed offsets we own.
    unsafe {
        let pid = std::process::id();
        std::ptr::write_volatile((addr + STATE_PID) as *mut u32, pid);
        std::ptr::write_volatile((addr + STATE_STATUS) as *mut u32, status);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn debug(msg: &str) {
    let w = wide(msg);
    unsafe { OutputDebugStringW(PCWSTR(w.as_ptr())) };
    log(msg);
}

// The DLL is loaded into more than just explorer.exe (the app LoadLibraryWs it
// to resolve the hook proc, and the global hook maps it into every GUI
// process). Only the targeted shell/Control Panel hosts need the detour.
fn process_name() -> Option<String> {
    unsafe {
        let mut buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut buf);
        if len == 0 || len as usize >= buf.len() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn process_base_name() -> String {
    let Some(name) = process_name() else {
        return String::new();
    };
    Path::new(&name)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_explorer() -> bool {
    process_base_name().eq_ignore_ascii_case("explorer.exe")
}

// Everything the app wants the detour in: Explorer plus the classic Control
// Panel host processes and the MMC admin consoles. SystemProperties is a
// family of executables, matched by prefix.
fn is_target() -> bool {
    let lower = process_base_name().to_ascii_lowercase();
    lower == "explorer.exe"
        || lower == "control.exe"
        || lower == "rundll32.exe"
        || lower == "mmc.exe"
        || (lower.starts_with("systemproperties") && lower.ends_with(".exe"))
}

fn map_state() {
    unsafe {
        let name = wide(r"Local\OledHelperExplorerHookState");
        let Ok(mapping) =
            OpenFileMappingW(FILE_MAP_READ.0 | FILE_MAP_WRITE.0, false, PCWSTR(name.as_ptr()))
        else {
            return;
        };
        let view =
            MapViewOfFile(mapping, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, STATE_SIZE as usize);
        if !view.Value.is_null() {
            let _ = STATE.set(view.Value as usize);
        }
        let _ = CloseHandle(mapping);
    }
}

// ── Text color helpers ─────────────────────────────────────────────────

// COLORREF is 0x00BBGGRR: low byte = red, then green, then blue.
fn text_color_rgb(hdc: HDC) -> (u8, u8, u8) {
    let raw = unsafe { GetTextColor(hdc) }.0;
    let r = (raw & 0xFF) as u8;
    let g = ((raw >> 8) & 0xFF) as u8;
    let b = ((raw >> 16) & 0xFF) as u8;
    (r, g, b)
}

// Per-channel linear blend: out = text*cov + bg*(1-cov), cov in [0, 255]. This
// is the per-subpixel compositing ClearType uses for the colored fringes.
fn blend_channel(text: u8, bg: u8, cov: u8) -> u8 {
    ((text as u32 * cov as u32 + bg as u32 * (255 - cov as u32)) / 255) as u8
}

// ── Blit ──────────────────────────────────────────────────────────────

// Compute the tight bounding box of a rasterized run in run-local coordinates
// (origin = left edge + baseline). Returns (left, top, width, height).
fn run_bounds(run: &text_raster::RasterizedText) -> Option<(i32, i32, i32, i32)> {
    if run.glyphs.is_empty() {
        return None;
    }
    let mut min_left = i32::MAX;
    let mut min_top = i32::MAX;
    let mut max_right = i32::MIN;
    let mut max_bottom = i32::MIN;
    for g in &run.glyphs {
        let left = g.x.floor() as i32 + g.bitmap_left;
        // `bitmap_top` is measured UPWARD from the baseline (swash's y-up
        // Origin::BottomLeft), but the DIB is top-down, so it must be negated
        // to keep every glyph on the same baseline.
        let top = g.y.floor() as i32 - g.bitmap_top;
        let right = left + g.width as i32;
        let bottom = top + g.height as i32;
        min_left = min_left.min(left);
        min_top = min_top.min(top);
        max_right = max_right.max(right);
        max_bottom = max_bottom.max(bottom);
    }
    Some((min_left, min_top, (max_right - min_left).max(1), (max_bottom - min_top).max(1)))
}

// Alpha-blend the rasterized run into the HDC at the text reference point,
// honoring the HDC's text alignment. Transparent background only (opaque /
// ETO_OPAQUE handled in a later pass).
unsafe fn blit_run(
    hdc: HDC,
    x: i32,
    y: i32,
    run: &text_raster::RasterizedText,
    color: (u8, u8, u8),
) -> bool {
    let Some((min_left, min_top, w, h)) = run_bounds(run) else {
        return true; // empty run, nothing to draw
    };
    // A malformed glyph position must never produce a giant allocation.
    if w <= 0 || h <= 0 || w > 8192 || h > 8192 {
        return false;
    }

    // Build a 32bpp BGRA DIB section.
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    bmi.bmiHeader.biHeight = -h; // negative = top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let hbitmap = match CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
        Ok(hb) => hb,
        Err(_) => return false,
    };
    if hbitmap.is_invalid() || bits.is_null() {
        if !hbitmap.is_invalid() {
            let _ = DeleteObject(hbitmap.into());
        }
        return false;
    }

    let memdc = CreateCompatibleDC(Some(hdc));
    let old = SelectObject(memdc, hbitmap.into());

    // Translate the reference point to the run origin using text alignment.
    let align = GetTextAlign(hdc);
    let origin_x = match align.0 & 0x6 {
        v if v == TA_RIGHT.0 => x - run.advance as i32,
        v if v == TA_CENTER.0 => x - (run.advance / 2.0) as i32,
        _ => x,
    };
    // GDI positions text by the font's alignment box (tmAscent/tmDescent),
    // not the tight ink bounds, so the baseline is placed from the font
    // metrics rather than the glyphs' ink extents.
    let origin_y = match align.0 & 0x18 {
        v if v == TA_BASELINE.0 => y,
        v if v == TA_BOTTOM.0 => y - (run.descent as i32),
        _ => y + (run.ascent as i32),
    };

    let dest_x = origin_x + min_left;
    let dest_y = origin_y + min_top;

    let buf = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);

    let ok = match run.content {
        // Grayscale/crisp: composite coverage into a fresh DIB as premultiplied
        // BGRA text, then AlphaBlend so the destination shows through the
        // transparent pixels.
        Content::Mask => {
            buf.fill(0);
            for g in &run.glyphs {
                let dst_left = g.x.floor() as i32 + g.bitmap_left - min_left;
                let dst_top = g.y.floor() as i32 - g.bitmap_top - min_top;
                for row in 0..g.height as usize {
                    for col in 0..g.width as usize {
                        let c = g.coverage[row * g.width as usize + col];
                        if c == 0 {
                            continue;
                        }
                        let dx = dst_left as usize + col;
                        let dy = dst_top as usize + row;
                        if dx >= w as usize || dy >= h as usize {
                            continue;
                        }
                        let i = (dy * w as usize + dx) * 4;
                        buf[i] = (color.2 as u32 * c as u32 / 255) as u8; // B
                        buf[i + 1] = (color.1 as u32 * c as u32 / 255) as u8; // G
                        buf[i + 2] = (color.0 as u32 * c as u32 / 255) as u8; // R
                        buf[i + 3] = c; // A
                    }
                }
            }
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            AlphaBlend(hdc, dest_x, dest_y, w, h, memdc, 0, 0, w, h, blend).as_bool()
        }
        // Subpixel: read the destination background, blend each color channel
        // against its own coverage, and write the result back. This is the
        // per-channel compositing ClearType uses for its colored fringes.
        Content::SubpixelMask => {
            let read_ok = BitBlt(memdc, 0, 0, w, h, Some(hdc), dest_x, dest_y, SRCCOPY).is_ok();
            if !read_ok {
                false
            } else {
                for g in &run.glyphs {
                    let dst_left = g.x.floor() as i32 + g.bitmap_left - min_left;
                    let dst_top = g.y.floor() as i32 - g.bitmap_top - min_top;
                    for row in 0..g.height as usize {
                        for col in 0..g.width as usize {
                            let dx = dst_left as usize + col;
                            let dy = dst_top as usize + row;
                            if dx >= w as usize || dy >= h as usize {
                                continue;
                            }
                            let src = (row * g.width as usize + col) * 4;
                            let rc = g.coverage[src];
                            let gc = g.coverage[src + 1];
                            let bc = g.coverage[src + 2];
                            let i = (dy * w as usize + dx) * 4;
                            buf[i] = blend_channel(color.2, buf[i], bc); // B
                            buf[i + 1] = blend_channel(color.1, buf[i + 1], gc); // G
                            buf[i + 2] = blend_channel(color.0, buf[i + 2], rc); // R
                            buf[i + 3] = 255; // opaque result
                        }
                    }
                }
                BitBlt(hdc, dest_x, dest_y, w, h, Some(memdc), 0, 0, SRCCOPY).is_ok()
            }
        }
        Content::Color => false,
    };

    let _ = SelectObject(memdc, old);
    let _ = DeleteObject(hbitmap.into());
    let _ = DeleteDC(memdc);
    ok
}

// ── Detour ────────────────────────────────────────────────────────────

// Try to render this ExtTextOutW call with swash. Returns true when the call
// was fully handled; false means the caller should fall back to GDI.
//
// lprect/dx are plain raw pointers (null = the Option's None) rather than
// `Option<*const _>`: the compiler was miscompiling the niche-Option form in
// calls by dereferencing the pointer as if it were a by-reference 16-byte
// struct, which crashes on null. Raw pointers have a single unambiguous ABI.
unsafe extern "system" fn try_render_swash(
    hdc: HDC,
    x: i32,
    y: i32,
    options: ETO_OPTIONS,
    _lprect: *const RECT,
    string: PCWSTR,
    count: u32,
    dx: *const i32,
) -> bool {
    let mode = state_mode();
    let render_mode = match mode {
        1 => text_raster::RenderMode::Grayscale,
        2 => text_raster::RenderMode::Crisp,
        3 => text_raster::RenderMode::SubpixelRgb,
        4 => text_raster::RenderMode::SubpixelBgr,
        5 => text_raster::RenderMode::SubpixelRgwb,
        6 => text_raster::RenderMode::SubpixelRwbg,
        _ => return false,
    };
    // Only count attempts that actually reach the swash path (mode 1/2), so
    // passthrough (mode 0) draws don't flood the log.
    let n = RENDER_COUNT.fetch_add(1, Ordering::Relaxed);
    let verbose = n < 100 || n % 10000 == 0;
    let fallback = |reason: &str| {
        if verbose {
            log(&format!("render #{n}: fallback ({reason})"));
        }
        false
    };
    if verbose {
        log(&format!("render #{n}: attempt mode={mode} count={count}"));
    }

    // Bound the text so a pathological call cannot drive unbounded work.
    if count == 0 || count > 4096 {
        return fallback("count out of range");
    }
    // ETO_GLYPH_INDEX: the string is an array of glyph indices, not UTF-16.
    if options.0 & ETO_GLYPH_INDEX.0 != 0 {
        return try_render_glyph_indices(hdc, x, y, string, count, dx, render_mode);
    }
    if options.0 & ETO_PDY.0 != 0 {
        return fallback(&format!("ETO_PDY (options={:#x})", options.0));
    }
    // `dx` sets the distance to the NEXT character cell, so for a single
    // glyph it has no effect on where that glyph lands. Render single-glyph
    // draws through swash so one-off characters (ellipsis and friends) get
    // grayscale instead of ClearType. Multi-glyph custom spacing still falls
    // back for now.
    if !dx.is_null() && count > 1 {
        return fallback("dx spacing");
    }

    let Some((font_data, px)) = resolve_hdc_font(hdc) else {
        return fallback("font not resolved");
    };

    let text = String::from_utf16_lossy(std::slice::from_raw_parts(string.as_ptr(), count as usize));
    let opts = state_render_options();
    let Some(run) = text_raster::rasterize(&font_data, 0, px, &text, render_mode, opts) else {
        return fallback("rasterize failed");
    };

    if verbose {
        log(&format!(
            "render #{n}: px={px} glyphs={} -> blit",
            run.glyphs.len()
        ));
    }
    let color = text_color_rgb(hdc);
    blit_run(hdc, x, y, &run, color)
}

// Resolve the font selected into the HDC to (file bytes, pixel size). Shared
// by the character and glyph-index paths.
unsafe fn resolve_hdc_font(hdc: HDC) -> Option<(std::sync::Arc<[u8]>, f32)> {
    let hgdi = GetCurrentObject(hdc, OBJ_FONT);
    if hgdi.is_invalid() {
        return None;
    }
    let mut logfont = LOGFONTW::default();
    let got = GetObjectW(
        hgdi,
        std::mem::size_of::<LOGFONTW>() as i32,
        Some(&mut logfont as *mut LOGFONTW as *mut core::ffi::c_void),
    );
    if got == 0 {
        return None;
    }
    let face: Vec<u16> = logfont
        .lfFaceName
        .iter()
        .take_while(|&&c| c != 0)
        .copied()
        .collect();
    let face = String::from_utf16_lossy(&face);
    if face.is_empty() {
        return None;
    }
    let font_data = font::cached_font(&face, logfont.lfWeight, logfont.lfItalic != 0)?;
    // Pixel size from lfHeight (negative = character height, the common case).
    let px = logfont.lfHeight.unsigned_abs().max(1) as f32;
    if px > 512.0 {
        return None;
    }
    Some((font_data, px))
}

// Handle ETO_GLYPH_INDEX: the string is an array of `count` u16 glyph indices
// (already shaped by GDI), optionally with a `dx` array of per-glyph advances
// in logical units (MM_TEXT => pixels). Rasterize each glyph by ID and blit.
unsafe fn try_render_glyph_indices(
    hdc: HDC,
    x: i32,
    y: i32,
    string: PCWSTR,
    count: u32,
    dx: *const i32,
    mode: text_raster::RenderMode,
) -> bool {
    let n = RENDER_COUNT.fetch_add(1, Ordering::Relaxed);
    let verbose = n < 100 || n % 10000 == 0;
    if verbose {
        log(&format!("render #{n}: glyph-index count={count}"));
    }

    let Some((font_data, px)) = resolve_hdc_font(hdc) else {
        if verbose {
            log(&format!("render #{n}: glyph-index fallback (font not resolved)"));
        }
        return false;
    };

    let glyph_ids = std::slice::from_raw_parts(string.as_ptr() as *const u16, count as usize);
    let advances: Vec<f32> = if dx.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(dx, count as usize)
            .iter()
            .map(|&a| a as f32)
            .collect()
    };

    let opts = state_render_options();
    let Some(run) = text_raster::rasterize_glyphs(
        &font_data,
        0,
        px,
        glyph_ids,
        &advances,
        mode,
        opts,
    ) else {
        if verbose {
            log(&format!("render #{n}: glyph-index fallback (rasterize failed)"));
        }
        return false;
    };

    if verbose {
        log(&format!(
            "render #{n}: glyph-index px={px} glyphs={} -> blit",
            run.glyphs.len()
        ));
    }
    let color = text_color_rgb(hdc);
    blit_run(hdc, x, y, &run, color)
}

// Re-entrancy guard: gdi32!ExtTextOutW calls back into itself internally, so
// while we're already inside the hook (on this thread) the original's nested
// call must pass straight through instead of recursing forever.
thread_local! {
    static IN_EXT_TEXT_OUT: Cell<bool> = const { Cell::new(false) };
}

// Invoke the original ExtTextOutW (through the trampoline, since the entry is
// patched). Shared by the fallback and re-entrant paths.
unsafe extern "system" fn call_original_ext_text_out(
    hdc: HDC,
    x: i32,
    y: i32,
    options: ETO_OPTIONS,
    lprect: *const RECT,
    string: PCWSTR,
    count: u32,
    dx: *const i32,
) -> BOOL {
    match DETOUR.get() {
        Some(detour) => {
            let orig: ExtTextOutWFn = std::mem::transmute(detour.trampoline());
            orig(hdc, x, y, options, lprect, string, count, dx)
        }
        None => ORIGINAL
            .get()
            .map(|orig| orig(hdc, x, y, options, lprect, string, count, dx))
            .unwrap_or(BOOL(0)),
    }
}

#[allow(improper_ctypes_definitions)]
unsafe extern "system" fn hook_ext_text_out_w(
    hdc: HDC,
    x: i32,
    y: i32,
    options: ETO_OPTIONS,
    lprect: *const RECT,
    string: PCWSTR,
    count: u32,
    dx: *const i32,
) -> BOOL {
    // Re-entrant call from the original ExtTextOutW: pass straight through so
    // its nested call can complete instead of recursing forever.
    if IN_EXT_TEXT_OUT.with(|f| f.get()) {
        return call_original_ext_text_out(hdc, x, y, options, lprect, string, count, dx);
    }
    IN_EXT_TEXT_OUT.with(|f| f.set(true));

    // A panic in the swash path must never take down the shell: catch it and
    // fall back to GDI untouched.
    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        try_render_swash(hdc, x, y, options, lprect, string, count, dx)
    }));
    let result = match rendered {
        Ok(true) => BOOL(1),
        Ok(false) => call_original_ext_text_out(hdc, x, y, options, lprect, string, count, dx),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            log(&format!("PANIC in swash path (falling back to GDI): {msg}"));
            call_original_ext_text_out(hdc, x, y, options, lprect, string, count, dx)
        }
    };
    IN_EXT_TEXT_OUT.with(|f| f.set(false));
    result
}

// ── Safe patching ─────────────────────────────────────────────────────
// The inline patch (retour) overwrites the function entry with a multi-byte
// jump that is NOT written atomically. ExtTextOutW is a hot function called
// from many threads at once, so a thread executing the entry mid-write would
// run a half-written instruction and crash the shell (which Windows then
// auto-restarts). Suspending every other thread while the bytes are written
// closes that window (the same technique MinHook uses).

struct SuspendedThread(HANDLE);

impl Drop for SuspendedThread {
    fn drop(&mut self) {
        // SAFETY: handle came from OpenThread + SuspendThread above.
        unsafe {
            let _ = ResumeThread(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

fn suspend_other_threads() -> Vec<SuspendedThread> {
    let pid = std::process::id();
    let current = unsafe { GetCurrentThreadId() };
    let mut suspended = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return suspended;
        };
        let mut entry = THREADENTRY32::default();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        if Thread32First(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32OwnerProcessID == pid && entry.th32ThreadID != current {
                    if let Ok(handle) =
                        OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)
                    {
                        if SuspendThread(handle) != u32::MAX {
                            suspended.push(SuspendedThread(handle));
                        } else {
                            let _ = CloseHandle(handle);
                        }
                    }
                }
                if Thread32Next(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    suspended
}

fn find_ext_text_out_w() -> Option<ExtTextOutWFn> {
    unsafe {
        let module = GetModuleHandleW(w!("gdi32.dll")).ok()?;
        let proc = GetProcAddress(module, s!("ExtTextOutW"))?;
        Some(std::mem::transmute(proc))
    }
}

// Install the ExtTextOutW detour, then report the status so the app can tell
// "hooked" from "DLL loaded but detour failed".
fn install_detour() -> bool {
    if DETOUR.get().is_some() {
        return true;
    }
    // Resolve the entry point BEFORE suspending threads: GetModuleHandleW /
    // GetProcAddress acquire the loader lock, and a suspended thread could be
    // holding it (e.g. a shell-extension DLL being loaded while a window is
    // created) — that would deadlock the whole shell.
    let Some(original) = find_ext_text_out_w() else {
        debug("[explorer-hook] ExtTextOutW not found; no hook installed");
        return false;
    };
    // Make the direct-call fallback available before the patch goes live.
    let _ = ORIGINAL.set(original);
    // Suspend only around the lock-free patch (disassembly + memory write).
    let suspended = suspend_other_threads();
    let ok = enable_ext_text_out_detour(original);
    drop(suspended);
    if is_explorer() {
        report_state(if ok { STATUS_HOOK_INSTALLED } else { 0 });
    }
    ok
}

fn enable_ext_text_out_detour(original: ExtTextOutWFn) -> bool {
    // SAFETY: the detour is created once, enabled, and kept alive in DETOUR
    // for the lifetime of the process. Other threads are suspended by the
    // caller, so the non-atomic patch write cannot race a mid-call thread.
    unsafe {
        let Some(detour) =
            Detour::new(original as usize, hook_ext_text_out_w as *const () as usize)
        else {
            debug("[explorer-hook] detour setup failed");
            return false;
        };
        if !detour.enable() {
            debug("[explorer-hook] detour enable failed");
            return false;
        }
        let _ = DETOUR.set(detour);
        debug("[explorer-hook] ExtTextOutW detour installed");
        true
    }
}

// Restore the pristine function bytes on DLL_PROCESS_DETACH so a later
// re-enable patches a clean entry.
fn disable_detours() {
    let suspended = suspend_other_threads();
    // SAFETY: disable() restores the original prolog. The trampoline is
    // intentionally leaked (a few dozen bytes) rather than freed, so a thread
    // that was suspended mid-fallback can't resume into freed memory.
    unsafe {
        if let Some(detour) = DETOUR.get() {
            detour.disable();
        }
    }
    drop(suspended);
}

// Exported so the app can hand it to SetWindowsHookExW. The hook only exists
// to make Windows load this DLL into the targets through the documented hook
// mechanism. The first call also runs the deferred detour install (out of
// DllMain / the loader lock); every callback is then forwarded straight through.
#[no_mangle]
pub unsafe extern "system" fn hook_callback(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if IS_TARGET.load(Ordering::Acquire) && !INSTALL_ATTEMPTED.swap(true, Ordering::SeqCst) {
        log("hook_callback: first window event, installing detour");
        if !install_detour() {
            INSTALL_ATTEMPTED.store(false, Ordering::SeqCst);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

#[cfg(test)]
mod prolog_dump {
    use super::*;

    #[test]
    fn dump_ext_text_out_bytes() {
        unsafe {
            let m = LoadLibraryW(w!("gdi32.dll"));
            println!("gdi32 module handle: {m:?}");
            let Ok(m) = m else { return; };
            let p = GetProcAddress(m, s!("ExtTextOutW"));
            println!("ExtTextOutW proc: {p:?}");
            let Some(p) = p else { return; };
            let addr = p as usize as *const u8;
            let bytes = std::slice::from_raw_parts(addr, 48);
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
            println!("ExtTextOutW first 48 bytes: {}", hex.join(" "));
            // The `call rel32` at offset 0x28 targets ExtTextOutW+0x8F90.
            let helper = addr.add(0x8F90);
            let hb = std::slice::from_raw_parts(helper, 32);
            let hhex: Vec<String> = hb.iter().map(|b| format!("{b:02X}")).collect();
            println!("helper(+0x8F90) first 32 bytes: {}", hhex.join(" "));
        }
    }

    // Hook a local function (which cannot re-enter) to prove the detour and
    // trampoline themselves are sound, independent of ExtTextOutW re-entrancy.
    #[test]
    fn detour_local_roundtrip() {
        unsafe {
            type AddOneFn = unsafe extern "system" fn(i32) -> i32;

            #[inline(never)]
            unsafe extern "system" fn add_one(x: i32) -> i32 {
                x + 1
            }

            static LOCAL: OnceLock<Detour> = OnceLock::new();

            #[inline(never)]
            unsafe extern "system" fn add_one_hook(x: i32) -> i32 {
                let orig: AddOneFn = std::mem::transmute(LOCAL.get().unwrap().trampoline());
                orig(x) + 100
            }

            let d = Detour::new(add_one as *const () as usize, add_one_hook as *const () as usize)
                .expect("new");
            d.enable();
            let _ = LOCAL.set(d);

            println!("add_one(5) via hook = {} (expect 106)", add_one(5));
            let orig: AddOneFn = std::mem::transmute(LOCAL.get().unwrap().trampoline());
            println!("trampoline(5) = {} (expect 6)", orig(5));
        }
    }

    // Round-trip the original ExtTextOutW through our trampoline WITHOUT
    // explorer. Exercises the re-entrancy guard, since gdi32!ExtTextOutW calls
    // back into itself internally.
    #[test]
    fn trampoline_roundtrip() {
        unsafe {
            let original = find_ext_text_out_w().expect("find ExtTextOutW");
            let hdc = windows::Win32::Graphics::Gdi::GetDC(None);
            let text = wide("test");

            #[allow(improper_ctypes_definitions)]
            unsafe extern "system" fn noop(
                hdc: HDC,
                x: i32,
                y: i32,
                o: ETO_OPTIONS,
                r: *const RECT,
                s: PCWSTR,
                c: u32,
                d: *const i32,
            ) -> BOOL {
                if IN_EXT_TEXT_OUT.with(|f| f.get()) {
                    return call_original_ext_text_out(hdc, x, y, o, r, s, c, d);
                }
                IN_EXT_TEXT_OUT.with(|f| f.set(true));
                let out = call_original_ext_text_out(hdc, x, y, o, r, s, c, d);
                IN_EXT_TEXT_OUT.with(|f| f.set(false));
                out
            }

            let detour = Detour::new(original as usize, noop as *const () as usize).expect("new");
            assert!(detour.enable());
            let _ = DETOUR.set(detour);

            for i in 0..3 {
                let r = original(
                    hdc,
                    10 * i,
                    10 * i,
                    ETO_OPTIONS(0),
                    std::ptr::null(),
                    PCWSTR(text.as_ptr()),
                    4,
                    std::ptr::null(),
                );
                assert!(r.as_bool(), "roundtrip #{i} should return TRUE");
            }
            let _ = windows::Win32::Graphics::Gdi::ReleaseDC(None, hdc);
        }
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _hinst: windows::Win32::Foundation::HINSTANCE,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            let target = is_target();
            IS_TARGET.store(target, Ordering::Release);
            if target {
                map_state();
            }
            BOOL(1)
        }
        DLL_PROCESS_DETACH => {
            if IS_TARGET.load(Ordering::Acquire) {
                disable_detours();
            }
            BOOL(1)
        }
        DLL_THREAD_ATTACH | DLL_THREAD_DETACH => BOOL(1),
        _ => BOOL(1),
    }
}
