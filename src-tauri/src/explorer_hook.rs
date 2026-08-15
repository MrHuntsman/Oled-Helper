// explorer_hook.rs — load explorer_hook.dll into explorer.exe and control
// its font-antialiasing override. The Windows shell hard-codes ClearType and
// ignores the per-user font-smoothing registry values, so the only way to
// change Explorer's text is to load a DLL that hooks CreateFontIndirectW (see
// explorer-hook/). The DLL is loaded through a thread-specific
// SetWindowsHookExW CALLWNDPROC hook on Explorer's UI thread — the documented
// OS mechanism — instead of CreateRemoteThread injection, so Windows Defender
// does not flag the app the way it did the injection path. The DLL reads a
// session-local shared-memory mode that this module writes: 0 = pass through,
// 1 = force grayscale (ANTIALIASED_QUALITY), 2 = force no antialiasing / crisp
// (NONANTIALIASED_QUALITY).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tauri::Manager;
use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HINSTANCE, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK,
    WH_CALLWNDPROC,
};

type HookProcFn = unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT;

// The mapped view of the shared state (created once, leaked for app
// lifetime). Layout: u32 mode (app->explorer) + u32 status + u32 pid
// (explorer->app, written by the DLL at attach so the app can VERIFY the
// hook is really installed in the current explorer.exe instead of trusting
// its own bookkeeping — the DLL loading only proves the module mapped, not
// that the detour installed).
static STATE_VIEW: OnceLock<usize> = OnceLock::new();
const STATE_SIZE: usize = 12;
const STATE_STATUS_OFFSET: usize = 4;
const STATE_PID_OFFSET: usize = 8;
// The primary CreateFontIndirectW detour is what makes the fix real; the
// Ex variant is optional (see explorer-hook), so the app only requires this
// one for a hook to count as active.
const STATUS_HOOK_W_INSTALLED: u32 = 1;
// The installed CALLWNDPROC hook (HHOOK value as isize, 0 = none) plus the
// explorer generation it targets. The pid+thread let us detect an Explorer
// restart: a fresh shell gets a new thread id, so we re-install the hook.
static HOOK_VALUE: AtomicUsize = AtomicUsize::new(0);
static HOOKED_PID: AtomicU32 = AtomicU32::new(0);
static HOOKED_TID: AtomicU32 = AtomicU32::new(0);
// Last explorer pid whose hook report verified, so a pid change (Explorer
// restart) can flip the applied flag.
static VERIFIED_PID: AtomicU32 = AtomicU32::new(0);
// Whether the feature is active (Grayscale/Off vs RGB/BGR).
static ENABLED: AtomicBool = AtomicBool::new(false);
// Master toggle from the Text tab (persisted in config): off means Explorer
// is never touched and the DLL is not loaded. Defaults to off; the
// frontend syncs the stored value on load.
static TOGGLE: AtomicBool = AtomicBool::new(false);
// Last mode-state from the Text tab (0/1/2), gated by TOGGLE in apply_state.
static LAST_STATE: AtomicU32 = AtomicU32::new(0);
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

// The main Explorer shell: its taskbar window gives us both the pid (for
// status verification) and the owning UI thread (the hook target). This keeps
// the hook pointed at the same single shell process the old injection used,
// and avoids loading the DLL into every GUI process on the system.
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
// procedure), then install a thread-specific CALLWNDPROC hook on Explorer's UI
// thread. Windows itself maps the DLL into explorer.exe as a result — the
// documented hook mechanism — which is why this does not trip the injection
// signatures that Windows Defender flags.
fn install_hook(app: &tauri::AppHandle, pid: u32, tid: u32) -> Result<(), String> {
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

    // SAFETY: installs a thread-specific CALLWNDPROC hook on Explorer's UI
    // thread. Because the hook procedure lives in the DLL and the thread is in
    // another process, Windows maps the DLL into explorer.exe for us.
    let hook = unsafe {
        SetWindowsHookExW(
            WH_CALLWNDPROC,
            Some(proc),
            Some(HINSTANCE(module.0)),
            tid,
        )
    }
    .map_err(|e| format!("SetWindowsHookExW failed: {e}"))?;

    HOOK_VALUE.store(hook.0 as usize, Ordering::Release);
    HOOKED_PID.store(pid, Ordering::Release);
    HOOKED_TID.store(tid, Ordering::Release);
    log::info!("[explorer-hook] installed CALLWNDPROC hook on explorer.exe (pid {pid}, thread {tid})");
    Ok(())
}

