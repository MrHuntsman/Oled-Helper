//! Process lifecycle: elevation detection, UAC relaunch, restart handoff, and
//! external-link opening. Command-line flag names are centralized here so the
//! argument builders (restart/elevate) and parsers (run) cannot drift.

use tauri::AppHandle;

// Command-line flags shared by the restart/elevation paths and parsed by run().
pub(crate) const ARG_RESTART_PARENT: &str = "--restart-parent";
pub(crate) const ARG_OPEN_TAB_PREFIX: &str = "--open-tab=";
// Passed by the Start on Boot entry (the autostart plugin builds the Run value
// from the args given to tauri_plugin_autostart::init): launch straight into
// the tray instead of showing the main window. A login launch must not steal
// focus, and the app is designed to idle in the tray with its webview torn
// down anyway (see main_window), so this is the same state a close-to-tray
// reaches, just from the first frame. Restart children and manual launches
// never carry it; the UAC relaunch and tray Restart strip nothing because
// they build their own argument lists.
pub(crate) const ARG_START_TRAY: &str = "--start-tray";

// True when this launch was an autostart (Start on Boot) launch.
static START_IN_TRAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn start_in_tray() -> bool {
    START_IN_TRAY.load(std::sync::atomic::Ordering::Acquire)
}

pub(crate) fn set_start_in_tray() {
    START_IN_TRAY.store(true, std::sync::atomic::Ordering::Release);
}

// Tab ids the app actually renders (see the tab switch chain in App.svelte);
// used to validate --open-tab forwarding on restart.
const KNOWN_TABS: [&str; 11] = [
    "crush",
    "dimmer",
    "screen-dimmer",
    "windows-hdr",
    "hotkeys",
    "text",
    "windows-font",
    "debug",
    "system",
    "settings",
    "about",
];

// Removes one-shot handoff arguments from a forwarded command line. These
// flags describe a single immediate handoff (a UAC relaunch or a restart) and
// must not be carried into later launches:
//   - "--restart-parent <pid>" pairs: only the caller-appended current
//     parent pid may be present, otherwise a restart child waits on (or skips
//     waiting for) the wrong process and races its shutdown, so the restarted
//     app never comes back.
//   - "--open-tab=...", "--rtx-hdr-flags=...", "--rtx-hdr-enabled=...",
//     "--hdr-mode=...", "--screen-dimmer-enabled=...": one-shot intent that
//     was already applied (or that the user changed since). Re-forwarding it
//     would force the old tab on every restart of an always-elevated session
//     and re-apply the old setting even after the user disabled it.
// The caller re-appends the flags it actually wants for the new process.
pub(crate) fn strip_handoff_args(args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut stripped = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == ARG_RESTART_PARENT {
            // Drop the pid that follows the flag too.
            let _ = iter.next();
            continue;
        }
        let text = arg.to_string_lossy();
        if text.starts_with(ARG_OPEN_TAB_PREFIX) {
            continue;
        }
        stripped.push(arg);
    }
    stripped
}

