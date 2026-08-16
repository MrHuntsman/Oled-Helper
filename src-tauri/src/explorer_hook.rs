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
// RGB, 4 = subpixel BGR, 5 = WOLED RGWB, 6 = WOLED RWBG. Because the hook
// sits on the draw path, a mode change applies on the next repaint (live).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tauri::Manager;
use windows::core::{s, w, BOOL, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, HINSTANCE, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM, HWND,
};
use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
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
// lifetime). Layout: u32 mode (app->explorer) + u32 status + u32 pid
// (explorer->app, written by the DLL at attach so the app can VERIFY the
// hook is really installed in the current explorer.exe instead of trusting
// its own bookkeeping — the DLL loading only proves the module mapped, not
// that the detour installed).
static STATE_VIEW: OnceLock<usize> = OnceLock::new();
const STATE_SIZE: usize = 24;
const STATE_STATUS_OFFSET: usize = 4;
const STATE_PID_OFFSET: usize = 8;
const STATE_LCD_STRENGTH_OFFSET: usize = 12;
const STATE_CROSSTALK_OFFSET: usize = 16;
const STATE_BOLDNESS_OFFSET: usize = 20;
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
// Last mode-state from the Text tab (0..6), gated by TOGGLE in apply_state.
static LAST_STATE: AtomicU32 = AtomicU32::new(0);
// Render knobs from the Text tab sliders (0..=100): LCD filter strength (0 =
// off), WOLED crosstalk compensation, and faux boldness. Written into the
// shared memory so the DLL reads them on every draw.
static LCD_STRENGTH: AtomicU32 = AtomicU32::new(0);
static CROSSTALK: AtomicU32 = AtomicU32::new(0);
static BOLDNESS: AtomicU32 = AtomicU32::new(0);
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

fn write_state_knobs() -> Result<(), String> {
    let ptr = ensure_state_mapped()?;
    // SAFETY: ptr is the mapped view, valid for the process lifetime; offsets
    // 12/16/20 are u32 slots we own.
    unsafe {
        let base = ptr as *mut u8;
        std::ptr::write_volatile(
            base.add(STATE_LCD_STRENGTH_OFFSET) as *mut u32,
            LCD_STRENGTH.load(Ordering::Acquire),
        );
        std::ptr::write_volatile(
            base.add(STATE_CROSSTALK_OFFSET) as *mut u32,
            CROSSTALK.load(Ordering::Acquire),
        );
        std::ptr::write_volatile(
            base.add(STATE_BOLDNESS_OFFSET) as *mut u32,
            BOLDNESS.load(Ordering::Acquire),
        );
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

// Write the embedded DLL into the config dir (next to config.json) so it can
// be loaded by a full path. A short SHA-256 prefix of the embedded bytes is
// baked into the filename, so a newer app build writes a fresh file instead of
// trying to overwrite a DLL an older explorer.exe may still have mapped
// (Windows locks loaded DLL files).
fn ensure_hook_dll(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("could not resolve config dir: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("could not create config dir: {e}"))?;

    let digest = Sha256::digest(EMBEDDED_HOOK_DLL);
    let hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("explorer_hook-{hash}.dll"));
    if !path.exists() {
        fs::write(&path, EMBEDDED_HOOK_DLL)
            .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    }
    // Best-effort cleanup of superseded versions. Ignore failures: a DLL still
    // loaded by a live explorer.exe cannot be deleted, and that is fine — it
    // gets removed on a later run once the process has restarted.
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("explorer_hook-")
                && name.ends_with(".dll")
                && name != format!("explorer_hook-{hash}.dll")
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
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
        let addr = GetProcAddress(module, s!("hook_callback"))
            .ok_or("hook_callback export not found in the explorer hook DLL")?;
        std::mem::transmute(addr)
    };

    // SAFETY: installs a global WH_CBT hook. Thread id 0 associates it with
    // all threads on the desktop; because the hook procedure lives in the DLL,
    // Windows maps the DLL into each GUI process for us. WH_CBT fires on
    // window creation, before fonts rasterize, so fast-starting applets get
    // the detour in time.
    let hook = unsafe { SetWindowsHookExW(WH_CBT, Some(proc), Some(HINSTANCE(module.0)), 0) }
        .map_err(|e| format!("SetWindowsHookExW failed: {e}"))?;

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
    // rewriting fonts immediately (no restart needed for that). Keep the LCD
    // filter flag in sync regardless, so the DLL never reads a stale value.
    write_state(if enabled { last } else { 0 })?;
    write_state_knobs()?;
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

/// Follow the Text rendering tab's mode in Explorer:
///   grayscale → force grayscale
///   off       → force no antialiasing, crisp
///   rgb       → swash subpixel antialiasing (RGB panel)
///   bgr       → swash subpixel antialiasing (BGR panel)
///   rgwb      → WOLED RGBW subpixel antialiasing (R, G, W, B)
///   rwbg      → WOLED RGBW subpixel antialiasing (R, W, B, G)
/// Stores the mode and applies it through the master toggle.
#[tauri::command]
pub fn explorer_hook_set_mode(app: tauri::AppHandle, mode: String) -> Result<(), String> {
    let state = match mode.as_str() {
        "grayscale" => 1,
        "off" => 2,
        "rgb" => 3,
        "bgr" => 4,
        "rgwb" => 5,
        "rwbg" => 6,
        _ => 0,
    };
    if LAST_STATE.load(Ordering::Acquire) != state {
        // The config changed: the running Explorer must be restarted again
        // before the new mode is fully applied everywhere.
        RESTARTED_WHILE_ACTIVE.store(false, Ordering::Release);
    }
    LAST_STATE.store(state, Ordering::Release);
    apply_state(&app)
}

/// Set the render knobs from the Text tab sliders: LCD filter strength
/// (0 = off), WOLED crosstalk compensation, and faux boldness (all 0..=100).
/// Written to the shared memory so the DLL applies them on the next repaint
/// (live).
#[tauri::command]
pub fn explorer_hook_set_render_knobs(
    lcd_strength: u32,
    crosstalk: u32,
    boldness: u32,
) -> Result<(), String> {
    LCD_STRENGTH.store(lcd_strength.min(100), Ordering::Release);
    CROSSTALK.store(crosstalk.min(100), Ordering::Release);
    BOLDNESS.store(boldness.min(100), Ordering::Release);
    write_state_knobs()?;
    invalidate_explorer_windows();
    broadcast_rendering_change();
    Ok(())
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