fn uninstall_hook() {
    let value = HOOK_VALUE.swap(0, Ordering::AcqRel);
    if value != 0 {
        // SAFETY: value is a live HHOOK we installed and never otherwise free.
        let _ = unsafe { UnhookWindowsHookEx(HHOOK(value as *mut core::ffi::c_void)) };
    }
    HOOKED_PID.store(0, Ordering::Release);
    HOOKED_TID.store(0, Ordering::Release);
}

// Recompute the effective state from the master toggle and the last mode,
// write it to the shared memory, and keep the hook/watchdog running only while
// the feature is genuinely active.
fn apply_state(app: &tauri::AppHandle) -> Result<(), String> {
    let last = LAST_STATE.load(Ordering::Acquire);
    let enabled = TOGGLE.load(Ordering::Acquire) && last != 0;
    ENABLED.store(enabled, Ordering::Release);
    // When disabled, write passthrough so an already-loaded Explorer stops
    // rewriting fonts immediately (no restart needed for that).
    write_state(if enabled { last } else { 0 })?;
    if enabled {
        ensure_watchdog_running(app.clone());
        ensure_hooked(app)?;
    } else {
        uninstall_hook();
    }
    Ok(())
}

/// Follow the Text rendering tab's mode in Explorer:
///   grayscale → force grayscale (ANTIALIASED_QUALITY)
///   off       → force no antialiasing, crisp (NONANTIALIASED_QUALITY)
///   rgb/bgr   → pass through (Explorer keeps its ClearType + the system
///               orientation the app already sets)
/// Stores the mode and applies it through the master toggle.
#[tauri::command]
pub fn explorer_hook_set_mode(app: tauri::AppHandle, mode: String) -> Result<(), String> {
    let state = match mode.as_str() {
        "grayscale" => 1,
        "off" => 2,
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
/// restart and re-installs the hook on the new thread); any mode/toggle change
/// resets it until then.
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
// its next processed message), so this runs from the watchdog rather than
// synchronously inside install_hook. A pid change while VERIFIED_PID is
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
    if prev != 0 {
        RESTARTED_WHILE_ACTIVE.store(true, Ordering::Release);
        log::info!("[explorer-hook] verified hook in restarted explorer.exe (pid {pid})");
    } else {
        log::info!("[explorer-hook] verified hook in explorer.exe (pid {pid})");
    }
}

/// Install the CALLWNDPROC hook on the current explorer.exe if this shell
/// generation isn't already hooked. Serialized so the watchdog and the mode-
/// change handler cannot race.
fn ensure_hooked(app: &tauri::AppHandle) -> Result<(), String> {
    let _guard = HOOK_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some((pid, tid)) = explorer_target() else {
        log::warn!("[explorer-hook] explorer shell window not found; hook deferred");
        return Ok(());
    };
    if HOOK_VALUE.load(Ordering::Acquire) != 0
        && HOOKED_PID.load(Ordering::Acquire) == pid
        && HOOKED_TID.load(Ordering::Acquire) == tid
    {
        // Already hooked on this shell generation.
        return Ok(());
    }
    // A hook on a stale thread (previous shell generation) is dead: drop it
    // before installing the new one.
    uninstall_hook();
    install_hook(app, pid, tid)?;
    Ok(())
}

/// Start the watchdog exactly once. It waits event-driven on the hook state
/// (one poll per second) and re-installs the hook when Explorer restarts while
/// the feature is enabled.
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
        // Re-install whenever the running explorer generation differs from the
        // one the hook targets (Explorer restart) and verify the DLL's report
        // so the applied flag tracks reality. Polling once per second is cheap
        // (one FindWindowW) and immune to the transition race where the fresh
        // explorer.exe shell window does not exist yet.
        if ENABLED.load(Ordering::Acquire) {
            match explorer_target() {
                Some((pid, tid)) => {
                    let hooked = HOOK_VALUE.load(Ordering::Acquire) != 0
                        && HOOKED_PID.load(Ordering::Acquire) == pid
                        && HOOKED_TID.load(Ordering::Acquire) == tid;
                    if !hooked {
                        if let Err(e) = ensure_hooked(&app) {
                            log::warn!("[explorer-hook] hook install failed, will retry: {e}");
                        }
                    }
                    verify_report(pid);
                }
                None => {
                    // Explorer isn't running (mid-logoff etc.). Back off.
                }
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
    uninstall_hook();
}
