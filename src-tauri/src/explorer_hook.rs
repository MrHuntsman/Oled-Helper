// explorer_hook.rs — load explorer_hook.dll into the shell and the classic
// Control Panel host processes via a single global WH_CBT hook, and control
// their font-antialiasing override. The Windows shell (and several system
// consoles) hard-code ClearType and ignore the per-user font-smoothing
// registry values, so the only way to change their text is to load a DLL that
// hooks ExtTextOutW and re-rasterizes each draw with swash (see explorer-hook/).
// The DLL is loaded through a
// global SetWindowsHookExW WH_CBT hook — the documented OS mechanism — instead
// of CreateRemoteThread injection, so Windows Defender does not flag the app
// the way it did the injection path. WH_CBT fires on window creation, before
// fonts are rasterized, so the fix reaches fast-starting applets (Device
// Manager, classic Control Panel, etc.) that a per-process watchdog would
// miss. The DLL reads a session-local shared-memory mode that this module
// writes: 0 = pass through, 1 = force grayscale, 2 = force crisp, 3 = subpixel
// RGB, 4 = subpixel BGR, 7 = custom subpixel offsets (5/6 were the fixed
// WOLED RGWB/RWBG presets, removed — the Custom mode covers those layouts).
// Because the hook sits on the draw path, a mode change applies on the next
// repaint (live).

use crate::monitors::all_monitors;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tauri::Manager;
use windows::core::{s, w, BOOL, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, FreeLibrary, HINSTANCE, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM, HWND,
};
use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetClassNameW, GetWindowThreadProcessId, PostMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, HWND_BROADCAST, WH_CBT, WM_FONTCHANGE,
    WM_SETTINGCHANGE,
};

type HookProcFn = unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT;

