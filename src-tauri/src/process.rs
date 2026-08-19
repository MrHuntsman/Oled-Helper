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
