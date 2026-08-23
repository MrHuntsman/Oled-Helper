//! explorer-hook.dll, inject a GDI text-rendering replacement into the shell
//! and the classic Control Panel hosts via a global WH_CBT hook.
//!
//! Instead of rewriting the font-quality flag at CreateFontIndirectW (which
//! only affected fonts created after a window was built), we hook the text
//! OUTPUT path (ExtTextOutW) and rasterize each draw with `swash`, a pure-Rust
//! glyph rasterizer, so a mode change is picked up on the very next repaint
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
    GetBkColor, GetCurrentObject, GetDeviceCaps, GetObjectW, GetTextAlign, GetTextColor,
    HBITMAP, HDC, LOGFONTW, MonitorFromWindow, SelectObject, SetDIBitsToDevice, WindowFromDC,
    AC_SRC_ALPHA,
    AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS,
    ETO_GLYPH_INDEX, ETO_OPAQUE, ETO_OPTIONS, ETO_PDY, HORZRES, HORZSIZE, LOGPIXELSX,
    LOGPIXELSY, MONITOR_DEFAULTTONEAREST, OBJ_FONT, SRCCOPY, TA_BASELINE, TA_BOTTOM,
    TA_CENTER, TA_RIGHT, VERTRES, VERTSIZE, GET_DEVICE_CAPS_INDEX,
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

// Shared memory layout (5348 bytes): the app writes the mode at offset 0 and the DLL
// writes its own state back at offsets 4 and 8 so the app can verify the hook
// is really installed in the CURRENT explorer.exe instead of trusting that
// the module merely loaded. Offset 12 holds the hook-scope flags (bit 0 =
// "apply to all apps"); offset 16 is the loaded-process counter. The
// per-monitor table
// lives at 24/28/32+ (flag, count, up to 8 entries x 72 bytes), each entry
// carrying the monitor's mode, knob pair (boldness, damping; the lcd
// filter / crosstalk / gamma / filter-width slots are reserved, always 0),
// and custom subpixel centers as six (x, y) f32 bit patterns, see
// resolve_draw_options. The blocklist follows the
// table at 608+ (count, up to 8 entries x 64 bytes = 32 UTF-16 chars): exe
// names excluded from the detour while "all apps" scope is on.
const STATE_MODE: usize = 0;
const STATE_STATUS: usize = 4;
const STATE_PID: usize = 8;
const STATE_SCOPE: usize = 12;
const STATE_PROCESS_COUNT: usize = 16;
const STATE_PRIMARY_EXPLORER_PID: usize = 20;
const STATE_PER_MONITOR: usize = 24;
const STATE_MONITOR_COUNT: usize = 28;
const STATE_MONITORS: usize = 32;
const MONITOR_ENTRY_STRIDE: usize = 72;
const MAX_MONITORS: usize = 8;
const STATE_BLOCKLIST_COUNT: usize = 608;
const STATE_BLOCKLIST_ENTRIES: usize = 612;
const BLOCKLIST_ENTRY_WORDS: usize = 32;
const BLOCKLIST_ENTRY_STRIDE: usize = BLOCKLIST_ENTRY_WORDS * 2;
const MAX_BLOCKLIST: usize = 8;
const PROCESS_NAME_WORDS: usize = 64;
const PROCESS_SLOT_STRIDE: usize = 4 + PROCESS_NAME_WORDS * 2;
const MAX_PROCESS_SLOTS: usize = 32;
const STATE_PROCESS_SLOTS: usize =
    STATE_BLOCKLIST_ENTRIES + MAX_BLOCKLIST * BLOCKLIST_ENTRY_STRIDE;
const STATE_SIZE: usize = STATE_PROCESS_SLOTS + MAX_PROCESS_SLOTS * PROCESS_SLOT_STRIDE;
const PROCESS_SLOT_WRITING: u32 = 0x8000_0000;
const NO_PROCESS_SLOT: u32 = u32::MAX;
static PROCESS_SLOT: AtomicU32 = AtomicU32::new(NO_PROCESS_SLOT);
static PROCESS_REGISTERING: AtomicBool = AtomicBool::new(false);
// Scope flag bits at STATE_SCOPE.
const SCOPE_HOOK_ALL: u32 = 1;
// Status bit written by the DLL once the ExtTextOutW detour is installed.
const STATUS_HOOK_INSTALLED: u32 = 1;