// The mapped view of the shared state (created once, leaked for app
// lifetime). Layout (608 bytes):
//    0  u32 global mode (app->explorer; the system effective mode, synced
//       on Text-tab load)
//    4  u32 status + 8 u32 pid (explorer->app, written by the DLL at attach
//       so the app can VERIFY the hook is really installed in the current
//       explorer.exe instead of trusting its own bookkeeping — the DLL
//       loading only proves the module mapped, not that the detour
//       installed)
//   12  unused padding (old global knob slots, kept at 0)
//   24  u32 per-monitor flag (0/1)
//   28  u32 monitor count
//   32  monitor table: up to 8 entries x 72 bytes
//       u32 hmonitor (system-wide monitor handle)
//       u32 resolution (horzres | vertres<<16)
//       u32 physical size (horzsize | vertsize<<16)
//       u32 dpi (logpixelsx | logpixelsy<<16)
//       u32 mode | primary<<8  (mode 0 = system default, 7 = custom)
//       u32 lcd filter (reserved at 0 — removed filter), boldness,
//           damping (all 0..=100; the crosstalk / gamma / filter-width
//           slots likewise reserved at 0 — removed filters)
//       u32 custom subpixel centers r/g/b x/y (f32 bits; all-zero = not set)
// After the table, the hook-scope blocklist (count + up to 8 exe names x 32
// UTF-16 units); offset 12 holds the hook-scope flags (bit 0 = all apps).
// The DLL reads the table on every draw to pick the mode + knob sextet +
// custom offsets for the monitor the DC belongs to (see
// explorer-hook/src/lib.rs, resolve_draw_options).
static STATE_VIEW: OnceLock<usize> = OnceLock::new();
const STATE_STATUS_OFFSET: usize = 4;
const STATE_PID_OFFSET: usize = 8;
const STATE_SCOPE_OFFSET: usize = 12;
const STATE_PER_MONITOR_OFFSET: usize = 24;
const STATE_MONITOR_COUNT_OFFSET: usize = 28;
const STATE_MONITORS_OFFSET: usize = 32;
const MAX_MONITORS: usize = 8;
const MONITOR_ENTRY_STRIDE: usize = 72;
const STATE_BLOCKLIST_COUNT_OFFSET: usize = 608;
const STATE_BLOCKLIST_ENTRIES_OFFSET: usize = 612;
const BLOCKLIST_ENTRY_WORDS: usize = 32;
const BLOCKLIST_ENTRY_STRIDE: usize = BLOCKLIST_ENTRY_WORDS * 2;
const MAX_BLOCKLIST: usize = 8;
const STATE_SIZE: usize = STATE_BLOCKLIST_ENTRIES_OFFSET + MAX_BLOCKLIST * BLOCKLIST_ENTRY_STRIDE;
const SCOPE_HOOK_ALL: u32 = 1;
// The ExtTextOutW detour is what makes the fix real; the app requires this
// status bit for a hook to count as active.
const STATUS_HOOK_W_INSTALLED: u32 = 1;
// The single global WH_CBT hook handle (0 = not installed yet). A global hook
// (thread id 0) makes Windows map the DLL into every GUI process on the
// desktop, including ones that start later, so no per-process discovery or
// re-install is needed — the DLL's own is_target() gate limits the detour to
// the shell/Control Panel hosts.
static GLOBAL_HOOK: OnceLock<Mutex<Option<usize>>> = OnceLock::new();
// Last explorer pid whose hook report verified, so a pid change (Explorer
// restart) can flip the applied flag.
static VERIFIED_PID: AtomicU32 = AtomicU32::new(0);
// Whether the feature is active (Grayscale/Off vs RGB/BGR).
static ENABLED: AtomicBool = AtomicBool::new(false);
// Master toggle from the Text tab (persisted in config): off means Explorer
// is never touched and the DLL is not loaded. Defaults to off; the
// frontend syncs the stored value on load.
static TOGGLE: AtomicBool = AtomicBool::new(false);
// Last mode-state from the Text tab (0..7), gated by TOGGLE in apply_state.
static LAST_STATE: AtomicU32 = AtomicU32::new(0);
// Per-monitor rendering (Text tab): master flag plus per-monitor mode
// assignments and knob pairs keyed by the monitor's stable signature
// (see MonitorEntry::sig). Mode values match the DLL: 1..=7 as in
// explorer_hook_set_mode; absent = "system default". Knobs are the
// boldness / damping pairs (0..=100); absent = the defaults (no bold,
// no damping). The LCD filter knob was removed — its shared-memory slot
// stays reserved at 0.
static PER_MONITOR_ENABLED: AtomicBool = AtomicBool::new(false);
static MONITOR_MODES: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
static MONITOR_KNOBS: OnceLock<Mutex<HashMap<String, (u32, u32)>>> = OnceLock::new();
// Custom subpixel centers per monitor (mode 7): sig → [[r, g, b] x (x, y)]
// as f32 fractions of a pixel. All-zero pairs are treated as unset (the
// entry writes 0, and the DLL only uses them when the mode is 7).
static MONITOR_CUSTOM: OnceLock<Mutex<HashMap<String, [[f32; 2]; 3]>>> = OnceLock::new();
// Text hook scope (Text tab): whether the detour applies to every GUI
// process ("all apps", the default — many GDI apps force ClearType per-font
// and ignore the registry setting) or only the shell/Control Panel hosts,
// plus the exe-name blocklist as the escape hatch. Pushed into shared
// memory so the DLL's attach/callback decisions and per-draw scope guard
// see them.
static HOOK_ALL_APPS: AtomicBool = AtomicBool::new(true);
static BLOCKLIST: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
// True once the hook is verified in a NEW explorer.exe generation while the
// feature was already active, so the Text tab can show "✓ Fix applied". Fonts
// are rasterized at window creation, so a mode change only fully applies after
// Explorer restarts (or to newly opened windows). Any mode/toggle change
// resets it.
static RESTARTED_WHILE_ACTIVE: AtomicBool = AtomicBool::new(false);
// Watchdog lifecycle: started once, stopped on app shutdown.
static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
// Serializes hook install so the watchdog and the command handler don't race.
static HOOK_LOCK: Mutex<()> = Mutex::new(());

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ensure_state_mapped() -> Result<*mut u32, String> {
    if let Some(addr) = STATE_VIEW.get() {
        return Ok(*addr as *mut u32);
    }
    // SAFETY: the mapping and its view are created once and intentionally
    // leaked so they stay valid for the process lifetime (explorer.exe must be
    // able to open the mapping by name while the app is running).
    unsafe {
        let name = wide(r"Local\OledHelperExplorerHookState");
        let mapping = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            STATE_SIZE as u32,
            PCWSTR(name.as_ptr()),
        )
        .map_err(|e| format!("CreateFileMappingW failed: {e}"))?;
        let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, STATE_SIZE);
        if view.Value.is_null() {
            let _ = CloseHandle(mapping);
            return Err("MapViewOfFile failed".to_string());
        }
        // Intentionally do NOT close the mapping handle: HANDLE has no Drop,
        // so leaving it open keeps the mapping alive for explorer.exe to open
        // by name. The mapped view is likewise never unmapped.
        let ptr = view.Value as *mut u32;
        let _ = STATE_VIEW.set(view.Value as usize);
        Ok(ptr)
    }
}

fn write_state(mode: u32) -> Result<(), String> {
    let ptr = ensure_state_mapped()?;
    // SAFETY: ptr is the mapped view, valid for the process lifetime.
    unsafe {
        *ptr = mode;
    }
    Ok(())
}

// ── Per-monitor mode table ────────────────────────────────────────────
// Each connected monitor gets one entry in the shared table: its system-wide
// HMONITOR handle (for window-DC attribution) plus a device-caps signature
// (resolution + physical size + DPI, for memory-DC attribution) and the
// assigned mode. The DLL matches a draw's DC against these on every draw.
// The monitor list comes from the shared monitors module (all_monitors),
// which also supplies the CCD friendly names shown in the UI.