// One-time migration for Start on Boot entries created before the app could
// start into the tray: the autostart plugin's registry value was just the exe
// path with no --start-tray, so a config that predates this flag would keep
// opening a visible window at login forever. When the Run entry exists and
// lacks the flag, rewrite it to the same value the plugin would write today
// (path + current args), leaving the StartupApproved enable/disable byte the
// Task Manager shows untouched. Enabled autostart entries only; an entry the
// user disabled in Task Manager must stay as it is. Runs once per process,
// from run(), before the Tauri builder is created.
#[cfg(target_os = "windows")]
pub(crate) fn migrate_autostart_entry() {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ,
    };

    // Cheap gate: only the exe path needs rewriting, and a value that already
    // carries the flag is correct as is.
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return,
    };
    let exe_str = exe.display().to_string();

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    // Must match the app_name the autostart plugin used when it enabled the
    // entry: it defaults to package_info().name, which tauri.conf.json sets
    // to "OLED Helper".
    const VALUE_NAME: &str = "OLED Helper";

    let run_key_w: Vec<u16> = RUN_KEY
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_name_w: Vec<u16> = VALUE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(run_key_w.as_ptr()),
            None,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut hkey,
        )
        .is_err()
        {
            return; // no Run key or no entry: nothing to migrate
        }

        // Read the current command line (REG_SZ). Missing value: autostart
        // has never been enabled, nothing to do.
        let mut size: u32 = 0;
        let query = RegGetValueW(
            hkey,
            PCWSTR::null(),
            PCWSTR(value_name_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        );
        if query.is_err() || size == 0 {
            let _ = RegCloseKey(hkey);
            return;
        }
        let mut buf = vec![0u16; (size as usize) / 2];
        if RegGetValueW(
            hkey,
            PCWSTR::null(),
            PCWSTR(value_name_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
        .is_err()
        {
            let _ = RegCloseKey(hkey);
            return;
        }
        // The registry hands back the length in bytes; the buffer is NUL
        // terminated, so trim to the first NUL.
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let current: String = String::from_utf16_lossy(&buf[..len]);
        let _ = RegCloseKey(hkey);

        // Already migrated (or enabled through a newer build): nothing to do.
        // Also leave entries that do not point at this exe alone; a user who
        // hand-edited the command owns it.
        if current.contains(ARG_START_TRAY) || !current.contains(&exe_str) {
            return;
        }

        // Rewrite to exactly what the plugin writes for a fresh enable with
        // the --start-tray arg: "<exe> --start-tray". The plugin quotes
        // nothing on Windows (auto-launch joins path + args with spaces), so
        // this matches byte for byte for a normally installed path.
        let new_value = format!("{exe_str} {ARG_START_TRAY}");
        let new_w: Vec<u16> = new_value
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(run_key_w.as_ptr()),
            None,
            KEY_SET_VALUE,
            &mut hkey,
        )
        .is_err()
        {
            return;
        }
        let status = RegSetValueExW(
            hkey,
            PCWSTR(value_name_w.as_ptr()),
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                new_w.as_ptr() as *const u8,
                new_w.len() * 2,
            )),
        );
        let _ = RegCloseKey(hkey);
        if status.is_ok() {
            log::info!("[autostart] migrated the Start on Boot entry to launch into the tray");
        } else {
            log::warn!("[autostart] could not migrate the Start on Boot entry: {status:?}");
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn migrate_autostart_entry() {}

pub(crate) fn request_restart(app: &AppHandle, open_tab: Option<&str>) {
    // Always perform a real process restart, including when launched through
    // `tauri dev`. Spawn first so a failure leaves the current instance fully
    // functional rather than closing the app.
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            log::error!("failed to locate replacement executable: {error}");
            return;
        }
    };
    // Forward the original launch arguments minus every one-shot handoff
    // flag (see strip_handoff_args): only the current parent pid and the
    // requested tab below may be present, so the replacement waits for THIS
    // process and never re-applies stale intent from an earlier UAC relaunch.
    let mut args: Vec<std::ffi::OsString> =
        strip_handoff_args(std::env::args_os().skip(1).collect());
    args.push(ARG_RESTART_PARENT.into());
    args.push(std::process::id().to_string().into());
    if let Some(tab) = open_tab {
        if KNOWN_TABS.contains(&tab) {
            args.push(format!("{ARG_OPEN_TAB_PREFIX}{tab}").into());
        }
    }
    if let Err(error) = std::process::Command::new(exe).args(args).spawn() {
        log::error!("failed to spawn replacement instance: {error}");
        return;
    }
    // Milestone for the early logger: with pre-Tauri logging in place the
    // replacement's parent wait and lock race are captured, and this line
    // timestamps the exact moment the old instance began its own teardown.
    log::info!("restart handoff: replacement spawned; this instance is exiting");

    // Release the single-instance lock now that the replacement is running.
    // The named mutex lives until this process's kernel handles are closed,
    // which lags the exit code the replacement polls, so a restart child
    // probing the lock in that window saw "already running" and exited,
    // leaving no app behind. Handing the lock over explicitly removes the
    // race; the retry in acquire_single_instance is the safety net.
    release_single_instance();

    crate::taskbar_dimmer::shutdown();
    crate::shutdown_gamma_worker(app);
    // Spawn the replacement before leaving the event loop so the next process
    // has a guaranteed handoff path. The child waits for this PID to exit
    // (--restart-parent) before initializing WebView2, preventing overlapping
    // browser/profile lifetimes during a tray restart.
    app.exit(0);
}

// Opens an external URL in the user's default browser via ShellExecuteW.
// Without this the WebView would navigate to the link in-place; the
// frontend intercepts anchor clicks and routes them through here instead.
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    // Only allow http/https. ShellExecuteW would otherwise happily launch
    // arbitrary protocols (file:, cmd:, ms-settings:, javascript:, ...) with
    // whatever string the webview passes, so constrain the scheme here.
    let scheme = url.trim().to_ascii_lowercase();
    if !(scheme.starts_with("http://") || scheme.starts_with("https://")) {
        return Err("only http and https links can be opened".into());
    }
    let url_w: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::w!("open"),
            windows::core::PCWSTR(url_w.as_ptr()),
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value greater than 32 on success; values ≤ 32
    // are failures (same convention as spawn_elevated).
    let code = result.0 as isize;
    if code <= 32 {
        return Err(format!("could not open link (error code {code})"));
    }
    Ok(())
}

