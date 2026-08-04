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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
            get_display_info,
            set_refresh_rate,
            set_resolution,
            set_tray_enabled,
            config::load_config,
            config::save_config,
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

            let saved_config = config::load_config(app.handle().clone());
            app.state::<TrayState>()
                .enabled
                .store(saved_config.tray_enabled, Ordering::Relaxed);

            let show_item = MenuItem::with_id(app, "show", "Show OLED Helper", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_item, &quit_item])?;
            let tray_image = window_icon::crisp_tray_image()
                .or_else(|| app.default_window_icon().cloned())
                .ok_or("missing default app icon")?;

            TrayIconBuilder::with_id("main-tray")
                .icon(tray_image)
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
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