// Write the per-monitor flag + table into the shared memory so the DLL can
// resolve a draw's monitor on every repaint. Called on every per-monitor
// change and on apply_state.
fn write_state_table() -> Result<(), String> {
    let ptr = ensure_state_mapped()?;
    let entries = all_monitors();
    let modes = MONITOR_MODES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let knobs = MONITOR_KNOBS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let custom = MONITOR_CUSTOM
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    // SAFETY: ptr is the mapped view, valid for the process lifetime; the
    // table region (offsets 24..608) is plain u32 slots we own.
    unsafe {
        let base = ptr as *mut u8;
        std::ptr::write_volatile(
            base.add(STATE_PER_MONITOR_OFFSET) as *mut u32,
            if PER_MONITOR_ENABLED.load(Ordering::Acquire) {
                1
            } else {
                0
            },
        );
        let count = entries.len().min(MAX_MONITORS) as u32;
        std::ptr::write_volatile(base.add(STATE_MONITOR_COUNT_OFFSET) as *mut u32, count);
        for (i, e) in entries.iter().take(MAX_MONITORS).enumerate() {
            let entry = base.add(STATE_MONITORS_OFFSET + i * MONITOR_ENTRY_STRIDE);
            let (bd, dm) = knobs.get(&e.sig()).copied().unwrap_or((0, 0));
            let [[srx, sry], [sgx, sgy], [sbx, sby]] = custom
                .get(&e.sig())
                .copied()
                .unwrap_or([[0.0; 2]; 3]);
            std::ptr::write_volatile(entry as *mut u32, e.hmonitor);
            std::ptr::write_volatile(
                entry.add(4) as *mut u32,
                e.width | (e.height << 16),
            );
            std::ptr::write_volatile(
                entry.add(8) as *mut u32,
                e.phys_width | (e.phys_height << 16),
            );
            std::ptr::write_volatile(
                entry.add(12) as *mut u32,
                e.dpi_x | (e.dpi_y << 16),
            );
            std::ptr::write_volatile(
                entry.add(16) as *mut u32,
                modes.get(&e.sig()).copied().unwrap_or(0).min(7) | ((e.primary as u32) << 8),
            );
            // Slot 20 (LCD filter) is reserved at 0 — the filter was removed.
            std::ptr::write_volatile(entry.add(20) as *mut u32, 0);
            // Slots 24 (crosstalk), 32 (gamma), and 40 (filter width) are
            // likewise reserved: the filters were removed, but the slots stay
            // at 0 so the shared-memory layout (and HOOK_LAYOUT_MARKER) is
            // unchanged.
            std::ptr::write_volatile(entry.add(24) as *mut u32, 0);
            std::ptr::write_volatile(entry.add(28) as *mut u32, bd.min(30));
            std::ptr::write_volatile(entry.add(32) as *mut u32, 0);
            std::ptr::write_volatile(entry.add(36) as *mut u32, dm.min(100));
            std::ptr::write_volatile(entry.add(40) as *mut u32, 0);
            std::ptr::write_volatile(entry.add(44) as *mut u32, srx.to_bits());
            std::ptr::write_volatile(entry.add(48) as *mut u32, sry.to_bits());
            std::ptr::write_volatile(entry.add(52) as *mut u32, sgx.to_bits());
            std::ptr::write_volatile(entry.add(56) as *mut u32, sgy.to_bits());
            std::ptr::write_volatile(entry.add(60) as *mut u32, sbx.to_bits());
            std::ptr::write_volatile(entry.add(64) as *mut u32, sby.to_bits());
        }
    }
    write_scope()?;
    Ok(())
}

// Normalize a blocklist exe name: trim, lowercase, and append ".exe" when
// the user typed a bare name (e.g. "notepad" → "notepad.exe").
fn normalize_exe(name: &str) -> String {
    let mut n = name.trim().to_ascii_lowercase();
    if !n.is_empty() && !n.contains('.') {
        n.push_str(".exe");
    }
    n
}

// Write the hook-scope flag (offset 12) and the blocklist (count + up to 8
// exe names as 32 UTF-16 units each) into the shared memory so the DLL's
// attach decision and per-draw scope guard see the current values. The
// region is zeroed first so a shorter previous list cannot leave stale
// entries that still match.
fn write_scope() -> Result<(), String> {
    let ptr = ensure_state_mapped()?;
    let list = BLOCKLIST
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let entries: Vec<String> = list
        .iter()
        .filter(|s| !s.is_empty())
        .take(MAX_BLOCKLIST)
        .cloned()
        .collect();
    // SAFETY: ptr is the mapped view, valid for the process lifetime; the
    // scope + blocklist region (offsets 12 and 608..1124) is ours.
    unsafe {
        let base = ptr as *mut u8;
        std::ptr::write_volatile(
            base.add(STATE_SCOPE_OFFSET) as *mut u32,
            if HOOK_ALL_APPS.load(Ordering::Acquire) {
                SCOPE_HOOK_ALL
            } else {
                0
            },
        );
        std::ptr::write_volatile(
            base.add(STATE_BLOCKLIST_COUNT_OFFSET) as *mut u32,
            entries.len() as u32,
        );
        std::ptr::write_bytes(
            base.add(STATE_BLOCKLIST_ENTRIES_OFFSET),
            0,
            MAX_BLOCKLIST * BLOCKLIST_ENTRY_STRIDE,
        );
        for (i, exe) in entries.iter().enumerate() {
            let entry = base.add(STATE_BLOCKLIST_ENTRIES_OFFSET + i * BLOCKLIST_ENTRY_STRIDE)
                as *mut u16;
            for (j, unit) in exe.encode_utf16().take(BLOCKLIST_ENTRY_WORDS - 1).enumerate() {
                *entry.add(j) = unit;
            }
        }
    }
    Ok(())
}