// Layout-generation marker embedded in the DLL. The app refuses to serve a
// locked on-disk copy that lacks the CURRENT marker (an older shared-memory
// layout could misread the table or read past the app's mapping), and
// instead prefers the alternate slot carrying the current bytes. KEEP THIS
// IN SYNC with the app's HOOK_LAYOUT_MARKER in src/explorer_hook.rs and bump
// it whenever STATE_SIZE / the shared-memory layout changes.
#[used]
#[no_mangle]
pub static HOOK_LAYOUT_MARKER: [u8; 13] = *b"OH2-SMEM-5349";

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
// Whether this process is a shell/Control Panel host (cached in DllMain so
// the hook callback doesn't re-query the process image path on every
// window-creation event). The all-apps scope decision is NOT cached here:
// it is re-evaluated per draw from the shared-memory flag + blocklist so the
// app's "apply to all apps" toggle and blocklist edits are live for
// already-loaded processes.
static IS_TARGET: AtomicBool = AtomicBool::new(false);

// ── File logging (%TEMP%\OledHelperHook.log) ───────────────────────────
// The hook runs inside explorer.exe and friends, where OutputDebugStringW is
// invisible to the app. An append-only file survives a process crash (the OS
// flushes the handle on exit), so the last line before a crash is the clue.
static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static LOG_START: OnceLock<Instant> = OnceLock::new();
static PROC_NAME: OnceLock<String> = OnceLock::new();
static RENDER_COUNT: AtomicU32 = AtomicU32::new(0);
// Count of draws that took the opaque (ETO_OPAQUE) fast path, the plain
// buffer + SetDIBitsToDevice route. Exposed in the verbose log so we can see
// how much of Explorer's text is opaque (list views etc.).
static OPAQUE_DRAWS: AtomicU32 = AtomicU32::new(0);

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

// ── Per-monitor mode resolution ───────────────────────────────────────
// The app writes a monitor table (system-wide HMONITOR handles + device-caps
// signatures + per-monitor mode) into the shared memory. Each draw's DC is
// attributed to a monitor and rendered with that monitor's mode. Window DCs
// match by HMONITOR (MonitorFromWindow); memory DCs have no window, so they
// match by the device-caps signature (resolution + physical size + DPI). The
// counters show in the verbose log which attribution path fired.

static MON_WINDOW: AtomicU32 = AtomicU32::new(0);
static MON_CAPS: AtomicU32 = AtomicU32::new(0);
static MON_NONE: AtomicU32 = AtomicU32::new(0);

fn state_per_monitor_enabled() -> bool {
    // SAFETY: STATE is written once in map_state; the mapped view stays valid
    // for the lifetime of the process.
    unsafe {
        STATE.get()
            .map(|addr| *((addr + STATE_PER_MONITOR) as *const u32) != 0)
            .unwrap_or(false)
    }
}

fn state_monitor_count() -> usize {
    // SAFETY: as above; the count is clamped so a stale app build cannot make
    // us read past the mapping.
    unsafe {
        STATE.get()
            .map(|addr| (*((addr + STATE_MONITOR_COUNT) as *const u32) as usize).min(MAX_MONITORS))
            .unwrap_or(0)
    }
}

// Entry i as (hmonitor, resolution, physical size, dpi, mode | flags<<8,
// lcd filter (reserved), crosstalk (reserved), boldness, gamma (reserved),
// damping, filter width (reserved), subpixel r/g/b x/y as u32 bit patterns).
// The reserved slots are kept at 0 by the app (removed filters) to keep the
// layout stable. Offsets within an entry are all 4-aligned (STATE_MONITORS
// is 32).
fn state_monitor_entry(
    i: usize,
) -> Option<(u32, u32, u32, u32, u32, u32, u32, u32, u32, u32, u32, u32, u32, u32, u32, u32, u32)> {
    if i >= state_monitor_count() {
        return None;
    }
    // SAFETY: STATE is valid for the process lifetime and the index is
    // clamped to the written count (<= MAX_MONITORS).
    unsafe {
        let base = STATE.get()? + STATE_MONITORS + i * MONITOR_ENTRY_STRIDE;
        Some((
            *((base) as *const u32),
            *((base + 4) as *const u32),
            *((base + 8) as *const u32),
            *((base + 12) as *const u32),
            *((base + 16) as *const u32),
            *((base + 20) as *const u32),
            *((base + 24) as *const u32),
            *((base + 28) as *const u32),
            *((base + 32) as *const u32),
            *((base + 36) as *const u32),
            *((base + 40) as *const u32),
            *((base + 44) as *const u32),
            *((base + 48) as *const u32),
            *((base + 52) as *const u32),
            *((base + 56) as *const u32),
            *((base + 60) as *const u32),
            *((base + 64) as *const u32),
        ))
    }
}

// Device-caps signature of an HDC, packed exactly like the app's table
// entries so memory-DC draws can be matched when no window is available.
fn hdc_caps_sig(hdc: HDC) -> (u32, u32, u32) {
    unsafe {
        let caps = |index: GET_DEVICE_CAPS_INDEX| GetDeviceCaps(Some(hdc), index).max(0) as u32;
        (
            (caps(HORZRES) & 0xFFFF) | (caps(VERTRES) << 16),
            (caps(HORZSIZE) & 0xFFFF) | (caps(VERTSIZE) << 16),
            (caps(LOGPIXELSX) & 0xFFFF) | (caps(LOGPIXELSY) << 16),
        )
    }
}

