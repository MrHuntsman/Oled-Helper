mod gamma_ramp;
mod window_icon;
mod display_info;
mod config;
mod taskbar_dimmer;
mod hdr_toggle;

use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State,
};

struct TrayState {
    enabled: AtomicBool,
}

// Gamma state — synchronous apply + periodic safety restore.
// The frontend calls apply_gamma_ramp on every slider change; the ramp is
// applied immediately (no polling thread, no IPC round-trip delay). A
// lightweight safety thread re-applies every 2 seconds so another app
// resetting the system gamma (fullscreen games, driver updates) is
// corrected without user interaction.
struct GammaState {
    desired: Arc<AtomicI32>,
    shutdown: Arc<AtomicBool>,
    safety_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for GammaState {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Ok(mut guard) = self.safety_thread.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
        unsafe { gamma_ramp::reset_display_ramp(); }
    }
}

fn gamma_safety_loop(desired: Arc<AtomicI32>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_secs(2));
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let value = desired.load(Ordering::Relaxed);
        // When black lift is zero the system ramp is linear (OS default) —
        // nothing to restore. Skip the GDI call to keep the thread truly idle
        // while black crush is disabled or set to OFF.
        if value == 0 {
            continue;
        }
        let ramp = gamma_ramp::build_ramp(value);
        unsafe { gamma_ramp::set_display_ramp(&ramp); }
    }
}

// Applies the gamma ramp synchronously — the display update happens on the
// caller's thread (the Tauri command thread) and returns immediately. No
// background polling, no IPC delay. A separate safety thread re-applies at
// 2-second intervals to recover from external gamma resets.
#[tauri::command]
fn apply_gamma_ramp(state: State<GammaState>, black_lift: i32) -> Result<(), String> {
    let ramp = gamma_ramp::build_ramp(black_lift);
    let applied = unsafe { gamma_ramp::set_display_ramp(&ramp) };
    if !applied {
        return Err("SetDeviceGammaRamp failed".into());
    }
    state.desired.store(black_lift, Ordering::Relaxed);
    Ok(())
}

// Bridge for frontend console/log lines and uncaught JS errors, so the
// on-disk log captures what happened even if the app hangs or crashes
// before the user can open DevTools or the in-app Diagnostics tab.
#[tauri::command]
fn log_frontend(level: String, message: String) {
    match level.as_str() {
        "error" => log::error!("[frontend] {message}"),
        "warn" => log::warn!("[frontend] {message}"),
        _ => log::info!("[frontend] {message}"),
    }
}

// Wipes the WebView2 HTTP cache (and other stored web data — this app uses
// none). Tray restarts on older builds hard-killed the process mid-write
// (Tauri's app.restart() calls std::process::exit(0)), which corrupted cache
// entries for some embedded icons; the corrupt responses were then served as
// broken images on the next launch. The pre-build purge (purge_webview_cache)
// is the primary mechanism — this command is a fallback that clears through
// the WebView2 API itself, so it works even if the purge's hardcoded path
// ever stops matching. Called by the frontend a couple of seconds after
// mount, after the (healthy) initial page load has settled.
#[tauri::command]
fn clear_webview_cache(app: tauri::AppHandle) -> Result<(), String> {
    match app.get_webview_window("main") {
        Some(window) => window
            .clear_all_browsing_data()
            .map_err(|error| error.to_string()),
        None => Err("main window not found".into()),
    }
}

#[tauri::command]
fn get_display_info() -> Option<display_info::DisplayInfo> {
    display_info::query_current_display()
}

#[tauri::command]
fn set_refresh_rate(hz: u32) -> Result<(), String> {
    display_info::set_refresh_rate(hz)
}

#[tauri::command]
fn set_resolution(width: u32, height: u32, preferred_hz: u32) -> Result<(), String> {
    display_info::set_resolution(width, height, preferred_hz)
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.show().is_ok() {
            let _ = window.set_focus();
            let _ = app.emit("window-visibility", true);
        }
    }
}

fn shutdown_gamma_worker(app: &AppHandle) {
    let state = app.state::<GammaState>();
    state.shutdown.store(true, Ordering::Release);
    if let Ok(mut guard) = state.safety_thread.lock() {
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
    }
    unsafe { gamma_ramp::reset_display_ramp(); }
    config::shutdown();
}

fn request_shutdown_and_exit(app: &AppHandle) {
    taskbar_dimmer::shutdown();
    shutdown_gamma_worker(app);
    app.exit(0);
}

