// explorer_hook.rs — inject explorer_hook.dll into explorer.exe and control
// its grayscale-forcing flag. The Windows shell hard-codes ClearType and
// ignores the per-user font-smoothing registry values, so the only way to
// change Explorer's text is to inject a DLL that hooks CreateFontIndirectW
// (see explorer-hook/). The DLL reads a session-local shared-memory flag that
// this module writes: 0 = pass through, 1 = force ANTIALIASED_QUALITY.

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
    CreateRemoteThread, GetExitCodeThread, OpenProcess, WaitForSingleObject, PROCESS_ALL_ACCESS,
    PROCESS_SYNCHRONIZE,
};

type LoadLibraryWFn = unsafe extern "system" fn(PCWSTR) -> HMODULE;
type ThreadProc = unsafe extern "system" fn(*mut core::ffi::c_void) -> u32;

// The mapped view of the shared flag (created once, leaked for app lifetime).
static STATE_VIEW: OnceLock<usize> = OnceLock::new();
// Last explorer PID we injected into, so we only re-inject after a restart.
static LAST_INJECTED_PID: AtomicU32 = AtomicU32::new(0);
// Whether the feature is active (Grayscale/Off vs RGB/BGR).
static ENABLED: AtomicBool = AtomicBool::new(false);
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

fn write_state(enabled: bool) -> Result<(), String> {
    let ptr = ensure_state_mapped()?;
    // SAFETY: ptr is the mapped view, valid for the process lifetime.
    unsafe {
        *ptr = if enabled { 1 } else { 0 };
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
        let process = OpenProcess(PROCESS_ALL_ACCESS, false, pid)
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

        WaitForSingleObject(thread, u32::MAX);
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

/// Follow the Text rendering tab's mode: force grayscale in Explorer while the
/// app is in Grayscale or Off, and leave Explorer's ClearType alone in
/// RGB/BGR. Writes the shared flag, starts the restart watchdog, and injects
/// (or re-injects) the hook DLL into explorer.exe.
#[tauri::command]
pub fn explorer_hook_set_force_grayscale(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    ENABLED.store(enabled, Ordering::Release);
    write_state(enabled)?;
    ensure_watchdog_running(app.clone());
    ensure_injected(&app)
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
        let Some(pid) = find_explorer_pid() else {
            // Explorer isn't running (mid-logoff etc.). Back off and retry.
            sleep_or_shutdown(2);
            continue;
        };
        // Hold a SYNCHRONIZE handle and block until Explorer exits, ticking
        // once per second so shutdown is still observed without a signal.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok() };
        match handle {
            Some(h) => {
                loop {
                    if SHUTDOWN.load(Ordering::Acquire) {
                        let _ = unsafe { CloseHandle(h) };
                        return;
                    }
                    let wait = unsafe { WaitForSingleObject(h, 1000) };
                    if wait != WAIT_TIMEOUT {
                        break; // explorer exited
                    }
                }
                let _ = unsafe { CloseHandle(h) };
            }
            None => {
                sleep_or_shutdown(2);
                continue;
            }
        }
        // Explorer restarted: re-inject if the feature is still enabled.
        if ENABLED.load(Ordering::Acquire) {
            let _ = ensure_injected(&app);
        }
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