// Whether the current process runs with an elevated token. The helper child
// checks its own token this way to prove real elevation, and the app uses it
// to decide in-process apply vs. the transient helper round trip.
#[cfg(target_os = "windows")]
pub(crate) fn process_is_elevated() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ACCESS_MASK, TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ACCESS_MASK(0x0008), &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let result = GetTokenInformation(
            token,
            TOKEN_INFORMATION_CLASS(20),
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut std::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        let _ = CloseHandle(token);
        result.is_ok() && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn process_is_elevated() -> bool {
    false
}

// Whether the current foreground window belongs to a process running at a
// higher integrity level than this one (an elevated admin app or game). A
// non-elevated process's low-level hooks are filtered by UIPI and its
// GetAsyncKeyState calls fail once such a window takes the foreground, so
// input paths that need to keep working over it (the Screen Dimmer's idle
// clock, hold-to-compare, the raw-input probe) check this instead of
// assuming the hooks see everything.
#[cfg(target_os = "windows")]
pub(crate) fn foreground_process_is_elevated() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, PSID,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.0.is_null() {
            return false;
        }
        let mut process_id = 0u32;
        let _ = GetWindowThreadProcessId(foreground, Some(&mut process_id));
        if process_id == 0 || process_id == std::process::id() {
            return false;
        }
        let process = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) {
            Ok(process) => process,
            Err(_) => return false,
        };
        let mut token = windows::Win32::Foundation::HANDLE::default();
        const TOKEN_QUERY: u32 = 0x0008;
        if OpenProcessToken(
            process,
            windows::Win32::Security::TOKEN_ACCESS_MASK(TOKEN_QUERY),
            &mut token,
        )
        .is_err()
        {
            let _ = CloseHandle(process);
            return false;
        }
        // TokenIntegrityLevel (25) returns a TOKEN_MANDATORY_LABEL whose
        // label SID's last sub-authority is the integrity RID. Any value at
        // or above SECURITY_MANDATORY_HIGH_RID is elevated from this
        // (medium-integrity) perspective. The buffer is parsed by hand the
        // same way the taskbar dimmer's identical check does.
        let mut buffer = [0u8; 256];
        let mut return_length = 0u32;
        let result = GetTokenInformation(
            token,
            windows::Win32::Security::TOKEN_INFORMATION_CLASS(25),
            Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
            buffer.len() as u32,
            &mut return_length,
        );
        let _ = CloseHandle(token);
        let _ = CloseHandle(process);
        if result.is_err() || !(8..=buffer.len() as u32).contains(&return_length) {
            return false;
        }
        let sid_ptr = std::ptr::read::<*mut std::ffi::c_void>(buffer.as_ptr() as *const _);
        if sid_ptr.is_null() {
            return false;
        }
        let sub_auth = GetSidSubAuthorityCount(PSID(sid_ptr));
        if sub_auth.is_null() || *sub_auth == 0 {
            return false;
        }
        let value = *GetSidSubAuthority(PSID(sid_ptr), (*sub_auth as u32) - 1);
        const SECURITY_MANDATORY_HIGH_RID: u32 = 0x0000_3000;
        value >= SECURITY_MANDATORY_HIGH_RID
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn foreground_process_is_elevated() -> bool {
    false
}