fn request_restart(app: &AppHandle) {
    // In dev mode the app runs under `tauri dev`, which owns the vite dev
    // server. Exiting this process tears the whole dev environment down
    // (vite included), so a spawned replacement instance would be left with
    // no dev server — the page can't even load, and any asset requested
    // after the teardown renders as a broken icon. Reload the webview
    // instead: the frontend fully re-initializes (config reload, dimmer and
    // gamma re-sync) while the Rust backend and the dev server stay alive.
    if tauri::is_dev() {
        log::info!("dev-mode restart: reloading webview (process stays alive so the dev server survives)");
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.reload();
        }
        return;
    }

    // Production path: real process restart.
    taskbar_dimmer::shutdown();
    shutdown_gamma_worker(app);
    // Tauri's app.restart() spawns the new process and then hard-exits via
    // std::process::exit(0), which kills WebView2 mid-write. The abrupt
    // termination corrupts the WebView2 disk cache in the shared user-data
    // folder, and on the next launch the webview serves the corrupt cached
    // responses — the broken icon images after a tray restart. Spawn the
    // replacement instance first, then exit through the event loop
    // (app.exit) so WebView2 flushes its cache and releases the user-data
    // folder before the new process opens it. The child is told which PID to
    // wait for (--restart-parent) so it never starts WebView2 while the old
    // process still holds the user-data folder or cache files.
    if let Ok(exe) = std::env::current_exe() {
        let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        args.push("--restart-parent".into());
        args.push(std::process::id().to_string().into());
        if let Err(e) = std::process::Command::new(exe).args(args).spawn() {
            log::error!("failed to spawn replacement instance: {e}");
        }
    }
    app.exit(0);
}

#[tauri::command]
fn set_tray_enabled(
    app: tauri::AppHandle,
    state: State<TrayState>,
    enabled: bool,
) -> Result<(), String> {
    let tray = app
        .tray_by_id("main-tray")
        .ok_or_else(|| "system tray icon is not available".to_string())?;
    tray.set_visible(enabled).map_err(|error| error.to_string())?;
    state.enabled.store(enabled, Ordering::Relaxed);
    Ok(())
}

// When this instance was spawned by a tray Restart, --restart-parent carries
// the old process's PID. Wait for it to fully exit (bounded, ~5 s) before
// creating the WebView2 environment: starting the new instance while the old
// one still holds the user-data folder makes WebView2 attach to the old
// process's browser process, which is being torn down — in-flight asset
// requests then abort and some images render as broken icons. Polling with
// GetExitCodeProcess is cheaper and more reliable than waiting on a named
// event, and it also fails safe (if the parent is already gone, OpenProcess
// errors and we proceed immediately).
#[cfg(target_os = "windows")]
fn wait_for_restart_parent(parent_pid: u32) {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, parent_pid) else {
            return; // parent already gone — nothing to wait for
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mut exit_code = 0u32;
            let _ = GetExitCodeProcess(handle, &mut exit_code);
            if exit_code != STILL_ACTIVE.0 as u32 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                log::warn!("restart parent {parent_pid} did not exit within 5s — continuing anyway");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = CloseHandle(handle);
    }
}