// Read the DLL's own report (pid + status) back from the shared memory. The
// pid lets us tie the report to the CURRENT explorer.exe generation: a stale
// report from a previous generation is ignored by pid mismatch. Returns
// (0, 0) when nothing is mapped yet.
fn read_hook_report() -> (u32, u32) {
    let Ok(ptr) = ensure_state_mapped() else {
        return (0, 0);
    };
    // SAFETY: ptr is the mapped view, valid for the process lifetime; both
    // fields are plain u32s at fixed offsets we own.
    unsafe {
        let base = ptr as *const u8;
        let status = std::ptr::read_volatile(base.add(STATE_STATUS_OFFSET) as *const u32);
        let pid = std::ptr::read_volatile(base.add(STATE_PID_OFFSET) as *const u32);
        (pid, status)
    }
}

// The main Explorer shell: its taskbar window gives us the pid used for status
// verification. Explorer is the primary target whose report we trust, since
// the shared memory has a single report slot that only Explorer writes.
fn explorer_target() -> Option<(u32, u32)> {
    // SAFETY: FindWindowW/GetWindowThreadProcessId are well-formed calls.
    unsafe {
        let hwnd = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).ok()?;
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if tid == 0 {
            return None;
        }
        Some((pid, tid))
    }
}

// Force Explorer's text-bearing windows to repaint after a mode change so the
// hook's new mode shows immediately instead of waiting for a natural repaint
// (hover, scroll, resize). The hook reads the mode on every draw, so this only
// has to make Explorer draw again. EnumWindows/RedrawWindow are safe from any
// thread; RDW_INVALIDATE (no RDW_UPDATENOW) only queues a repaint.
fn invalidate_explorer_windows() {
    let Some((pid, _)) = explorer_target() else {
        return;
    };
    unsafe {
        let _ = EnumWindows(Some(invalidate_proc), LPARAM(pid as isize));
    }
}

unsafe extern "system" fn invalidate_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let target_pid = lparam.0 as u32;
    let mut pid = 0u32;
    if GetWindowThreadProcessId(hwnd, Some(&mut pid)) == 0 || pid != target_pid {
        return BOOL(1);
    }
    let mut class = [0u16; 64];
    let len = GetClassNameW(hwnd, &mut class);
    if len <= 0 {
        return BOOL(1);
    }
    let class = String::from_utf16_lossy(&class[..len as usize]);
    if class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd" || class == "CabinetWClass" {
        let _ = RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_ALLCHILDREN);
    }
    BOOL(1)
}

// Notify every top-level window that a text-rendering setting changed, so the
// hooked hosts that don't get a targeted invalidate (classic Control Panel,
// MMC consoles, Device Manager, rundll32 applets) repaint their text. Posted
// asynchronously so a mode click never blocks on other apps' message loops.
fn broadcast_rendering_change() {
    unsafe {
        let msg = wide("Windows");
        let _ = PostMessageW(
            Some(HWND_BROADCAST),
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(msg.as_ptr() as isize),
        );
        let _ = PostMessageW(Some(HWND_BROADCAST), WM_FONTCHANGE, WPARAM(0), LPARAM(0));
    }
}

fn global_hook() -> &'static Mutex<Option<usize>> {
    GLOBAL_HOOK.get_or_init(|| Mutex::new(None))
}

// The hook DLL is compiled into the exe as a byte blob by build.rs (see
// build_explorer_hook in build.rs), so the app stays a single portable exe. At
// runtime the bytes are written to the config dir and that on-disk path is
// what both the app's LoadLibraryW and the hook loader in explorer.exe open.
static EMBEDDED_HOOK_DLL: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/explorer_hook.dll"));

// Layout-generation marker. MUST match explorer-hook's HOOK_LAYOUT_MARKER
// (explorer-hook/src/lib.rs); bump both whenever STATE_SIZE / the
// shared-memory layout changes. With per-build DLL names the app only ever
// loads its own build, so this is no longer a serve gate — it is a
// diagnostic: a stale copy on disk that lacks the marker means a running
// process holds a build predating the current layout and will keep
// misreading the shared memory until it restarts.
const HOOK_LAYOUT_MARKER: &[u8] = b"OH2-SMEM-1124";

// Hook DLL naming: one file per build, named by an FNV-1a hash of the
// embedded bytes (explorer_hook-<hash>.dll). A new build is always a NEW
// file name, so it can never collide with a copy still mapped by running
// processes (os error 32) — deployment never needs a free slot or a reboot,
// and the app never has to serve a previous build. Stale copies are deleted
// as soon as their last holder exits; locked ones are scheduled for deletion
// at the next boot, so the directory converges to the current build. This
// replaces the retired two-slot scheme, whose fixed names deadlocked once
// two different builds were held by different processes. (The Windows limit
// on distinct global-hook DLL names — historically 32 — only counts builds
// still alive in a boot session; release users accumulate one per update,
// so it is not reachable in practice.)
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

static HOOK_DLL_HASH: OnceLock<u64> = OnceLock::new();

fn hook_dll_name() -> String {
    let hash = *HOOK_DLL_HASH.get_or_init(|| fnv1a64(EMBEDDED_HOOK_DLL));
    format!("explorer_hook-{hash:016x}.dll")
}

