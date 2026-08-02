mod gamma_ramp;
mod window_icon;
mod display_info;
mod config;
mod taskbar_dimmer;

use std::sync::{atomic::{AtomicBool, Ordering}, Mutex};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State,
};

struct AppState {
    black_lift: Mutex<i32>,
}

struct TrayState {
    enabled: AtomicBool,
}

#[tauri::command]
fn apply_gamma_ramp(state: State<AppState>, black_lift: i32) -> Result<(), String> {
    let ramp = gamma_ramp::build_ramp(black_lift);
    unsafe {
        if !gamma_ramp::set_display_ramp(&ramp) {
            return Err("SetDeviceGammaRamp failed".into());
        }
    }
    *state.black_lift.lock().unwrap() = black_lift;
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

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.show().is_ok() {
            let _ = window.set_focus();
            let _ = app.emit("window-visibility", true);
        }
    }
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
        .manage(AppState {
            black_lift: Mutex::new(0),
        })
        .manage(taskbar_dimmer::TaskbarDimmerState::default())
        .manage(TrayState {
            enabled: AtomicBool::new(true),
        })
        .invoke_handler(tauri::generate_handler![
            apply_gamma_ramp,
            log_frontend,
            get_display_info,
            set_tray_enabled,
            config::load_config,
            config::save_config,
            taskbar_dimmer::set_taskbar_dimmer,
            taskbar_dimmer::set_taskbar_dimmer_verbose_logging,
            taskbar_dimmer::get_taskbar_dimmer_status,
        ])
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
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
                    "quit" => app.exit(0),
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
                tauri::WindowEvent::Destroyed => unsafe {
                    gamma_ramp::reset_display_ramp();
                },
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