// Deletes the WebView2 HTTP disk cache before the webview starts. Tray
// restarts on older builds hard-killed the process mid-write (Tauri's
// app.restart() → std::process::exit(0)), corrupting cache entries for some
// embedded icons; on the next launch WebView2 served the corrupt responses
// and those icons rendered as the broken-image logo. The cache has no value
// here — every asset is embedded in the binary or served fresh by the dev
// server — so purge it once per launch to guarantee a stale entry can never
// be served. Runs before tauri::Builder is built, so WebView2 isn't running
// and no cache file is locked.
#[cfg(target_os = "windows")]
fn purge_webview_cache() {
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("APPDATA"))
        .unwrap_or_default();
    if base.is_empty() {
        return;
    }
    // Default WebView2 user-data folder for the "OledHelper" identifier:
    // %LOCALAPPDATA%\OledHelper\EBWebView\Default\Cache. NOTE: this path
    // depends on the app identifier in tauri.conf.json — if it is ever
    // renamed, the purge silently no-ops (NotFound is swallowed) and the
    // frontend clear_webview_cache fallback still covers the cleanup.
    let cache_dir = std::path::Path::new(&base)
        .join("OledHelper")
        .join("EBWebView")
        .join("Default")
        .join("Cache");
    match std::fs::remove_dir_all(&cache_dir) {
        Ok(()) => log::info!("purged WebView2 disk cache at {}", cache_dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("could not purge WebView2 cache: {e}"),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Tray-restart child: let the old instance release the user-data folder
    // and cache files before we start WebView2.
    #[cfg(target_os = "windows")]
    {
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        if let Some(pos) = args.iter().position(|arg| arg == "--restart-parent") {
            if let Some(pid) = args
                .get(pos + 1)
                .and_then(|arg| arg.to_str())
                .and_then(|text| text.parse::<u32>().ok())
            {
                wait_for_restart_parent(pid);
            }
        }
        purge_webview_cache();
    }

    tauri::Builder::default()
        .manage(GammaState {
            desired: Arc::new(AtomicI32::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
            safety_thread: Mutex::new(None),
        })
        .manage(taskbar_dimmer::TaskbarDimmerState::default())
        .manage(TrayState {
            enabled: AtomicBool::new(true),
        })
        .invoke_handler(tauri::generate_handler![
            apply_gamma_ramp,
            log_frontend,
            clear_webview_cache,
            get_display_info,
            set_refresh_rate,
            set_resolution,
            set_tray_enabled,
            config::load_config,
            config::save_config,
            config::reset_config,
            taskbar_dimmer::set_taskbar_dimmer,
            taskbar_dimmer::set_taskbar_dimmer_verbose_logging,
            taskbar_dimmer::set_taskbar_dimmer_detection,
            taskbar_dimmer::set_taskbar_dimmer_suspended,
            taskbar_dimmer::set_taskbar_dimmer_hover_mode,
            taskbar_dimmer::set_taskbar_dimmer_poll_interval,
            taskbar_dimmer::get_taskbar_dimmer_status,
            hdr_toggle::get_windows_hdr_status,
            hdr_toggle::set_windows_hdr_enabled,
            hdr_toggle::get_hdr_luminance,
        ])
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // Gamma safety thread: re-applies the last known ramp every 2
            // seconds so another app resetting the system gamma (fullscreen
            // games, driver mode switches) is corrected automatically.
            {
                let state = app.state::<GammaState>();
                let desired = state.desired.clone();
                let shutdown = state.shutdown.clone();
                let handle = std::thread::Builder::new()
                    .name("gamma-safety".into())
                    .spawn(move || gamma_safety_loop(desired, shutdown))
                    .map_err(|error| format!("could not start gamma safety thread: {error}"))?;
                state
                    .safety_thread
                    .lock()
                    .map_err(|_| "gamma state was poisoned".to_string())?
                    .replace(handle);
            }

            // Always-on rotating file log (not just debug builds) — GPU-init
            // hangs/crashes can happen in release builds too, and without a
            // persistent log the only evidence is whatever DevTools happened
            // to be open and scrolled to at the time. See log_frontend above
            // and the console bridge in main.js for how JS-side output gets in.
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    .max_file_size(5_000_000)
                    .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepAll)
                    .build(),
            )?;

            // Let display_info's EDID diagnostics reach the Runtime Log (the
            // Debug tab mirrors the "runtime-log" event in main.js).
            display_info::init_app_handle(app.handle().clone());

            let saved_config = config::load_config(app.handle().clone());
            app.state::<TrayState>()
                .enabled
                .store(saved_config.tray_enabled, Ordering::Relaxed);

            let show_item = MenuItem::with_id(app, "show", "Show OLED Helper", true, None::<&str>)?;
            let restart_item = MenuItem::with_id(app, "restart", "Restart", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_item, &restart_item, &quit_item])?;
            let tray_image = window_icon::crisp_tray_image()
                .or_else(|| app.default_window_icon().cloned())
                .ok_or("missing default app icon")?;

            TrayIconBuilder::with_id("main-tray")
                .icon(tray_image)
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "restart" => request_restart(app),
                    "quit" => request_shutdown_and_exit(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            if let Some(tray) = app.tray_by_id("main-tray") {
                tray.set_visible(saved_config.tray_enabled)?;
            }

            let dimmer_state = app.state::<taskbar_dimmer::TaskbarDimmerState>();
            if let Err(error) = taskbar_dimmer::create_overlay(app.handle(), &dimmer_state) {
                log::warn!("taskbar dimmer overlay unavailable: {error}");
            }

            // Tauri's runtime window icon (titlebar/taskbar while running) is blurry
            // on Windows — it sets HICON from raw RGBA instead of reading per-size
            // frames from icon.ico like Explorer does. Fix it manually.
            #[cfg(target_os = "windows")]
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(hwnd) = window.hwnd() {
                    let hwnd_raw = hwnd.0 as isize;
                    window_icon::apply_crisp_window_icon(hwnd_raw);
                    // WebView2 initialization can reset the icon shortly after; reapply once more.
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        window_icon::apply_crisp_window_icon(hwnd_raw);
                    });
                }
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                    let tray_enabled = window
                        .app_handle()
                        .state::<TrayState>()
                        .enabled
                        .load(Ordering::Relaxed);
                    if tray_enabled {
                        api.prevent_close();
                        if window.hide().is_ok() {
                            let _ = window.app_handle().emit("window-visibility", false);
                        }
                    }
                }
                tauri::WindowEvent::Destroyed => {
                    // Closing with the tray enabled only hides the window. When
                    // the window is actually destroyed, request a synchronous
                    // worker shutdown so the display ramp is reset before the
                    // process exits.
                    taskbar_dimmer::shutdown();
                    shutdown_gamma_worker(window.app_handle());
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