// Launches a copy of the current executable elevated (ShellExecuteW "runas",
// UAC prompt). Forwards the current command-line arguments plus
// --restart-parent <pid> so the replacement waits for this process to exit
// (the same handoff the in-app relaunch uses), then appends the given extra
// args. Returns Err on failure or when the user dismisses the UAC prompt -
// the caller keeps running in that case.
pub(crate) fn spawn_elevated(extra_args: &[String]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Strip every one-shot handoff flag from a previous relaunch (see
    // strip_handoff_args) so the elevated replacement only ever waits on
    // THIS process and receives only the extra_args intent below.
    let mut args: Vec<std::ffi::OsString> =
        strip_handoff_args(std::env::args_os().skip(1).collect());
    args.push(ARG_RESTART_PARENT.into());
    args.push(std::process::id().to_string().into());
    args.extend(extra_args.iter().map(|arg| arg.into()));
    // Quote each argument so a path with spaces survives the command line.
    let params: Vec<u16> = args
        .iter()
        .map(|arg| {
            let text = arg.to_string_lossy().replace('"', "\\\"");
            format!("\"{text}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let exe_w: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::w!("runas"),
            windows::core::PCWSTR(exe_w.as_ptr()),
            windows::core::PCWSTR(params.as_ptr()),
            windows::core::PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value greater than 32 on success; values ≤ 32
    // are failures. The one exception is ERROR_CANCELLED (1223): dismissing
    // the UAC prompt also yields > 32, so it must be checked explicitly -
    // otherwise cancelling the elevation would look like success and kill
    // this instance. Keep the app fully functional whenever the launch did
    // not actually happen.
    let code = result.0 as isize;
    if code <= 32 || code == 1223 {
        // Translate the handful of codes users can actually hit into plain
        // language instead of surfacing the raw Win32/ShellExecuteW value -
        // "ShellExecuteW returned 5" means nothing to someone who just wanted
        // to change an HDR setting.
        let message = match code {
            1223 => "Elevation cancelled. No changes made.".to_string(),
            5 => "Administrator access denied.".to_string(),
            2 | 3 => "App not found to restart. Try reinstalling.".to_string(),
            8 => "Out of memory during restart. Close some programs and retry.".to_string(),
            26 => "A required file is in use. Close other app instances and retry.".to_string(),
            31 => "No program is associated with the restart. Try reinstalling.".to_string(),
            _ => format!("Restart with administrator access failed (error {code}). Restart manually as Administrator."),
        };
        return Err(message);
    }
    Ok(())
}

// After a restart the replacement instance can come up without the
// foreground: an elevated relaunch is spawned by consent.exe (the UAC
// broker) on the secure desktop, so Windows does not treat the new process
// as started by the foreground process and refuses its window activation.
// Bring the main window forward with the standard workarounds: wait for it
// to be visible, un-minimize it, simulate an Alt key press (which marks
// this process as user-interactive for the foreground-lock heuristic), then
// SetForegroundWindow, and if that is refused, a topmost z-order flip that
// usually raises the window anyway. If the grab still fails, flash the
// taskbar button so the window at least announces itself. Called only for
// restart children (tray restart, elevated UAC relaunch), never for
// login/autostart launches where stealing focus would be wrong.
#[cfg(target_os = "windows")]
pub(crate) fn grab_foreground_after_restart(app: AppHandle) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        FlashWindowEx, IsWindowVisible, SetForegroundWindow, SetWindowPos, ShowWindow, FLASHWINFO,
        FLASHW_ALL, FLASHW_TIMERNOFG, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE,
        SWP_SHOWWINDOW, SW_RESTORE,
    };

    let Some(window) = crate::main_window::window(&app) else {
        log::warn!("foreground grab skipped: main window is missing");
        return;
    };
    // Round-trip through isize: Tauri hands back the HWND of its own
    // transitive windows crate version, while the calls below use the direct
    // dependency's version. The raw pointer value is identical either way.
    let hwnd = match window.hwnd() {
        Ok(hwnd) => HWND(hwnd.0 as *mut std::ffi::c_void),
        Err(error) => {
            log::warn!("foreground grab skipped: main window has no handle ({error})");
            return;
        }
    };
    // Give the window time to appear (WebView2 initialization can take a
    // moment) and poll until it is actually visible before grabbing.
    for _ in 0..50 {
        if unsafe { IsWindowVisible(hwnd).as_bool() } {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let grabbed = unsafe {
        // The window may have come up minimized (restored geometry); un-
        // minimize before the grab so the raise also surfaces it.
        let _ = ShowWindow(hwnd, SW_RESTORE);
        // Simulate an Alt key press so Windows treats this process as
        // having user input and allows SetForegroundWindow past the
        // foreground lock.
        keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
        keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        let mut grabbed = SetForegroundWindow(hwnd).as_bool();
        // The Alt trick is not always enough right after a UAC relaunch.
        // The topmost flip forces a z-order raise that usually succeeds
        // where SetForegroundWindow alone was refused.
        if !grabbed {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            grabbed = SetForegroundWindow(hwnd).as_bool();
        }
        if !grabbed {
            // Last resort: pulse the taskbar button a few times so the
            // window still announces itself if it cannot take the
            // foreground.
            let info = FLASHWINFO {
                cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                hwnd,
                dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
                uCount: 3,
                dwTimeout: 0,
            };
            let _ = FlashWindowEx(&info);
        }
        grabbed
    };
    if grabbed {
        log::info!("foreground grab succeeded after restart");
    } else {
        log::warn!("foreground grab refused after restart; taskbar button flashed");
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn grab_foreground_after_restart(_app: AppHandle) {}

// ── Cross-elevation single-instance guard ────────────────────────────────
// tauri-plugin-single-instance (replaced below) created its named mutex
// with a default security descriptor. A mutex created by an elevated
// (Administrator) process carries a High mandatory integrity label and a
// DACL granting only the creator/system/administrators, so a second,
// NON-elevated instance failed to open it with ERROR_ACCESS_DENIED instead
// of ERROR_ALREADY_EXISTS - and started a second copy of the app (the bug
// where opening the app as admin then launching it normally produced two
// apps). This guard creates the mutex (and a notification event) with a
// NULL DACL (everyone may open it) plus a Medium mandatory integrity label,
// so elevated and non-elevated instances see the same lock no matter which
// one started first.
//
// A second instance is detected from the mutex alone, and the first
// instance is woken through a named event (a kernel object) rather than a
// window message: UIPI would silently drop WM_COPYDATA sent from a
// medium-integrity process to a high-integrity window, while kernel objects
// are not subject to UIPI.
//
// Names keep the plugin's "{identifier}-sim" suffix so an already-running
// plugin-era instance is still detected by a new build (the first launcher
// of a pair wins, which was the old behavior too). APP_IDENTIFIER mirrors
// tauri.conf.json's "identifier".
pub(crate) const APP_IDENTIFIER: &str = "OledHelper";

// Raw handle values, 0 when unset. Atomics rather than OnceLock because a
// restarting instance hands the mutex over explicitly: the handle has to be
// closable, and forgettable, before the process ends.
#[cfg(target_os = "windows")]
static SINGLE_INSTANCE_MUTEX: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(target_os = "windows")]
static SINGLE_INSTANCE_EVENT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// How long a restart child keeps retrying the single-instance lock while the
// old instance finishes releasing its kernel handles. Bounded so a genuinely
// running instance (a second launch racing the restart) still wins.
#[cfg(target_os = "windows")]
const RESTART_LOCK_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(target_os = "windows")]
const RESTART_LOCK_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(target_os = "windows")]
use std::sync::atomic::Ordering;

// SeSecurityPrivilege is required to attach a SACL (the Medium integrity
// label below) to a newly created kernel object. It sits disabled in an
// elevated token and is absent from a normal one, so this is best effort:
// failure just means the SACL step is skipped, which is fine when the
// creator is non-elevated (its objects already default to the Medium
// label).
#[cfg(target_os = "windows")]
unsafe fn enable_security_privilege() {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        SE_SECURITY_NAME, TOKEN_ACCESS_MASK, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
        TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = windows::Win32::Foundation::HANDLE::default();
    if OpenProcessToken(
        GetCurrentProcess(),
        TOKEN_ACCESS_MASK(TOKEN_ADJUST_PRIVILEGES.0 | TOKEN_QUERY.0),
        &mut token,
    )
    .is_err()
    {
        return;
    }
    let mut luid = windows::Win32::Foundation::LUID::default();
    if LookupPrivilegeValueW(windows::core::PCWSTR::null(), SE_SECURITY_NAME, &mut luid).is_err() {
        let _ = CloseHandle(token);
        return;
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    let _ = AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None);
    let _ = CloseHandle(token);
}

// Security descriptor handed to CreateMutexW/CreateEventW so the object is
// reachable from BOTH integrity levels:
//   - NULL DACL: no access protection, anyone may open/modify it.
//   - Medium mandatory integrity label in the SACL: without it, an object
//     created by an elevated process gets a High label that a non-elevated
//     process fails the mandatory-integrity check on even with a NULL DACL.
// The descriptor (and its backing ACL buffer) must stay alive for as long
// as the creation call runs, so it is returned as one self-contained value.
#[cfg(target_os = "windows")]
#[repr(C, align(8))]
struct AlignedBytes([u8; 64]);

#[cfg(target_os = "windows")]
struct PermissiveSecurity {
    descriptor: windows::Win32::Security::SECURITY_DESCRIPTOR,
    acl_storage: AlignedBytes,
    attributes: windows::Win32::Security::SECURITY_ATTRIBUTES,
}

#[cfg(target_os = "windows")]
fn permissive_security(with_sacl: bool) -> Option<Box<PermissiveSecurity>> {
    use windows::Win32::Security::{
        AddMandatoryAce, InitializeAcl, InitializeSecurityDescriptor, SetSecurityDescriptorDacl,
        SetSecurityDescriptorSacl, ACE_FLAGS, ACL_REVISION, PSECURITY_DESCRIPTOR, PSID, SID,
        SID_IDENTIFIER_AUTHORITY,
    };

    unsafe {
        // Heap-allocated and pinned behind Box from the start: `attributes`
        // below stores a raw pointer into `descriptor`, and this struct used
        // to be built on the stack and returned BY VALUE. That return is a
        // move, so `security` (and `descriptor` with it) relocates to the
        // caller's stack slot while `lpSecurityDescriptor` kept pointing at
        // the pre-move address in this now-dead frame - a dangling pointer
        // handed straight to CreateMutexW/CreateEventW. This was silent
        // undefined behavior: it usually "worked" because the dead stack
        // memory was still readable garbage, but the descriptor it pointed
        // to was invalid, which could make Windows treat it as unset/default
        // security and let a second instance create (rather than open) the
        // mutex, defeating the single-instance check non-deterministically.
        // Boxing up front means `security`'s address never changes after
        // this point, so the pointer taken below stays valid for the Box's
        // lifetime.
        let mut security = Box::new(PermissiveSecurity {
            descriptor: windows::Win32::Security::SECURITY_DESCRIPTOR::default(),
            acl_storage: AlignedBytes([0u8; 64]),
            attributes: windows::Win32::Security::SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>()
                    as u32,
                lpSecurityDescriptor: std::ptr::null_mut(),
                bInheritHandle: false.into(),
            },
        });
        // SECURITY_DESCRIPTOR_REVISION = 1.
        if InitializeSecurityDescriptor(
            PSECURITY_DESCRIPTOR(&mut security.descriptor as *mut _ as *mut std::ffi::c_void),
            1,
        )
        .is_err()
        {
            return None;
        }
        // NULL DACL (pdacl = None with dacl-present): everyone is granted
        // everything, integrity checks aside.
        if SetSecurityDescriptorDacl(
            PSECURITY_DESCRIPTOR(&mut security.descriptor as *mut _ as *mut std::ffi::c_void),
            true,
            None,
            false,
        )
        .is_err()
        {
            return None;
        }
        if with_sacl {
            // Medium integrity label S-1-16-8192, built in place (a one-
            // subauthority SID: revision 1, label authority 16, RID 8192).
            let sid = SID {
                Revision: 1,
                SubAuthorityCount: 1,
                IdentifierAuthority: SID_IDENTIFIER_AUTHORITY {
                    Value: [0, 0, 0, 0, 0, 16],
                },
                SubAuthority: [8192],
            };
            let acl = security.acl_storage.0.as_mut_ptr() as *mut windows::Win32::Security::ACL;
            if InitializeAcl(acl, security.acl_storage.0.len() as u32, ACL_REVISION).is_err() {
                return None;
            }
            // SYSTEM_MANDATORY_LABEL_NO_WRITE_UP (1) is the standard policy
            // for an integrity label.
            if AddMandatoryAce(
                acl,
                ACL_REVISION,
                ACE_FLAGS(0),
                1,
                PSID(&sid as *const SID as *mut std::ffi::c_void),
            )
            .is_err()
            {
                return None;
            }
            if SetSecurityDescriptorSacl(
                PSECURITY_DESCRIPTOR(&mut security.descriptor as *mut _ as *mut std::ffi::c_void),
                true,
                Some(acl),
                false,
            )
            .is_err()
            {
                return None;
            }
        }
        security.attributes.lpSecurityDescriptor =
            &mut security.descriptor as *mut _ as *mut std::ffi::c_void;
        Some(security)
    }
}

// One CreateMutexW attempt with the permissive descriptor, falling back to a
// descriptor without the SACL. Returns the handle plus whether the mutex
// already existed, or None when the lock cannot be created at all (the caller
// then fails open).
//
// GetLastError() is captured immediately after each CreateMutexW call, with
// nothing in between: CreateMutexW returns a valid, non-null handle whether it
// created a NEW mutex or opened an EXISTING one, and ERROR_ALREADY_EXISTS is
// the only signal that distinguishes the two. Reading it after the match (as
// this used to) left room for other calls, including ones the windows crate's
// own Result handling can make, to overwrite the thread's last-error value
// first, which silently turned "already running" into "primary instance" and
// let duplicate windows open.
#[cfg(target_os = "windows")]
unsafe fn create_instance_mutex(
    name: windows::core::PCWSTR,
    with_sacl: Option<&PermissiveSecurity>,
) -> Option<(windows::Win32::Foundation::HANDLE, bool)> {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    let create_result = CreateMutexW(
        with_sacl.map(|security| &security.attributes as *const _),
        false,
        name,
    );
    if let Ok(mutex) = create_result {
        return Some((mutex, GetLastError() == ERROR_ALREADY_EXISTS));
    }
    let without_sacl = permissive_security(false);
    match CreateMutexW(
        without_sacl
            .as_ref()
            .map(|security| &security.attributes as *const _),
        false,
        name,
    ) {
        Ok(mutex) => Some((mutex, GetLastError() == ERROR_ALREADY_EXISTS)),
        Err(error) => {
            // Fail open: an unusable lock must not block startup.
            log::warn!(
                "single-instance mutex creation failed ({error}); continuing without a single-instance lock"
            );
            None
        }
    }
}

// Creates the named event the single-instance watcher thread waits on. Shared
// by the normal primary path and by a restart child that takes the lock over
// after a retry, so both set up exactly the same signal.
#[cfg(target_os = "windows")]
unsafe fn create_watcher_event(
    name: windows::core::PCWSTR,
    with_sacl: Option<&PermissiveSecurity>,
) {
    use windows::Win32::System::Threading::CreateEventW;

    match CreateEventW(
        with_sacl.map(|security| &security.attributes as *const _),
        true,
        false,
        name,
    ) {
        Ok(event) => {
            SINGLE_INSTANCE_EVENT.store(event.0 as usize, Ordering::Release);
        }
        Err(error) => {
            log::warn!(
                "single-instance event creation failed ({error}); running instances can still be detected but not woken"
            );
        }
    }
}

// Wakes the primary instance through the named event so it shows its window.
// Used when this process turns out to be a duplicate launch.
#[cfg(target_os = "windows")]
unsafe fn wake_running_instance(name: windows::core::PCWSTR) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{CreateEventW, SetEvent};

    match CreateEventW(None, true, false, name) {
        Ok(event) => {
            let _ = SetEvent(event);
            let _ = CloseHandle(event);
        }
        Err(_) => log::warn!("could not open single-instance event to wake the running instance"),
    }
}

// Returns true when this process is the primary instance and startup should
// continue; false when another instance already holds the lock (the caller
// must exit, the running instance has already been told to show itself).
// Runs before any Tauri/WebView2 initialization so a duplicate launch exits
// without flashing a window.
//
// Restart children (tray Restart, elevated UAC relaunch) pass restart_child
// true. Their old instance releases the lock explicitly, but the named mutex
// can still be alive for a moment after the exit code the child waited on
// became readable, because a process's kernel handles are closed after its
// exit code is set. Treating that window as "another instance is already
// running" is what closed the app with no replacement, so those children
// retry the lock instead of exiting, and only fall back to the duplicate
// launch path when something is still holding it after the timeout.
#[cfg(target_os = "windows")]
pub(crate) fn acquire_single_instance(identifier: &str, restart_child: bool) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;

    let mutex_w: Vec<u16> = format!("{identifier}-sim")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let event_w: Vec<u16> = format!("{identifier}-sie")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name = |text: &Vec<u16>| PCWSTR(text.as_ptr());

    unsafe {
        // Try with the Medium SACL first (needs SeSecurityPrivilege, which
        // only the elevated token holds). If that fails - e.g. a
        // non-elevated creator - retry without the SACL; a non-elevated
        // creator's objects default to the Medium label anyway.
        enable_security_privilege();
        let with_sacl = permissive_security(true);
        let deadline = std::time::Instant::now() + RESTART_LOCK_MAX_WAIT;
        let mut waiting_logged = false;
        loop {
            let Some((mutex, already_existed)) =
                create_instance_mutex(name(&mutex_w), with_sacl.as_deref())
            else {
                // Fail open: an unusable lock must not block startup.
                return true;
            };
            if !already_existed {
                // Primary instance: keep the mutex for the process lifetime
                // (the handle closes automatically when the process exits,
                // releasing the lock) and create the event the watcher thread
                // will wait on.
                SINGLE_INSTANCE_MUTEX.store(mutex.0 as usize, Ordering::Release);
                create_watcher_event(name(&event_w), with_sacl.as_deref());
                log::info!("single-instance lock acquired");
                return true;
            }
            // The mutex already exists: either a real second launch, or a
            // restart whose old instance has not finished releasing its
            // handles. The handle opened above belongs to this process now,
            // so close it before retrying.
            let _ = CloseHandle(mutex);
            if !restart_child || std::time::Instant::now() >= deadline {
                if restart_child {
                    log::error!(
                        "restart child could not take the single-instance lock within {:?}; treating this as a duplicate launch",
                        RESTART_LOCK_MAX_WAIT
                    );
                }
                let _ = wake_running_instance(name(&event_w));
                log::info!("another instance is already running; exiting");
                return false;
            }
            if !waiting_logged {
                log::info!(
                    "restart child waiting for the old instance to release the single-instance lock"
                );
                waiting_logged = true;
            }
            std::thread::sleep(RESTART_LOCK_RETRY_INTERVAL);
        }
    }
}