// Whether this draw's DC is a DPI-virtualized surface: the DC reports a
// LOWER DPI than the monitor it belongs to. System-DPI-aware processes like
// mmc.exe render into a 96-DPI surface that Windows stretches to the
// monitor's scale, so whatever we rasterize at the logical pixel size gets
// upscaled with the whole window and reads as low-resolution (the log shows
// mmc at px=12 while Explorer draws at px=24-27 on the same monitor). GDI's
// hinted ClearType survives that stretch far better than swash coverage, so
// those draws pass through untouched. Per-monitor-aware processes (Explorer,
// the Control Panel host) draw into DCs at the monitor DPI and stay hooked.
fn hdc_dpi_virtualized(hdc: HDC) -> bool {
    unsafe {
        let dc_dpi = GetDeviceCaps(Some(hdc), LOGPIXELSX).max(0) as u32;
        if dc_dpi == 0 {
            return false;
        }
        // Window DC: attribute by HWND → HMONITOR and compare with the
        // table entry's DPI (x dpi in the low 16 bits).
        let hwnd = WindowFromDC(hdc);
        if !hwnd.0.is_null() {
            let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if !hmon.0.is_null() {
                let want = hmon.0 as usize as u32;
                for i in 0..state_monitor_count() {
                    if let Some((h, _, _, dpi, ..)) = state_monitor_entry(i) {
                        if h == want {
                            return dc_dpi < (dpi & 0xFFFF);
                        }
                    }
                }
            }
        }
        // Memory DC: match by physical size. The mm size is a monitor
        // property and unaffected by virtualization, while the resolution
        // and DPI slots are scaled, so a virtualized DC's caps signature
        // cannot match on those (that is why these draws fall to this path).
        let (_, phys, _) = hdc_caps_sig(hdc);
        for i in 0..state_monitor_count() {
            if let Some((_, _, ep, ed, ..)) = state_monitor_entry(i) {
                if ep == phys {
                    return dc_dpi < (ed & 0xFFFF);
                }
            }
        }
        false
    }
}// Per-monitor render options for this draw's DC, when per-monitor is enabled:
// the attributed monitor's mode (0 = system default / passthrough), its knob
// pair (boldness, damping; 0 = no adjustment), and its custom subpixel
// centers as per-channel (x, y) pairs (all-zero = no custom offsets; only
// meaningful for mode 7, Custom). A monitor that cannot be attributed yields
// None → the caller falls back to the global mode + fallback knobs. Every
// connected monitor is in the table, so a successful attribution is the
// normal path; the table entry's mode is 0 for monitors left at the system
// default, which still resolves here so their knobs can apply.
fn resolve_draw_options(hdc: HDC) -> Option<(u32, u8, u8, [[f32; 2]; 3])> {
    let count = state_monitor_count();
    if count == 0 {
        return None;
    }
    // Boldness is capped at 30 by the app (≈0.3px of stroke thickening);
    // the clamp here is defense-in-depth for stale shared-memory values.
    let knobs_of = |boldness: u32, damping: u32| -> (u8, u8) {
        (boldness.min(30) as u8, damping.min(100) as u8)
    };
    let offsets_of = |rx: u32, ry: u32, gx: u32, gy: u32, bx: u32, by: u32| -> [[f32; 2]; 3] {
        [
            [f32::from_bits(rx), f32::from_bits(ry)],
            [f32::from_bits(gx), f32::from_bits(gy)],
            [f32::from_bits(bx), f32::from_bits(by)],
        ]
    };
    // 1. Window DC: HWND → monitor → exact HMONITOR match.
    let hwnd = unsafe { WindowFromDC(hdc) };
    if !hwnd.0.is_null() {
        let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        if !hmon.0.is_null() {
            let want = hmon.0 as usize as u32;
            for i in 0..count {
                if let Some((h, _, _, _, mf, _lcd, _ct, bd, _gm, dm, _fw, srx, sry, sgx, sgy, sbx, sby)) =
                    state_monitor_entry(i)
                {
                    if h == want {
                        MON_WINDOW.fetch_add(1, Ordering::Relaxed);
                        let (b, d) = knobs_of(bd, dm);
                        return Some((mf & 0xFF, b, d, offsets_of(srx, sry, sgx, sgy, sbx, sby)));
                    }
                }
            }
        }
    }
    // 2. Memory DC: match the device-caps signature against the table.
    let (res, phys, dpi) = hdc_caps_sig(hdc);
    for i in 0..count {
        if let Some((_, er, ep, ed, mf, _lcd, _ct, bd, _gm, dm, _fw, srx, sry, sgx, sgy, sbx, sby)) =
            state_monitor_entry(i)
        {
            if er == res && ep == phys && ed == dpi {
                MON_CAPS.fetch_add(1, Ordering::Relaxed);
                let (b, d) = knobs_of(bd, dm);
                return Some((mf & 0xFF, b, d, offsets_of(srx, sry, sgx, sgy, sbx, sby)));
            }
        }
    }

    MON_NONE.fetch_add(1, Ordering::Relaxed);
    None
}

