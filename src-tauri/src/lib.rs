mod gamma_ramp;
mod window_icon;
mod display_info;
mod config;
mod taskbar_dimmer;
mod hdr_toggle;

use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc,
};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State,
};

struct TrayState {
    enabled: AtomicBool,
}

// Gamma state shared between the webview command and the dedicated worker
// thread. The frontend only ever writes the latest desired black level
// (fire-and-forget, never waiting for a response), and the worker thread
// continuously applies it. This keeps the display update on the native side
// and out of WebView2's IPC round-trip, which is slow under sustained input.
struct GammaState {
    inner: Arc<GammaInner>,
}

struct GammaInner {
    desired: AtomicI32,
    shutdown: AtomicBool,
    last_applied: AtomicI32,
}

impl Default for GammaState {
    fn default() -> Self {
        Self {
            inner: Arc::new(GammaInner {
                desired: AtomicI32::new(0),
                shutdown: AtomicBool::new(false),
                last_applied: AtomicI32::new(i32::MIN),
            }),
        }
    }
}

fn gamma_worker_loop(state: &GammaInner) {
    // Short-sleep polling loop: applies the latest desired value within a few
    // milliseconds of the frontend writing it, without depending on WebView2
    // message delivery back to JS. `desired` is only read here, so the loop
    // runs independently of any IPC latency.
    while !state.shutdown.load(Ordering::Relaxed) {
        let desired = state.desired.load(Ordering::Relaxed);
        let last = state.last_applied.load(Ordering::Relaxed);
        if desired != last {
            let started = std::time::Instant::now();
            let ramp = gamma_ramp::build_ramp(desired);
            let applied = unsafe { gamma_ramp::set_display_ramp(&ramp) };
            if applied {
                state.last_applied.store(desired, Ordering::Relaxed);
            }
            let total_ms = started.elapsed().as_secs_f64() * 1000.0;
            if !applied {
                log::warn!("[gamma] value={} apply failed", desired);
            } else if total_ms >= 32.0 {
                log::info!("[gamma-slow] value={} apply={total_ms:.3}ms", desired);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
    unsafe { gamma_ramp::reset_display_ramp(); }
}

// Fire-and-forget: stores the latest value and returns immediately. The
// worker applies it on its own thread. No response is waited on, so WebView2's
// slow return path can never delay the display update.
#[tauri::command]
fn apply_gamma_ramp(state: State<GammaState>, black_lift: i32) -> Result<(), String> {
    state.inner.desired.store(black_lift, Ordering::Relaxed);
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
        .manage(GammaState::default())
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
            taskbar_dimmer::set_taskbar_dimmer_suspended,
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
            // Gamma worker: continuously applies the latest desired black
            // level so display updates never wait on WebView2 IPC. Runs at a
            // boosted priority so it stays responsive while the renderer is
            // busy processing drag input.
            {
                let gamma = app.state::<GammaState>().inner.clone();
                std::thread::Builder::new()
                    .name("gamma-ramp-worker".into())
                    .spawn(move || {
                        #[cfg(target_os = "windows")]
                        unsafe {
                            // Raise this thread above normal so it is scheduled
                            // promptly even while the renderer burns CPU.
                            use windows::Win32::System::Threading::{
                                GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
                            };
                            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
                        }
                        gamma_worker_loop(&gamma);
                    })
                    .map_err(|error| format!("could not start gamma worker: {error}"))?;
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
                tauri::WindowEvent::Destroyed => {
                    let gamma = window.app_handle().state::<GammaState>();
                    gamma.inner.shutdown.store(true, Ordering::Relaxed);
                    // The worker resets the display ramp itself after observing
                    // shutdown, so no race with an in-progress apply.
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
