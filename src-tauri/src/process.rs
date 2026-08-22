//! Process lifecycle: elevation detection, UAC relaunch, restart handoff, and
//! external-link opening. Command-line flag names are centralized here so the
//! argument builders (restart/elevate) and parsers (run) cannot drift.

use tauri::AppHandle;

// Command-line flags shared by the restart/elevation paths and parsed by run().
pub(crate) const ARG_RESTART_PARENT: &str = "--restart-parent";
pub(crate) const ARG_OPEN_TAB_PREFIX: &str = "--open-tab=";
pub(crate) const ARG_RTX_HDR_FLAGS_PREFIX: &str = "--rtx-hdr-flags=";
pub(crate) const ARG_RTX_HDR_ENABLED_PREFIX: &str = "--rtx-hdr-enabled=";
pub(crate) const ARG_HDR_MODE_PREFIX: &str = "--hdr-mode=";
pub(crate) const ARG_SCREEN_DIMMER_ENABLED_PREFIX: &str = "--screen-dimmer-enabled=";

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
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
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
    // are failures (same convention as relaunch_elevated above).
    let code = result.0 as isize;
    if code <= 32 {
        return Err(format!("could not open link (error code {code})"));
    }
    Ok(())
}

// The native power-request list and powercfg /requests both require admin
// rights on current Windows builds. The Screen Dimmer uses this check before
// deciding whether to request a UAC relaunch.
#[cfg(target_os = "windows")]
pub(crate) fn process_is_elevated() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS, TOKEN_ACCESS_MASK,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ACCESS_MASK(0x0008),
            &mut token,
        )
        .is_err()
        {
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

#[tauri::command]
pub fn is_process_elevated() -> bool {
    process_is_elevated()
}

// Launches a copy of the current executable elevated (ShellExecuteW "runas",
// UAC prompt). Forwards the current command-line arguments plus
// --restart-parent <pid> so the replacement waits for this process to exit
// (the same handoff the in-app relaunch uses), then appends the given extra
// args. Returns Err on failure or when the user dismisses the UAC prompt -
// the caller keeps running in that case.
pub(crate) fn spawn_elevated(extra_args: &[String]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
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

// RTX HDR and Screen Dimmer request elevation through the same UAC relaunch
// path. The pending operation is forwarded to the replacement process.
#[tauri::command]
pub fn relaunch_elevated(
    app: AppHandle,
    rtx_hdr_flags: Option<u32>,
    rtx_hdr_enabled: Option<bool>,
    hdr_mode: Option<String>,
    screen_dimmer_enabled: Option<bool>,
    tab: Option<String>,
) -> Result<(), String> {
    let mut extra: Vec<String> = Vec::new();
    if let Some(flags) = rtx_hdr_flags {
        extra.push(format!("{ARG_RTX_HDR_FLAGS_PREFIX}{flags}"));
    }
    if let Some(enabled) = rtx_hdr_enabled {
        extra.push(format!("{ARG_RTX_HDR_ENABLED_PREFIX}{}", u8::from(enabled)));
    }
    if let Some(mode) = hdr_mode
        .as_deref()
        .filter(|mode| matches!(*mode, "off" | "auto" | "rtx"))
    {
        extra.push(format!("{ARG_HDR_MODE_PREFIX}{mode}"));
    }
    if let Some(enabled) = screen_dimmer_enabled {
        extra.push(format!("{ARG_SCREEN_DIMMER_ENABLED_PREFIX}{}", u8::from(enabled)));
    }
    // Only forward tab ids the app actually has; anything else is ignored
    // rather than carried into the command line.
    if let Some(tab) = tab {
        if KNOWN_TABS.contains(&tab.as_str()) {
            extra.push(format!("{ARG_OPEN_TAB_PREFIX}{tab}"));
        }
    }
    spawn_elevated(&extra)?;
    crate::taskbar_dimmer::shutdown();
    crate::shutdown_gamma_worker(&app);
    app.exit(0);
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
    use tauri::Manager;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        FlashWindowEx, FLASHWINFO, FLASHW_ALL, FLASHW_TIMERNOFG, HWND_NOTOPMOST, HWND_TOPMOST,
        IsWindowVisible, SetForegroundWindow, SetWindowPos, ShowWindow, SWP_NOMOVE, SWP_NOSIZE,
        SWP_SHOWWINDOW, SW_RESTORE,
    };

    let Some(window) = app.get_webview_window("main") else {
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

#[cfg(target_os = "windows")]
static SINGLE_INSTANCE_MUTEX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
#[cfg(target_os = "windows")]
static SINGLE_INSTANCE_EVENT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

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
    if LookupPrivilegeValueW(windows::core::PCWSTR::null(), SE_SECURITY_NAME, &mut luid).is_err()
    {
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

// Returns true when this process is the primary instance and startup should
// continue; false when another instance already holds the lock (the caller
// must exit, the running instance has already been told to show itself).
// Runs before any Tauri/WebView2 initialization so a duplicate launch exits
// without flashing a window. Skipped entirely by restart children: the old
// instance is still alive mid-shutdown and still holds the lock, so a
// restart child taking it would see "already running" and exit instead of
// taking over (the same reason the plugin was skipped for them).
#[cfg(target_os = "windows")]
pub(crate) fn acquire_single_instance(identifier: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent};

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
        // GetLastError() must be captured immediately after each CreateMutexW
        // call, with nothing in between: CreateMutexW returns a valid,
        // non-null handle whether it created a NEW mutex or opened an
        // EXISTING one, and ERROR_ALREADY_EXISTS is the only signal that
        // distinguishes the two. Reading it after the match (as this used
        // to) left room for other calls - including ones the windows
        // crate's own Result handling can make - to overwrite the thread's
        // last-error value first, which silently turned "already running"
        // into "primary instance" and let duplicate windows open.
        let create_result = CreateMutexW(
            with_sacl.as_ref().map(|s| &s.attributes as *const _),
            false,
            name(&mutex_w),
        );
        let mut already_existed = GetLastError() == ERROR_ALREADY_EXISTS;
        let mutex = match create_result {
            Ok(mutex) => mutex,
            Err(_) => {
                let without_sacl = permissive_security(false);
                let create_result = CreateMutexW(
                    without_sacl.as_ref().map(|s| &s.attributes as *const _),
                    false,
                    name(&mutex_w),
                );
                already_existed = GetLastError() == ERROR_ALREADY_EXISTS;
                match create_result {
                    Ok(mutex) => mutex,
                    Err(error) => {
                        // Fail open: an unusable lock must not block startup.
                        log::warn!(
                            "single-instance mutex creation failed ({error}); continuing without a single-instance lock"
                        );
                        return true;
                    }
                }
            }
        };
        if already_existed {
            // Second instance: wake the primary through the named event,
            // then exit without creating any window.
            if let Ok(event) = CreateEventW(None, true, false, name(&event_w)) {
                let _ = SetEvent(event);
                let _ = CloseHandle(event);
            } else {
                log::warn!("could not open single-instance event to wake the running instance");
            }
            let _ = CloseHandle(mutex);
            log::info!("another instance is already running; exiting");
            return false;
        }
        // Primary instance: keep the mutex for the process lifetime (the
        // handle closes automatically when the process exits, releasing the
        // lock) and create the event the watcher thread will wait on.
        let _ = SINGLE_INSTANCE_MUTEX.set(mutex.0 as usize);
        let event = match CreateEventW(
            with_sacl.as_ref().map(|s| &s.attributes as *const _),
            true,
            false,
            name(&event_w),
        ) {
            Ok(event) => event,
            Err(error) => {
                log::warn!(
                    "single-instance event creation failed ({error}); running instances can still be detected but not woken"
                );
                HANDLE::default()
            }
        };
        let _ = SINGLE_INSTANCE_EVENT.set(event.0 as usize);
    }
    log::info!("single-instance lock acquired");
    true
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn acquire_single_instance(_identifier: &str) -> bool {
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

    let Some(event) = SINGLE_INSTANCE_EVENT.get().copied() else {
        return; // not the primary instance (restart child) or event failed
    };
    if HANDLE(event as *mut std::ffi::c_void).is_invalid() {
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

// When this instance was spawned by a tray Restart, --restart-parent carries
// the old process's PID. Wait for it to fully exit before creating the
// WebView2 environment. A vanished parent is success; a live parent remains
// polled with low-frequency warnings until it exits. Also used by the
// updater's --apply-update mode to wait for the old process to release the
// exe file before swapping it.
#[cfg(target_os = "windows")]
pub(crate) fn wait_for_restart_parent(parent_pid: u32) -> bool {
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, STILL_ACTIVE,
    };
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
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