// Fallback when a draw's DC cannot be attributed to a monitor: the PRIMARY
// monitor's entry. Explorer's list views draw through memory DCs that carry
// bitmaps (GetDeviceCaps reports the selected bitmap's dimensions, not the
// screen's) and whose WindowFromDC is NULL, so neither the window path nor
// the caps path matches them. Without this fallback those draws would render
// with zero knobs while the mode still applies through the global slot — the
// "grayscale works but boldness doesn't" symptom. The shell's windows live on
// the primary, so its entry is the sane default; the caller still maps a 0
// mode to the global mode exactly like a matched entry.
fn resolve_primary_options() -> Option<(u32, u8, u8, [[f32; 2]; 3])> {
    let count = state_monitor_count();
    if count == 0 {
        return None;
    }
    let knobs_of = |boldness: u32, damping: u32| -> (u8, u8) {
        (boldness.min(30) as u8, damping.min(100) as u8)
    };
    let offsets_of = |rx: u32, ry: u32, gx: u32, gy: u32, bx: u32, by: u32| -> [[f32; 2]; 3] {
        [
            [f32::from_bits(rx), f32::from_bits(ry)],
            [f32::from_bits(gx), f32::from_bits(gy)],
            [f32::from_bits(bx), f32::from_bits(by)],
        ]
    };
    for i in 0..count {
        if let Some((_, _, _, _, mf, _lcd, _ct, bd, _gm, dm, _fw, srx, sry, sgx, sgy, sbx, sby)) =
            state_monitor_entry(i)
        {
            if (mf >> 8) & 1 != 0 {
                let (b, d) = knobs_of(bd, dm);
                return Some((mf & 0xFF, b, d, offsets_of(srx, sry, sgx, sgy, sbx, sby)));
            }
        }
    }
    // No primary bit set (stale table from an old build): use entry 0.
    state_monitor_entry(0).map(|(_, _, _, _, mf, _lcd, _ct, bd, _gm, dm, _fw, srx, sry, sgx, sgy, sbx, sby)| {
        let (b, d) = knobs_of(bd, dm);
        (mf & 0xFF, b, d, offsets_of(srx, sry, sgx, sgy, sbx, sby))
    })
}

