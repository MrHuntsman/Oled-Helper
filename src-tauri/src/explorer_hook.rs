// explorer_hook.rs — inject explorer_hook.dll into explorer.exe and control
// its font-antialiasing override. The Windows shell hard-codes ClearType and
// ignores the per-user font-smoothing registry values, so the only way to
// change Explorer's text is to inject a DLL that hooks CreateFontIndirectW
// (see explorer-hook/). The DLL reads a session-local shared-memory mode
// that this module writes: 0 = pass through, 1 = force grayscale
// (ANTIALIASED_QUALITY), 2 = force no antialiasing / crisp
// (NONANTIALIASED_QUALITY).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tauri::Manager;
use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HMODULE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, VirtualAllocEx, VirtualFreeEx, FILE_MAP_ALL_ACCESS,
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, OpenProcess, TerminateThread, WaitForSingleObject,
    PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ,
    PROCESS_VM_WRITE,
};

type LoadLibraryWFn = unsafe extern "system" fn(PCWSTR) -> HMODULE;
type ThreadProc = unsafe extern "system" fn(*mut core::ffi::c_void) -> u32;

// The mapped view of the shared flag (created once, leaked for app lifetime).
static STATE_VIEW: OnceLock<usize> = OnceLock::new();
// Last explorer PID we injected into, so we only re-inject after a restart.
static LAST_INJECTED_PID: AtomicU32 = AtomicU32::new(0);
// Whether the feature is active (Grayscale/Off vs RGB/BGR).
static ENABLED: AtomicBool = AtomicBool::new(false);
// Master toggle from the Text tab (persisted in config): off means Explorer
// is never touched and the DLL is not injected. Defaults to off; the
// frontend syncs the stored value on load.
static TOGGLE: AtomicBool = AtomicBool::new(false);
// Last mode-state from the Text tab (0/1/2), gated by TOGGLE in apply_state.
static LAST_STATE: AtomicU32 = AtomicU32::new(0);
// True once the hook is loaded in the current explorer.exe generation, so
// the Text tab can show "✓ Fix applied". Set when Explorer is restarted
// while the hook is active (the in-app restart) and whenever the hook is
// injected into a new explorer generation — an external restart while the
// app is running (watchdog re-injection) or an app restart that finds a
// fresh Explorer. A generation the hook was injected into renders every
// font through the override, which is exactly what "applied" means. Any
// mode/toggle change resets it (the running Explorer needs another restart
// to fully apply to already-open windows).
static RESTARTED_WHILE_ACTIVE: AtomicBool = AtomicBool::new(false);
// Watchdog lifecycle: started once, stopped on app shutdown.
static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
// Serializes injection so the watchdog and the command handler don't race.
static INJECT_LOCK: Mutex<()> = Mutex::new(());

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
            4,
            PCWSTR(name.as_ptr()),
        )
        .map_err(|e| format!("CreateFileMappingW failed: {e}"))?;
        let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 4);
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