// Hands the single-instance lock over to a replacement process during a
// restart. The named mutex stays alive until every handle to it is closed, and
// that teardown lags the exit code the replacement waits on, so a replacement
// probing the lock in that window used to see ERROR_ALREADY_EXISTS and exit as
// a duplicate launch, leaving no app running. This instance no longer needs
// the lock once its replacement is spawned, so close it here and let the
// replacement take over instead of racing this process's own teardown.
#[cfg(target_os = "windows")]
pub(crate) fn release_single_instance() {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};

    let handle = SINGLE_INSTANCE_MUTEX.swap(0, Ordering::AcqRel);
    if handle != 0 {
        let _ = unsafe { CloseHandle(HANDLE(handle as *mut std::ffi::c_void)) };
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn release_single_instance() {}

#[cfg(not(target_os = "windows"))]
pub(crate) fn acquire_single_instance(_identifier: &str, _restart_child: bool) -> bool {
    true
}

// Spawned from setup() for the primary instance. Waits on the named event
// and brings the main window forward whenever another instance launches
// (mirrors what tauri-plugin-single-instance's callback did). The event is
// manual-reset, so a signal arriving before this thread's first wait is not
// lost: the wait returns immediately and the event is re-armed afterwards.
#[cfg(target_os = "windows")]
pub(crate) fn start_single_instance_watcher(app: AppHandle) {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{ResetEvent, WaitForSingleObject, INFINITE};

    let event = SINGLE_INSTANCE_EVENT.load(Ordering::Acquire);
    if event == 0 {
        // Not the primary instance (a restart child before it takes the lock
        // over) or the event could not be created.
        return;
    }
    let result = std::thread::Builder::new()
        .name("single-instance-watcher".into())
        .spawn(move || {
            // HANDLE wraps a raw pointer and is not Send; the usize value
            // is, so rebuild the handle inside the thread.
            let event = HANDLE(event as *mut std::ffi::c_void);
            loop {
                let wait = unsafe { WaitForSingleObject(event, INFINITE) };
                if wait == WAIT_OBJECT_0 {
                    crate::show_main_window(&app);
                    // Re-arm so the next launch is noticed too.
                    let _ = unsafe { ResetEvent(event) };
                } else {
                    // WAIT_FAILED (events are never abandoned): the handle
                    // is gone, nothing left to wait on.
                    log::warn!("single-instance watcher wait failed ({wait:?}); stopping");
                    break;
                }
            }
        });
    if let Err(error) = result {
        log::warn!("could not start single-instance watcher thread: {error}");
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn start_single_instance_watcher(_app: AppHandle) {}

// How long a restart/update child waits for the old instance to exit before
// declaring it stuck. Bounded so a hung teardown (WebView2, a driver call)
// cannot block the restart or update forever.
pub(crate) const RESTART_PARENT_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

// When this instance was spawned by a tray Restart, --restart-parent carries
// the old process's PID. Wait for it to fully exit before creating the
// WebView2 environment. A vanished parent is success; a live parent remains
// polled with low-frequency warnings until it exits. Also used by the
// updater's --apply-update mode to wait for the old process to release the
// exe file before swapping it.
//
// The wait is bounded (RESTART_PARENT_MAX_WAIT). If the old instance is
// still alive past it, the teardown is stuck, and waiting any longer only
// guarantees the restart never completes. The old instance is our own exe,
// and the watchdog children make a hard kill survivable (the gamma watchdog
// restores the ramp, the display-mode watchdog restores the mode), so
// terminate it and let the replacement proceed. Returns false only when the
// parent's state cannot be inspected or it survives termination, in which
// case the caller must abort rather than compete with it.
#[cfg(target_os = "windows")]
pub(crate) fn wait_for_restart_parent(parent_pid: u32) -> bool {
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, STILL_ACTIVE,
    };
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, TerminateProcess, WaitForSingleObject,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };
    let deadline = std::time::Instant::now() + RESTART_PARENT_MAX_WAIT;
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, parent_pid) {
            Ok(handle) => handle,
            Err(error) => {
                // ERROR_INVALID_PARAMETER means the PID no longer exists;
                // other failures (for example, an access restriction) do not
                // prove that the parent released the WebView2 profile.
                if GetLastError() == ERROR_INVALID_PARAMETER {
                    log::info!("restart parent {parent_pid} is already gone");
                    return true;
                }
                log::error!(
                    "could not inspect restart parent {parent_pid}: {error}; aborting replacement"
                );
                return false;
            }
        };
        let mut last_warning = std::time::Instant::now();
        loop {
            let mut exit_code = 0u32;
            if GetExitCodeProcess(handle, &mut exit_code).is_err() {
                let _ = CloseHandle(handle);
                log::error!(
                    "could not read restart parent {parent_pid} status; aborting replacement"
                );
                return false;
            }
            if exit_code != STILL_ACTIVE.0 as u32 {
                let _ = CloseHandle(handle);
                // Milestone for diagnosing restarts: the wait is over. Its
                // kernel handles can still be closing for a moment after the
                // exit code appears, which is why the replacement retries the
                // single-instance lock instead of trusting this point alone.
                log::info!("restart parent {parent_pid} has exited; replacement proceeding");
                return true;
            }
            if std::time::Instant::now() >= deadline {
                log::error!(
                    "restart parent {parent_pid} did not exit within {:?}; terminating it to complete the restart",
                    RESTART_PARENT_MAX_WAIT
                );
                let terminate_handle = match OpenProcess(
                    PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                    false,
                    parent_pid,
                ) {
                    Ok(handle) => handle,
                    Err(error) => {
                        let _ = CloseHandle(handle);
                        log::error!(
                            "could not open restart parent {parent_pid} for termination: {error}; aborting replacement"
                        );
                        return false;
                    }
                };
                let terminated = TerminateProcess(terminate_handle, 1).is_ok();
                let _ = CloseHandle(terminate_handle);
                if !terminated {
                    let _ = CloseHandle(handle);
                    log::error!(
                        "could not terminate the stuck restart parent {parent_pid}; aborting replacement"
                    );
                    return false;
                }
                // Wait for the terminated process to fully exit: its handles,
                // including the single-instance mutex, close only then.
                let _ = WaitForSingleObject(handle, 5000);
                let _ = CloseHandle(handle);
                log::warn!(
                    "restart parent {parent_pid} was terminated after the shutdown timeout to complete the restart"
                );
                return true;
            }
            if last_warning.elapsed() >= std::time::Duration::from_secs(5) {
                log::warn!("restart parent {parent_pid} is still shutting down; replacement remains waiting");
                last_warning = std::time::Instant::now();
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}