fn dll_matches_current(path: &std::path::Path) -> bool {
    matches!(fs::read(path), Ok(bytes) if bytes == EMBEDDED_HOOK_DLL)
}

// Whether an on-disk DLL was built against the current shared-memory
// layout, by searching for the layout marker in its bytes. Diagnostic only:
// a locked copy that predates the current layout means a running process
// keeps the old build until it restarts.
fn dll_has_current_layout(path: &std::path::Path) -> bool {
    matches!(
        fs::read(path),
        Ok(bytes)
            if bytes
                .windows(HOOK_LAYOUT_MARKER.len())
                .any(|w| w == HOOK_LAYOUT_MARKER)
    )
}

// Delete every stale hook DLL in the config dir — previous hash-named
// builds and the retired fixed slots (explorer_hook.dll / explorer_hook_b.dll)
// — except the build in use. A file still mapped by a running process cannot
// be deleted; those are scheduled for deletion at the next boot, so the
// directory converges to the current build without ever blocking a deploy.
fn cleanup_hook_dlls(dir: &std::path::Path, keep: Option<&std::path::Path>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let fname = entry.file_name();
            let Some(name) = fname.to_str() else {
                continue;
            };
            if !(name.starts_with("explorer_hook") && name.ends_with(".dll")) {
                continue;
            }
            if keep == Some(path.as_path()) {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                continue;
            }
            // Still mapped by a running process. Gone at the next boot; warn
            // when the held build predates the current shared-memory layout
            // (that build only fully applies after the process restarts).
            if !dll_has_current_layout(&path) {
                log::warn!(
                    "[explorer-hook] a running process holds {} (previous shared-memory layout); it will pick up the new build after restarting",
                    path.display()
                );
            }
            schedule_delete_at_boot(&path);
        }
    }
}

// Mark a locked DLL for deletion at the next boot. Best effort: the pending
// file-operation list lives under HKLM, so this can fail without elevation.
fn schedule_delete_at_boot(path: &std::path::Path) {
    let w = wide(&path.to_string_lossy());
    // SAFETY: MoveFileExW with MOVEFILE_DELAY_UNTIL_REBOOT only records the
    // path in the session-manager list; no file is touched now.
    unsafe {
        let _ = MoveFileExW(PCWSTR(w.as_ptr()), None, MOVEFILE_DELAY_UNTIL_REBOOT);
    }
}