fn find_explorer_pid() -> Option<u32> {
    // SAFETY: ToolHelp snapshot; the handle is closed before returning.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if name.eq_ignore_ascii_case("explorer.exe") {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

// The hook DLL is compiled into the exe as a byte blob by build.rs (see
// build_explorer_hook in build.rs), so the app stays a single portable exe. At
// runtime the bytes are written to the config dir and that on-disk path is
// what LoadLibraryW in explorer.exe opens.
static EMBEDDED_HOOK_DLL: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/explorer_hook.dll"));

// Write the embedded DLL into the config dir (next to config.json) so
// explorer.exe can LoadLibraryW it. A short SHA-256 prefix of the embedded
// bytes is baked into the filename, so a newer app build writes a fresh file
// instead of trying to overwrite a DLL an older explorer.exe may still have
// mapped (Windows locks loaded DLL files).
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

fn inject(pid: u32, dll_path: &Path) -> Result<(), String> {
    // SAFETY: standard LoadLibraryW remote-thread injection. All handles and
    // the remote allocation are released on every path.
    unsafe {
        // Least privilege: only the rights the injection actually needs. A
        // PROCESS_ALL_ACCESS handle to a shell process is exactly the kind of
        // thing antivirus heuristics flag.
        let process = OpenProcess(
            PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_WRITE
                | PROCESS_VM_READ,
            false,
            pid,
        )
        .map_err(|e| format!("OpenProcess(explorer.exe) failed: {e}"))?;

        let wide_path: Vec<u16> = dll_path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let byte_len = wide_path.len() * 2;

        let remote = VirtualAllocEx(process, None, byte_len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote.is_null() {
            let _ = CloseHandle(process);
            return Err("VirtualAllocEx failed".to_string());
        }

        if let Err(e) = WriteProcessMemory(
            process,
            remote.cast(),
            wide_path.as_ptr().cast(),
            byte_len,
            None,
        ) {
            let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
            let _ = CloseHandle(process);
            return Err(format!("WriteProcessMemory failed: {e}"));
        }

        let kernel32 = GetModuleHandleW(w!("kernel32.dll"))
            .map_err(|e| format!("GetModuleHandleW(kernel32.dll) failed: {e}"))?;
        let loadlib: LoadLibraryWFn = std::mem::transmute(
            GetProcAddress(kernel32, s!("LoadLibraryW")).ok_or("LoadLibraryW not found")?,
        );

        let thread = CreateRemoteThread(
            process,
            None,
            0,
            Some(std::mem::transmute::<LoadLibraryWFn, ThreadProc>(loadlib)),
            Some(remote.cast()),
            0,
            None,
        )
        .map_err(|e| {
            let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
            let _ = CloseHandle(process);
            format!("CreateRemoteThread failed: {e}")
        })?;

        // Bound the wait: if LoadLibraryW ever wedges in explorer.exe, fail
        // cleanly instead of blocking the command thread forever. On timeout
        // the loader thread is terminated and everything is released.
        if WaitForSingleObject(thread, 10_000) == WAIT_TIMEOUT {
            let _ = TerminateThread(thread, 1);
            let _ = CloseHandle(thread);
            let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
            let _ = CloseHandle(process);
            return Err("LoadLibraryW in explorer.exe timed out".to_string());
        }
        let mut exit_code = 0u32;
        let _ = GetExitCodeThread(thread, &mut exit_code);
        let _ = CloseHandle(thread);
        let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
        let _ = CloseHandle(process);

        if exit_code == 0 {
            Err(
                "LoadLibraryW in explorer.exe returned null — the DLL failed to load \
                 (check it matches the process architecture)."
                    .to_string(),
            )
        } else {
            Ok(())
        }
    }
}

// Recompute the effective state from the master toggle and the last mode,
// write it to the shared memory, and keep the watchdog/injection running only
// while the feature is genuinely active.
fn apply_state(app: &tauri::AppHandle) -> Result<(), String> {
    let last = LAST_STATE.load(Ordering::Acquire);
    let enabled = TOGGLE.load(Ordering::Acquire) && last != 0;
    ENABLED.store(enabled, Ordering::Release);
    // When disabled, write passthrough so an already-injected Explorer stops
    // rewriting fonts immediately (no restart needed for that).
    write_state(if enabled { last } else { 0 })?;
    if enabled {
        ensure_watchdog_running(app.clone());
        ensure_injected(app)?;
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

/// Master toggle for the Explorer hook (Text tab). Off stops the injection
/// and writes passthrough so Explorer's text returns to system ClearType;
/// on re-enables the mode mapping.
#[tauri::command]
pub fn explorer_hook_set_enabled(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    if TOGGLE.load(Ordering::Acquire) != enabled {
        RESTARTED_WHILE_ACTIVE.store(false, Ordering::Release);
    }
    TOGGLE.store(enabled, Ordering::Release);
    apply_state(&app)
}

/// Report whether the hook is currently loaded in the running explorer.exe,
/// so the Text tab can reflect it on the Restart Explorer button: restarting
/// only applies the mode to already-open windows when the DLL is in there.
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
    let pid = LAST_INJECTED_PID.load(Ordering::Acquire);
    let active = enabled && pid != 0 && find_explorer_pid() == Some(pid);
    let applied = active && RESTARTED_WHILE_ACTIVE.load(Ordering::Acquire);
    Ok(ExplorerHookStatus {
        enabled,
        active,
        applied,
        pid,
    })
}

/// Restart explorer.exe so font changes apply to already-open windows
/// immediately (fonts already rasterized stay cached until the process
/// restarts). The watchdog re-injects the hook afterwards if the feature is
/// active. Closes all open File Explorer windows.
#[tauri::command]
pub fn restart_explorer() -> Result<(), String> {
    // taskkill /f does NOT trigger Winlogon's shell auto-restart (only a
    // graceful termination does), so the shell must be started manually —
    // otherwise the desktop stays dead. But spawning the replacement while
    // the old process is still winding down is what makes the taskbar take
    // 10+ seconds (two explorer instances fight over the taskbar), so wait
    // for the killed generation to actually disappear first.
    let old_pid = find_explorer_pid();
    let kill = std::process::Command::new("taskkill")
        .args(["/f", "/im", "explorer.exe"])
        .status();
    match kill {
        Ok(status) if status.success() => {}
        Ok(_) => log::warn!("[explorer-hook] taskkill reported a non-zero exit"),
        Err(e) => log::warn!("[explorer-hook] taskkill failed: {e}"),
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match find_explorer_pid() {
            // Old process fully gone — safe to start the fresh shell.
            None => break,
            // A different generation already appeared on its own (Winlogon
            // auto-restart on some systems) — use it, no manual spawn.
            Some(pid) if old_pid.is_some_and(|old| pid != old) => {
                RESTARTED_WHILE_ACTIVE.store(true, Ordering::Release);
                return Ok(());
            }
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Old explorer is gone — start the shell fresh.
    std::process::Command::new("explorer.exe")
        .spawn()
        .map_err(|e| format!("could not restart explorer.exe: {e}"))?;
    // Explorer is coming up. The watchdog re-injects the hook into the fresh
    // process (its 1s poll); once status.active flips, the fix is fully
    // applied — every window in the new explorer renders through the hook.
    RESTARTED_WHILE_ACTIVE.store(true, Ordering::Release);
    Ok(())
}

/// Inject the hook DLL into the current explorer.exe if this process
/// generation hasn't been injected yet. Serialized so the watchdog and the
/// mode-change handler cannot race.
fn ensure_injected(app: &tauri::AppHandle) -> Result<(), String> {
    let _guard = INJECT_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(pid) = find_explorer_pid() else {
        log::warn!("[explorer-hook] explorer.exe not running; injection deferred");
        return Ok(());
    };
    if LAST_INJECTED_PID.load(Ordering::Acquire) == pid {
        return Ok(());
    }
    let dll = ensure_hook_dll(app)?;
    inject(pid, &dll)?;
    LAST_INJECTED_PID.store(pid, Ordering::Release);
    // The hook is now loaded in this explorer generation (a fresh process,
    // since LAST_INJECTED_PID differed): every font it renders from here on
    // goes through the override, which is exactly what "applied" means. This
    // is what lets an external Explorer restart (watchdog re-injection) or
    // an app restart onto a fresh Explorer show "✓ Fix applied" instead of
    // nagging for another restart.
    RESTARTED_WHILE_ACTIVE.store(true, Ordering::Release);
    log::info!("[explorer-hook] injected explorer_hook.dll into explorer.exe (pid {pid})");
    Ok(())
}

/// Start the restart watchdog exactly once. It waits event-driven on
/// explorer.exe's process handle (no polling) and re-injects when Explorer
/// restarts while the feature is enabled.
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
        // Re-inject whenever the running explorer.exe generation differs from
        // the one we last injected. Polling once per second is cheap (one
        // process snapshot) and immune to the transition race: the old
        // wait-on-handle design called ensure_injected exactly once when the
        // old process died — but at that moment the fresh explorer.exe often
        // did not exist yet, so injection was deferred and the watchdog then
        // blocked on the new process without ever re-injecting. That is why
        // the fix sometimes never applied after a restart.
        if ENABLED.load(Ordering::Acquire) {
            let Some(pid) = find_explorer_pid() else {
                // Explorer isn't running (mid-logoff etc.). Back off.
                sleep_or_shutdown(1);
                continue;
            };
            if LAST_INJECTED_PID.load(Ordering::Acquire) != pid {
                // New explorer generation (or first injection): try to inject
                // now, retrying on the next tick if explorer is still
                // initializing and the remote thread failed.
                if let Err(e) = ensure_injected(&app) {
                    log::warn!("[explorer-hook] re-injection failed, will retry: {e}");
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

/// Signal the watchdog to stop during app shutdown.
pub(crate) fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
}