fn report_state(status: u32) {
    let Some(addr) = STATE.get() else {
        return;
    };
    // Only the Explorer instance owning the taskbar may publish the single
    // shared status report. Secondary Explorer hosts must never make the app
    // lose verification when a Control Panel window opens.
    let primary_pid = unsafe {
        std::ptr::read_volatile((addr + STATE_PRIMARY_EXPLORER_PID) as *const u32)
    };
    if primary_pid == 0 || primary_pid != std::process::id() {
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

// Loaded-process counter at STATE_PROCESS_COUNT: incremented when a process
// enters the active hook scope, decremented when it leaves or detaches, so
// the app can show how many scoped processes currently carry the hook. The shared-memory word is a plain u32, but x86/x64
// interlocked ops make fetch_add/fetch_sub safe across processes. A crashed
// process skips DETACH, so the count can drift slightly upward; it resets
// whenever the mapping is recreated (app start).
fn bump_process_count(delta: i32) {
    let Some(addr) = STATE.get() else {
        return;
    };
    // SAFETY: STATE is written once in map_state; the mapped view stays valid
    // for the process lifetime, and the counter slot is a u32 at a fixed
    // offset we own. Only atomic ops touch it.
    unsafe {
        let cell = (addr + STATE_PROCESS_COUNT) as *const AtomicU32;
        if delta > 0 {
            (*cell).fetch_add(1, Ordering::SeqCst);
        } else {
            (*cell).fetch_sub(1, Ordering::SeqCst);
        }
    }
}

// Publish this process's executable name in one of the bounded shared-memory
// slots. The high bit marks a slot while its UTF-16 name is being written, so
// the backend only reads fully published names.
fn register_process_name() -> bool {
    if PROCESS_SLOT.load(Ordering::Acquire) != NO_PROCESS_SLOT
        || PROCESS_REGISTERING.swap(true, Ordering::AcqRel)
    {
        return false;
    }
    let Some(addr) = STATE.get() else {
        PROCESS_SLOT.store(NO_PROCESS_SLOT, Ordering::Release);
        PROCESS_REGISTERING.store(false, Ordering::Release);
        return false;
    };
    let pid = std::process::id();
    let process_name = process_base_name();
    if process_name.is_empty() {
        PROCESS_REGISTERING.store(false, Ordering::Release);
        return false;
    }
    let name: Vec<u16> = process_name
        .encode_utf16()
        .take(PROCESS_NAME_WORDS - 1)
        .collect();
    unsafe {
        for index in 0..MAX_PROCESS_SLOTS {
            let slot = addr + STATE_PROCESS_SLOTS + index * PROCESS_SLOT_STRIDE;
            let cell = slot as *const AtomicU32;
            if (*cell)
                .compare_exchange(
                    0,
                    pid | PROCESS_SLOT_WRITING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }
            let name_ptr = (slot + 4) as *mut u16;
            for i in 0..PROCESS_NAME_WORDS {
                std::ptr::write_volatile(name_ptr.add(i), name.get(i).copied().unwrap_or(0));
            }
            (*cell).store(pid, Ordering::Release);
            PROCESS_SLOT.store(index as u32, Ordering::Release);
            PROCESS_REGISTERING.store(false, Ordering::Release);
            return true;
        }
    }
    PROCESS_REGISTERING.store(false, Ordering::Release);
    false
}

fn unregister_process_name() -> bool {
    let Some(addr) = STATE.get() else {
        return false;
    };
    let index = PROCESS_SLOT.load(Ordering::Acquire);
    if index == NO_PROCESS_SLOT || PROCESS_REGISTERING.load(Ordering::Acquire) {
        return false;
    }
    let pid = std::process::id();
    unsafe {
        let slot = addr + STATE_PROCESS_SLOTS + index as usize * PROCESS_SLOT_STRIDE;
        let cell = slot as *const AtomicU32;
        if (*cell)
            .compare_exchange(
                pid,
                pid | PROCESS_SLOT_WRITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let name_ptr = (slot + 4) as *mut u16;
            for i in 0..PROCESS_NAME_WORDS {
                std::ptr::write_volatile(name_ptr.add(i), 0);
            }
            (*cell).store(0, Ordering::Release);
            PROCESS_SLOT.store(NO_PROCESS_SLOT, Ordering::Release);
            return true;
        }
    }
    false
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
// Panel host processes, the MMC admin consoles, and the per-monitor-aware
// classic Windows tools that draw real-DPI GDI text (Registry Editor,
// console windows, Volume Mixer). SystemProperties is a family of
// executables, matched by prefix. Third-party apps (7-Zip, ...) are
// intentionally NOT here, they're covered by the all-apps scope stop
// instead, so the shell stop stays a Windows-only list.
fn is_target() -> bool {
    let lower = proc_name().to_ascii_lowercase();
    lower == "explorer.exe"
        || lower == "control.exe"
        || lower == "rundll32.exe"
        || lower == "mmc.exe"
        || lower == "regedit.exe"
        || lower == "sndvol.exe"
        || lower == "conhost.exe"
        || (lower.starts_with("systemproperties") && lower.ends_with(".exe"))
}

// ── Hook scope (all apps vs shell) ────────────────────────────────────
// The app can widen the detour to every GUI process (default), since many
// GDI apps (7-Zip, WinRAR, ...) force ClearType per-font and ignore the
// registry grayscale setting. The scope flag lives in shared memory so it
// can change at runtime; the exe blocklist is the escape hatch for apps
// that misbehave or trip AV.

// Whether the app's "apply to all apps" scope flag is set. A single
// shared-memory word read, so it is safe to consult on every draw.
fn hook_all_enabled() -> bool {
    // SAFETY: STATE is written once in map_state; the mapped view stays valid
    // for the lifetime of the process. Unmapped (app not running) = shell-only.
    unsafe {
        STATE.get()
            .map(|addr| *((addr + STATE_SCOPE) as *const u32) & SCOPE_HOOK_ALL != 0)
            .unwrap_or(false)
    }
}

// Whether this process's exe name is on the app's blocklist. Compares against
// the cached process base name (PROC_NAME), so it only does real work once.
fn blocklisted() -> bool {
    let Some(base) = STATE.get() else {
        return false;
    };
    // SAFETY: the count is clamped to MAX_BLOCKLIST and every entry is 32
    // UTF-16 units within the mapping the app owns.
    unsafe {
        let count = (*((base + STATE_BLOCKLIST_COUNT) as *const u32) as usize).min(MAX_BLOCKLIST);
        if count == 0 {
            return false;
        }
        let name = proc_name().to_ascii_lowercase();
        for i in 0..count {
            let entry = base + STATE_BLOCKLIST_ENTRIES + i * BLOCKLIST_ENTRY_STRIDE;
            let words = std::slice::from_raw_parts(entry as *const u16, BLOCKLIST_ENTRY_WORDS);
            let mut len = 0;
            while len < words.len() && words[len] != 0 {
                len += 1;
            }
            if String::from_utf16_lossy(&words[..len]).to_ascii_lowercase() == name {
                return true;
            }
        }
    }
    false
}

// Full detour scope for a draw: shell/Control Panel hosts always, plus any
// other process while all-apps scope is on and it is not blocklisted. This
// is the per-draw guard, so both the toggle and the blocklist apply live.
fn in_hook_scope() -> bool {
    IS_TARGET.load(Ordering::Acquire) || (hook_all_enabled() && !blocklisted())
}

fn map_state() {
    unsafe {
        let name = wide(r"Local\OledHelperExplorerHookStateV3");
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

/// Create a 32bpp top-down DIB section of w×h and return it with its pixel
/// pointer, or None on any failure (caller frees nothing then).
unsafe fn create_dib_section(hdc: HDC, w: i32, h: i32) -> Option<(HBITMAP, *mut u8)> {
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
        Err(_) => return None,
    };
    if hbitmap.is_invalid() || bits.is_null() {
        if !hbitmap.is_invalid() {
            let _ = DeleteObject(hbitmap.into());
        }
        return None;
    }
    Some((hbitmap, bits as *mut u8))
}

// Render the rasterized run into the HDC at the text reference point,
// honoring the HDC's text alignment. Two paths:
//  - ETO_OPAQUE: the destination under the text is guaranteed to be the solid
//    background color, so the run is composited over GetBkColor into a plain
//    heap buffer and written with a single SetDIBitsToDevice, no DIB, no DC,
//    no AlphaBlend, and (for subpixel) no destination read-back.
//  - transparent: per-run DIB + AlphaBlend (Mask) / background read-back
//    blend (Subpixel).
unsafe fn blit_run(
    hdc: HDC,
    x: i32,
    y: i32,
    run: &text_raster::RasterizedText,
    color: (u8, u8, u8),
    opaque: bool,
) -> bool {
    let Some((min_left, min_top, w, h)) = run_bounds(run) else {
        return true; // empty run, nothing to draw
    };
    // A malformed glyph position must never produce a giant allocation.
    if w <= 0 || h <= 0 || w > 8192 || h > 8192 {
        return false;
    }

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

    if opaque {
        OPAQUE_DRAWS.fetch_add(1, Ordering::Relaxed);
        return blit_opaque(hdc, dest_x, dest_y, w, h, min_left, min_top, run, color);
    }

    // ── Transparent background: per-run DIB + AlphaBlend / read-back ──
    let Some((hbitmap, bits)) = create_dib_section(hdc, w, h) else {
        return false;
    };
    let memdc = CreateCompatibleDC(Some(hdc));
    if memdc.is_invalid() {
        let _ = DeleteObject(hbitmap.into());
        return false;
    }
    let _ = SelectObject(memdc, hbitmap.into());
    let buf = std::slice::from_raw_parts_mut(bits, (w * h * 4) as usize);

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
            // Zero first so a clipped or partial background read leaves black
            // rather than garbage pixels.
            buf.fill(0);
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

    // DC first so the bitmap is deselected before DeleteObject (which refuses
    // to delete a selected object).
    let _ = DeleteDC(memdc);
    let _ = DeleteObject(hbitmap.into());
    ok
}

// Opaque fast path (ETO_OPAQUE): the destination under the text is the solid
// background color, so composite over GetBkColor into a plain heap buffer and
// write it with one SetDIBitsToDevice call. The result is fully opaque -
// nothing to alpha-blend, and no DIB/DC objects to create or destroy. For
// subpixel this also skips the destination read-back entirely: the background
// is known, not read.
unsafe fn blit_opaque(
    hdc: HDC,
    dest_x: i32,
    dest_y: i32,
    w: i32,
    h: i32,
    min_left: i32,
    min_top: i32,
    run: &text_raster::RasterizedText,
    color: (u8, u8, u8),
) -> bool {
    // COLORREF is 0x00BBGGRR (same layout as the text color).
    let bk = unsafe { GetBkColor(hdc) }.0;
    let bk_r = (bk & 0xFF) as u8;
    let bk_g = ((bk >> 8) & 0xFF) as u8;
    let bk_b = ((bk >> 16) & 0xFF) as u8;

    let mut buf = vec![0u8; (w * h * 4) as usize];
    // Base layer = the solid background color, fully opaque, exactly what GDI
    // paints for an ETO_OPAQUE fill before the text.
    for px in buf.chunks_exact_mut(4) {
        px[0] = bk_b;
        px[1] = bk_g;
        px[2] = bk_r;
        px[3] = 255;
    }

    let ok = match run.content {
        // Grayscale/crisp: each coverage sample lerps the text color toward
        // the background color.
        Content::Mask => {
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
                        buf[i] = blend_channel(color.2, bk_b, c); // B
                        buf[i + 1] = blend_channel(color.1, bk_g, c); // G
                        buf[i + 2] = blend_channel(color.0, bk_r, c); // R
                        buf[i + 3] = 255;
                    }
                }
            }
            true
        }
        // Subpixel: blend each color channel against its own coverage over the
        // solid background, no destination read-back needed.
        Content::SubpixelMask => {
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
                        buf[i] = blend_channel(color.2, bk_b, bc); // B
                        buf[i + 1] = blend_channel(color.1, bk_g, gc); // G
                        buf[i + 2] = blend_channel(color.0, bk_r, rc); // R
                        buf[i + 3] = 255;
                    }
                }
            }
            true
        }
        Content::Color => false,
    };
    if !ok {
        return false;
    }

    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    bmi.bmiHeader.biHeight = -h; // negative = top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;
    // Returns the number of scanlines written; 0 (or fewer than h) on failure.
    SetDIBitsToDevice(
        hdc,
        dest_x,
        dest_y,
        w as u32,
        h as u32,
        0,
        0,
        0,
        h as u32,
        buf.as_ptr() as *const core::ffi::c_void,
        &bmi,
        DIB_RGB_COLORS,
    ) == h
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
    // DPI-virtualized surfaces can't carry swash at native quality (see
    // hdc_dpi_virtualized): let GDI draw those so the text stays sharp.
    if hdc_dpi_virtualized(hdc) {
        return false;
    }
    let global_mode = state_mode();
    // Per-monitor render options: when enabled, attribute this draw's DC to
    // a monitor and use its mode + knob triplet. Unattributable draws (or
    // per-monitor off) fall back to the global mode with NO knob adjustments
    // (knobs are purely per-monitor now) — except that when the DC cannot be
    // attributed (Explorer's bitmap-carrying memory DCs), the PRIMARY
    // monitor's entry stands in so knobs like faux boldness still apply
    // instead of silently zeroing out.
    let fallback_opts = text_raster::RenderOptions {
        boldness: 0,
        damping: 0,
        subpixel_offsets: None,
    };
    let (mode, opts) = if state_per_monitor_enabled() && global_mode != 0 {
        match resolve_draw_options(hdc).or_else(resolve_primary_options) {
            Some((m, b, d, offs)) => {
                // Mode 0 in the table = "no explicit mode" (Default): fall
                // back to the global mode (the system effective mode, synced
                // into slot 0 on load) so the monitor still renders the
                // system text setting, but keep ITS knob triplet so knobs
                // apply even on a Default monitor. Modes 5/6 (the fixed
                // RGWB/RWBG WOLED presets) were removed from the product;
                // a stale entry with one routes the same way. Mode 7
                // (Custom) carries the user's subpixel centers.
                let mode = if m == 0 || m == 5 || m == 6 { global_mode } else { m };
                // Custom mode with all-zero centers means the user has not
                // tuned offsets yet, render the mode's default layout
                // (render_format falls back to the WOLED quad positions)
                // rather than sampling all three channels at the same point.
                let all_zero = offs.iter().all(|&[x, y]| x == 0.0 && y == 0.0);
                let subpixel_offsets = if mode == 7 && !all_zero {
                    Some(offs)
                } else {
                    None
                };
                (
                    mode,
                    text_raster::RenderOptions {
                        boldness: b,
                        damping: d,
                        subpixel_offsets,
                    },
                )
            }
            None => (global_mode, fallback_opts),
        }
    } else {
        (global_mode, fallback_opts)
    };
    // Knobs are mode-scoped: a mode that can't use a knob must not inherit
    // it from another mode's settings. Crisp ("No filter") applies NO knobs
    // at all, boldness included, so a boldness tuned in grayscale does not
    // show up on a No-filter monitor. Grayscale applies only boldness
    // (fringe damping is subpixel-only; the rasterizer already ignores it
    // for mask content, this keeps it explicit). RGB/BGR/Custom use the
    // full knob pair.
    let opts = match mode {
        2 => text_raster::RenderOptions {
            boldness: 0,
            damping: 0,
            subpixel_offsets: None,
        },
        1 => text_raster::RenderOptions {
            boldness: opts.boldness,
            damping: 0,
            subpixel_offsets: None,
        },
        _ => opts,
    };
    let render_mode = match mode {
        1 => text_raster::RenderMode::Grayscale,
        2 => text_raster::RenderMode::Crisp,
        3 => text_raster::RenderMode::SubpixelRgb,
        4 => text_raster::RenderMode::SubpixelBgr,
        7 => text_raster::RenderMode::Custom,
        // Modes 5/6 (the fixed RGWB/RWBG WOLED presets) were removed; a stale
        // table entry with one of them is a legacy id and must not reach here
        // (resolve_draw_options routes them to the global mode like Default).
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
        return try_render_glyph_indices(hdc, x, y, options, string, count, dx, render_mode, opts);
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
    let Some(run) = text_raster::rasterize(&font_data, 0, px, &text, render_mode, opts) else {
        return fallback("rasterize failed");
    };

    if verbose {
        let (hits, misses, entries) = text_raster::glyph_cache_stats();
        log(&format!(
            "render #{n}: px={px} glyphs={} -> blit (cache hits={hits} misses={misses} entries={entries} | opaque={} mon w={} c={} n={})",
            run.glyphs.len(),
            OPAQUE_DRAWS.load(Ordering::Relaxed),
            MON_WINDOW.load(Ordering::Relaxed),
            MON_CAPS.load(Ordering::Relaxed),
            MON_NONE.load(Ordering::Relaxed)
        ));
    }
    let color = text_color_rgb(hdc);
    blit_run(hdc, x, y, &run, color, options.0 & ETO_OPAQUE.0 != 0)
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
    options: ETO_OPTIONS,
    string: PCWSTR,
    count: u32,
    dx: *const i32,
    mode: text_raster::RenderMode,
    opts: text_raster::RenderOptions,
) -> bool {
    // Same DPI-virtualization passthrough as try_render_swash: mmc.exe and
    // other system-DPI-aware hosts render into a 96-DPI surface that Windows
    // stretches, so swash there would be upscaled and read as low-res.
    if hdc_dpi_virtualized(hdc) {
        return false;
    }
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
        let (hits, misses, entries) = text_raster::glyph_cache_stats();
        log(&format!(
            "render #{n}: glyph-index px={px} glyphs={} -> blit (cache hits={hits} misses={misses} entries={entries} | opaque={} mon w={} c={} n={})",
            run.glyphs.len(),
            OPAQUE_DRAWS.load(Ordering::Relaxed),
            MON_WINDOW.load(Ordering::Relaxed),
            MON_CAPS.load(Ordering::Relaxed),
            MON_NONE.load(Ordering::Relaxed)
        ));
    }
    let color = text_color_rgb(hdc);
    blit_run(hdc, x, y, &run, color, options.0 & ETO_OPAQUE.0 != 0)
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
    // Scope guard: shell/Control Panel hosts always render through the hook;
    // other processes only while all-apps scope is on AND not blocklisted.
    // Re-evaluated per draw so the toggle and blocklist apply live to
    // already-loaded processes; the detour stays installed but passes
    // through while out of scope.
    if !in_hook_scope() {
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
    // created), that would deadlock the whole shell.
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
    // Re-evaluate scope on every callback (cheap: the flag is a shared-memory
    // word and the blocklist compare uses the cached process name), so a
    // process that was already running when the app flipped "all apps" on
    // gets the detour without a restart. INSTALL_ATTEMPTED stays false for
    // out-of-scope processes so a later scope change still installs; it only
    // latches once the install is attempted (or succeeded).
    // Process-name lookup is deferred here instead of DllMain, keeping the
    // loader lock free of filesystem and allocation work.
    if in_hook_scope() {
        if PROCESS_SLOT.load(Ordering::Acquire) == NO_PROCESS_SLOT
            && register_process_name()
        {
            bump_process_count(1);
        }
    } else if PROCESS_SLOT.load(Ordering::Acquire) != NO_PROCESS_SLOT
        && unregister_process_name()
    {
        bump_process_count(-1);
    }
    if in_hook_scope() && !INSTALL_ATTEMPTED.swap(true, Ordering::SeqCst) {
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
            // Map the state FIRST so the all-apps scope flag is readable.
            // OpenFileMappingW fails fast when the app is not running,
            // leaving STATE unset (scope falls back to shell-only). Only the
            // shell/Control Panel decision is cached; the all-apps scope is
            // re-evaluated per draw in in_hook_scope.
            map_state();
            IS_TARGET.store(is_target(), Ordering::Release);
            // Register immediately for in-scope processes (shell hosts like
            // control.exe and mmc.exe may never receive a hook callback), so
            // the name list reflects every process the detour applies to.
            // The callback path re-registers if the mapping was not reachable
            // at attach time.
            if in_hook_scope() && register_process_name() {
                bump_process_count(1);
            }
            BOOL(1)
        }
        DLL_PROCESS_DETACH => {
            // Disable whenever the detour was actually installed, a process
            // hooked via all-apps scope has IS_TARGET false but still carries
            // the patch, and leaving it in place as the DLL unloads would
            // jump into unmapped memory on the next ExtTextOutW call.
            if DETOUR.get().is_some() {
                disable_detours();
            }
            // Matches the attach increment. Processes that never published a
            // name were not included in the counter.
            if unregister_process_name() {
                bump_process_count(-1);
            }
            BOOL(1)
        }
        DLL_THREAD_ATTACH | DLL_THREAD_DETACH => BOOL(1),
        _ => BOOL(1),
    }
}