// Get the on-disk hook DLL for this build, writing the embedded copy under
// its build-unique name if needed. The name derives from the bytes, so the
// write can never hit a file a running process has mapped — Explorer and
// injected apps keep the previous build mapped for the whole session, but
// that is handled by cleanup_hook_dlls: old files die when their last
// holder exits or at the next boot, and newly started processes always load
// the current build.
fn ensure_hook_dll(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("could not resolve config dir: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("could not create config dir: {e}"))?;

    let path = dir.join(hook_dll_name());
    if !dll_matches_current(&path) {
        fs::write(&path, EMBEDDED_HOOK_DLL)
            .map_err(|e| format!("could not write the explorer hook DLL: {e}"))?;
    }
    cleanup_hook_dlls(&dir, Some(&path));
    Ok(path)
}

// Load the DLL into this process (so SetWindowsHookExW can resolve its hook
// procedure), then install a single GLOBAL WH_CBT hook (thread id 0). Windows
// itself maps the DLL into every GUI process on the desktop — both currently
// running and any that start later — which is why this does not trip the
// injection signatures that Windows Defender flags and why no per-process
// watchdog discovery is needed.
fn install_hook(app: &tauri::AppHandle) -> Result<(), String> {
    let dll = ensure_hook_dll(app)?;
    let wide_path = wide(&dll.to_string_lossy());
    // SAFETY: LoadLibraryW on the full path of our own embedded DLL. The
    // module is kept loaded for the process lifetime (same leak-for-lifetime
    // pattern as the shared-memory mapping) so the hook's module reference
    // stays valid.
    let module = unsafe { LoadLibraryW(PCWSTR(wide_path.as_ptr())) }
        .map_err(|e| format!("could not load the explorer hook DLL: {e}"))?;

    let proc: HookProcFn = unsafe {
        let Some(addr) = GetProcAddress(module, s!("hook_callback")) else {
            let _ = FreeLibrary(module);
            return Err("hook_callback export not found in the explorer hook DLL".to_string());
        };
        std::mem::transmute(addr)
    };

    // SAFETY: installs a global WH_CBT hook. Thread id 0 associates it with
    // all threads on the desktop; because the hook procedure lives in the DLL,
    // Windows maps the DLL into each GUI process for us. WH_CBT fires on
    // window creation, before fonts rasterize, so fast-starting applets get
    // the detour in time.
    let hook = match unsafe { SetWindowsHookExW(WH_CBT, Some(proc), Some(HINSTANCE(module.0)), 0) } {
        Ok(hook) => hook,
        Err(error) => {
            // The hook did not take ownership of this module reference. Free
            // it so the watchdog's one-second retry loop cannot accumulate
            // LoadLibrary references while Windows rejects the hook.
            unsafe { let _ = FreeLibrary(module); }
            return Err(format!("SetWindowsHookExW failed: {error}"));
        }
    };

    let mut slot = global_hook().lock().unwrap_or_else(|p| p.into_inner());
    *slot = Some(hook.0 as usize);
    log::info!("[explorer-hook] installed global WH_CBT hook");
    Ok(())
}

fn uninstall_all_hooks() {
    let Some(cell) = GLOBAL_HOOK.get() else {
        return;
    };
    let mut slot = cell.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(hook) = slot.take() {
        // SAFETY: each value is a live HHOOK we installed and never otherwise
        // free.
        let _ = unsafe { UnhookWindowsHookEx(HHOOK(hook as *mut core::ffi::c_void)) };
        log::info!("[explorer-hook] removed global WH_CBT hook");
    }
}

// Recompute the effective state from the master toggle and the last mode,
// write it to the shared memory, and keep the hook/watchdog running only while
// the feature is genuinely active.
fn apply_state(app: &tauri::AppHandle) -> Result<(), String> {
    let last = LAST_STATE.load(Ordering::Acquire);
    let enabled = TOGGLE.load(Ordering::Acquire) && last != 0;
    ENABLED.store(enabled, Ordering::Release);
    // When disabled, write passthrough so an already-loaded Explorer stops
    // rewriting fonts immediately (no restart needed for that). Keep the
    // monitor table in sync regardless, so the DLL never reads a stale value.
    write_state(if enabled { last } else { 0 })?;
    write_state_table()?;
    if enabled {
        ensure_watchdog_running(app.clone());
        ensure_hooked(app)?;
    } else {
        uninstall_all_hooks();
    }
    // Repaint Explorer's open windows so the new mode (or the return to
    // ClearType when disabled) is visible immediately, and broadcast a
    // settings change so the other hooked hosts repaint too.
    invalidate_explorer_windows();
    broadcast_rendering_change();
    Ok(())
}

// Map a mode string to the hook's mode id (0 = system default / passthrough).
// The fixed WOLED RGWB/RWBG presets (ids 5/6) were removed — the Custom mode
// covers those layouts with user-tuned offsets — so a stale string maps to 0.
fn mode_id(mode: &str) -> u32 {
    match mode {
        "grayscale" => 1,
        "off" => 2,
        "rgb" => 3,
        "bgr" => 4,
        "custom" => 7,
        _ => 0,
    }
}

/// Follow the Text rendering tab's mode in Explorer:
///   grayscale → force grayscale
///   off       → force no antialiasing, crisp
///   rgb       → swash subpixel antialiasing (RGB panel)
///   bgr       → swash subpixel antialiasing (BGR panel)
/// Stores the mode and applies it through the master toggle.
#[tauri::command]
pub fn explorer_hook_set_mode(app: tauri::AppHandle, mode: String) -> Result<(), String> {
    let state = mode_id(&mode);
    if LAST_STATE.load(Ordering::Acquire) != state {
        // The config changed: the running Explorer must be restarted again
        // before the new mode is fully applied everywhere.
        RESTARTED_WHILE_ACTIVE.store(false, Ordering::Release);
    }
    LAST_STATE.store(state, Ordering::Release);
    apply_state(&app)
}

/// Per-monitor master switch for the Explorer hook (Text tab). When on, each
/// draw's DC is attributed to a monitor and rendered with that monitor's
/// assigned mode; monitors without an assignment fall back to the global mode.
#[tauri::command]
pub fn explorer_hook_set_per_monitor(enabled: bool) -> Result<(), String> {
    PER_MONITOR_ENABLED.store(enabled, Ordering::Release);
    write_state_table()?;
    invalidate_explorer_windows();
    broadcast_rendering_change();
    Ok(())
}

/// Hook scope (Text tab): `hook_all` widens the swash detour from the
/// shell/Control Panel hosts to every GUI process (the default — GDI apps
/// like 7-Zip force ClearType per-font and ignore the registry setting);
/// `blocklist` excludes specific exe names from the detour as the escape
/// hatch. Written into shared memory so the DLL's per-draw guard applies it
/// live (newly started processes take it at attach).
#[tauri::command]
pub fn explorer_hook_set_scope(
    hook_all: bool,
    blocklist: Vec<String>,
) -> Result<(), String> {
    HOOK_ALL_APPS.store(hook_all, Ordering::Release);
    {
        let mut list = BLOCKLIST
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *list = blocklist
            .iter()
            .map(|s| normalize_exe(s))
            .filter(|s| !s.is_empty())
            .take(MAX_BLOCKLIST)
            .collect();
    }
    write_scope()?;
    broadcast_rendering_change();
    Ok(())
}

/// Assign a per-monitor render mode: `mode` is one of the mode ids
/// ("grayscale"/"off"/"rgb"/"bgr"/"custom"); "default" clears the
/// assignment so the monitor uses the system text setting. `sig` is the
/// monitor's stable signature from `explorer_hook_monitors`. The fixed
/// WOLED RGWB/RWBG presets (ids 5/6) were removed — the Custom mode covers
/// those layouts with user-tuned offsets — so a stale string maps to 0.
#[tauri::command]
pub fn explorer_hook_set_monitor_mode(sig: String, mode: String) -> Result<(), String> {
    let value = mode_id(&mode); // "default" (and anything unknown) maps to 0
    {
        let mut modes = MONITOR_MODES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if value == 0 {
            modes.remove(&sig);
        } else {
            modes.insert(sig, value);
        }
    }
    write_state_table()?;
    invalidate_explorer_windows();
    broadcast_rendering_change();
    Ok(())
}

/// The connected monitors for the per-monitor UI. `sig` is the stable key
/// used to persist assignments; `index` is the enumeration position shown to
/// the user; `friendly_name` is the CCD/EDID name (e.g. "LG OLED C4") when
/// available.
#[derive(serde::Serialize)]
pub struct ExplorerMonitorInfo {
    sig: String,
    index: u32,
    width: u32,
    height: u32,
    primary: bool,
    friendly_name: String,
}

#[tauri::command]
pub fn explorer_hook_monitors() -> Result<Vec<ExplorerMonitorInfo>, String> {
    Ok(all_monitors()
        .iter()
        .enumerate()
        .map(|(i, e)| ExplorerMonitorInfo {
            sig: e.sig(),
            index: i as u32,
            width: e.width,
            height: e.height,
            primary: e.primary,
            friendly_name: e.friendly_name.clone(),
        })
        .collect())
}

/// Assign a per-monitor render knob pair (faux boldness, fringe damping;
/// 0..=100, boldness capped at 30). Written into the monitor table so the
/// DLL applies them to that monitor's draws on the next repaint (live). An
/// entry equal to the defaults (0, 0) — "no adjustment" — clears the entry.
#[tauri::command]
pub fn explorer_hook_set_monitor_knobs(
    sig: String,
    boldness: u32,
    damping: u32,
) -> Result<(), String> {
    {
        let mut knobs = MONITOR_KNOBS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let pair = (boldness.min(30), damping.min(100));
        if pair == (0, 0) {
            knobs.remove(&sig);
        } else {
            knobs.insert(sig, pair);
        }
    }
    write_state_table()?;
    invalidate_explorer_windows();
    broadcast_rendering_change();
    Ok(())
}

/// Assign a per-monitor custom subpixel layout: the R/G/B subpixel centers
/// as (x, y) fractions of a pixel (e.g. RGB stripe ≈ [-1/3, 0], [0, 0],
/// [1/3, 0]; the WOLED quad ≈ [-0.375, 0], [-0.125, 0], [0.375, 0]). Only
/// used when the monitor's mode is "custom". All-zero clears the entry
/// (falls back to the mode default).
#[tauri::command]
pub fn explorer_hook_set_monitor_custom(
    sig: String,
    r_x: f32,
    r_y: f32,
    g_x: f32,
    g_y: f32,
    b_x: f32,
    b_y: f32,
) -> Result<(), String> {
    {
        let mut custom = MONITOR_CUSTOM
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Sanity-bound the centers to ±1 pixel (the usable subpixel space).
        let offsets = [
            [r_x.clamp(-1.0, 1.0), r_y.clamp(-1.0, 1.0)],
            [g_x.clamp(-1.0, 1.0), g_y.clamp(-1.0, 1.0)],
            [b_x.clamp(-1.0, 1.0), b_y.clamp(-1.0, 1.0)],
        ];
        if offsets == [[0.0; 2]; 3] {
            custom.remove(&sig);
        } else {
            custom.insert(sig, offsets);
        }
    }
    write_state_table()?;
    invalidate_explorer_windows();
    broadcast_rendering_change();
    Ok(())
}

/// Apply the persisted Text-tab hook state at app startup, so the hook is
/// installed (and per-monitor profiles loaded) without requiring the Text tab
/// to be opened first. Mirrors what TabText.svelte pushes on mount: the
/// global mode is the system effective text setting, per-monitor overrides
/// come from the config, and the per-monitor flag derives from whether any
/// assignment exists.
pub fn apply_startup_state(
    app: &tauri::AppHandle,
    cfg: &crate::config::AppConfig,
) -> Result<(), String> {
    TOGGLE.store(cfg.explorer_hook_enabled, Ordering::Release);
    HOOK_ALL_APPS.store(cfg.explorer_hook_all_apps, Ordering::Release);
    {
        let mut list = BLOCKLIST
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *list = cfg
            .explorer_hook_blocklist
            .iter()
            .map(|s| normalize_exe(s))
            .filter(|s| !s.is_empty())
            .take(MAX_BLOCKLIST)
            .collect();
    }
    if !cfg.explorer_hook_enabled {
        return Ok(());
    }
    // Global mode = the system effective text setting (registry), the same
    // value the Text tab syncs when it mounts.
    LAST_STATE.store(
        mode_id(&crate::text_rendering::system_effective_mode()),
        Ordering::Release,
    );
    {
        let mut modes = MONITOR_MODES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        modes.clear();
        for (sig, mode) in &cfg.per_monitor_modes {
            modes.insert(sig.clone(), mode_id(mode));
        }
    }
    {
        let mut knobs = MONITOR_KNOBS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        knobs.clear();
        for (sig, k) in &cfg.per_monitor_knobs {
            knobs.insert(sig.clone(), (k.boldness, k.damping));
        }
    }
    {
        let mut custom = MONITOR_CUSTOM
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        custom.clear();
        for (sig, offsets) in &cfg.per_monitor_custom {
            custom.insert(sig.clone(), *offsets);
        }
    }
    PER_MONITOR_ENABLED.store(
        !cfg.per_monitor_modes.is_empty()
            || !cfg.per_monitor_knobs.is_empty()
            || !cfg.per_monitor_custom.is_empty(),
        Ordering::Release,
    );
    apply_state(app)
}

/// Master toggle for the Explorer hook (Text tab). Off stops the hook and
/// writes passthrough so Explorer's text returns to system ClearType; on
/// re-enables the mode mapping.
#[tauri::command]
pub fn explorer_hook_set_enabled(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    if TOGGLE.load(Ordering::Acquire) != enabled {
        RESTARTED_WHILE_ACTIVE.store(false, Ordering::Release);
    }
    TOGGLE.store(enabled, Ordering::Release);
    apply_state(&app)
}

/// Report whether the hook is currently VERIFIED in the running explorer.exe,
/// so the Text tab can reflect it.
///
/// active is not inferred from app bookkeeping — the DLL itself writes its pid
/// and status into the shared memory at attach, and we only count it when the
/// report comes from the CURRENT explorer.exe generation AND the detour
/// actually installed. That closes the gap where the DLL mapped but
/// install_detour silently failed, so the app used to claim "Fix applied"
/// while Explorer's text was untouched.
///
/// applied additionally requires that the current mode is actually VISIBLE:
/// fonts are rasterized at window creation, so a mode change only reaches
/// already-open windows after Explorer restarts. It is set when a fresh
/// explorer.exe generation is verified hooked (the watchdog detects the
/// restart via a pid change); any mode/toggle change resets it until then.
#[derive(serde::Serialize)]
pub struct ExplorerHookStatus {
    enabled: bool,
    active: bool,
    applied: bool,
    pid: u32,
}

#[tauri::command]
pub fn explorer_hook_status() -> Result<ExplorerHookStatus, String> {
    let enabled = TOGGLE.load(Ordering::Acquire);
    let current = explorer_target().map(|(pid, _)| pid).unwrap_or(0);
    let (reported_pid, status) = read_hook_report();
    let active = enabled
        && current != 0
        && reported_pid == current
        && status & STATUS_HOOK_W_INSTALLED != 0;
    let applied = active && RESTARTED_WHILE_ACTIVE.load(Ordering::Acquire);
    Ok(ExplorerHookStatus {
        enabled,
        active,
        applied,
        pid: current,
    })
}

// Confirm the DLL actually reports itself hooked in this explorer generation.
// The DLL loads into explorer asynchronously after the hook is installed (on
// its next window-creation callback), so this runs from the watchdog rather
// than synchronously inside install_hook. A pid change while VERIFIED_PID is
// already set means Explorer restarted, which is exactly what "applied" means.
fn verify_report(pid: u32) {
    let (reported_pid, status) = read_hook_report();
    if reported_pid != pid || status & STATUS_HOOK_W_INSTALLED == 0 {
        return;
    }
    let prev = VERIFIED_PID.swap(pid, Ordering::AcqRel);
    if prev == pid {
        return;
    }
    // A fresh hook just installed (first load or Explorer restart): repaint so
    // already-open windows pick up the mode immediately.
    invalidate_explorer_windows();
    if prev != 0 {
        RESTARTED_WHILE_ACTIVE.store(true, Ordering::Release);
        log::info!("[explorer-hook] verified hook in restarted explorer.exe (pid {pid})");
    } else {
        log::info!("[explorer-hook] verified hook in explorer.exe (pid {pid})");
    }
}

/// Install the global WH_CBT hook if it isn't already installed. Serialized so
/// the watchdog and the mode-change handler cannot race. A global hook reaches
/// every GUI process automatically (including ones started later), so there is
/// no per-process install work — this only ensures the single hook exists.
fn ensure_hooked(app: &tauri::AppHandle) -> Result<(), String> {
    let _guard = HOOK_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let installed = global_hook()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_some();
    if installed {
        return Ok(());
    }
    install_hook(app)
}

/// Start the watchdog exactly once. It verifies the Explorer report (so the
/// applied flag tracks reality) and re-installs the hook if it was ever lost.
fn ensure_watchdog_running(app: tauri::AppHandle) {
    if WATCHDOG_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("explorer-hook-watchdog".to_string())
        .spawn(move || watchdog_loop(app));
    if let Err(e) = spawned {
        log::warn!("[explorer-hook] failed to spawn watchdog: {e}");
        WATCHDOG_STARTED.store(false, Ordering::SeqCst);
    }
}

fn watchdog_loop(app: tauri::AppHandle) {
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            return;
        }
        if ENABLED.load(Ordering::Acquire) {
            if let Err(e) = ensure_hooked(&app) {
                log::warn!("[explorer-hook] hook install failed, will retry: {e}");
            }
            // Verify the primary target (Explorer) for the applied flag. Only
            // Explorer writes its report back into shared memory, so this
            // stays unambiguous even with the global hook reaching many
            // processes.
            if let Some((pid, _)) = explorer_target() {
                verify_report(pid);
            }
        }
        sleep_or_shutdown(1);
    }
}

fn sleep_or_shutdown(seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        if SHUTDOWN.load(Ordering::Acquire) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Signal the watchdog to stop and drop the hook during app shutdown.
pub(crate) fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    uninstall_all_hooks();
}
