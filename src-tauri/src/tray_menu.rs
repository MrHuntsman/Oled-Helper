// tray_menu.rs — Native dark-mode system tray context menu.
//
// Uses Tauri 2's Menu/Submenu/CheckMenuItem API (backed by muda on Windows)
// so the popup is a real Win32 TrackPopupMenu — no custom WebView, no
// hit-testing hacks, no rendering artifacts. Dark mode is forced process-wide
// via the undocumented uxtheme.dll SetPreferredAppMode ordinal.

use std::sync::OnceLock;
use tauri::menu::{
    CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::AppHandle;

static DARK_MODE_INIT: OnceLock<()> = OnceLock::new();

/// Initialize Windows 11 dark mode for native menus.
pub fn init_dark_mode() {
    DARK_MODE_INIT.get_or_init(|| {
        #[cfg(target_os = "windows")]
        unsafe {
            use windows::core::PCSTR;
            use windows::Win32::Foundation::FreeLibrary;
            use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

            if let Ok(m) = LoadLibraryW(windows::core::w!("uxtheme.dll")) {
                if let Some(ptr) = GetProcAddress(m, PCSTR(135 as _)) {
                    type Fn = unsafe extern "system" fn(i32) -> i32;
                    let f: Fn = std::mem::transmute(ptr);
                    f(2); // ForceDark
                }
                if let Some(ptr) = GetProcAddress(m, PCSTR(136 as _)) {
                    type Fn = unsafe extern "system" fn();
                    let f: Fn = std::mem::transmute(ptr);
                    f(); // FlushMenuThemes
                }
                let _ = FreeLibrary(m);
            }
        }
    });
}

/// Build the full tray context menu from current display info and favorites.
pub fn build_menu(app: &AppHandle) -> Result<tauri::menu::Menu<tauri::Wry>, String> {
    let display = crate::display_info::query_current_display()
        .ok_or_else(|| "could not query display info".to_string())?;
    let config = crate::config::load_config(app.clone());

    let mut menu = MenuBuilder::new(app);

    // Resolution submenu
    {
        let mut sub = SubmenuBuilder::new(app, "Resolution");
        for res in &display.resolutions {
            let id = format!("tray:res:{}x{}", res.width, res.height);
            let label = format!("{}×{}", res.width, res.height);
            let checked = res.width == display.width && res.height == display.height;
            sub = sub.item(
                &CheckMenuItemBuilder::with_id(&id, &label)
                    .checked(checked)
                    .build(app)
                    .map_err(|e| e.to_string())?,
            );
        }
        menu = menu.item(&sub.build().map_err(|e| e.to_string())?);
    }

    // Refresh Rate submenu
    {
        let mut sub = SubmenuBuilder::new(app, "Refresh Rate");
        for &hz in &display.refresh_rates {
            let id = format!("tray:hz:{hz}");
            let label = format!("{hz} Hz");
            let checked = hz == display.refresh_hz;
            sub = sub.item(
                &CheckMenuItemBuilder::with_id(&id, &label)
                    .checked(checked)
                    .build(app)
                    .map_err(|e| e.to_string())?,
            );
        }
        menu = menu.item(&sub.build().map_err(|e| e.to_string())?);
    }

    // Favorites submenu
    {
        let mut sub = SubmenuBuilder::new(app, "Favorites");
        for (i, fav) in config.favorite_display_modes.iter().enumerate() {
            let id = format!("tray:fav:{i}");
            let label = format!("{}×{} @ {} Hz", fav.width, fav.height, fav.hz);
            sub = sub.item(
                &MenuItemBuilder::with_id(&id, &label)
                    .build(app)
                    .map_err(|e| e.to_string())?,
            );
        }
        if !config.favorite_display_modes.is_empty() {
            sub = sub.item(&PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?);
        }
        sub = sub.item(
            &MenuItemBuilder::with_id("tray:fav:add", "Add Current")
                .build(app)
                .map_err(|e| e.to_string())?,
        );
        sub = sub.item(
            &MenuItemBuilder::with_id("tray:fav:clear", "Clear All")
                .build(app)
                .map_err(|e| e.to_string())?,
        );
        menu = menu.item(&sub.build().map_err(|e| e.to_string())?);
    }

    menu = menu.item(&PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?);
    menu = menu.item(
        &MenuItemBuilder::with_id("tray:restart", "Restart")
            .build(app)
            .map_err(|e| e.to_string())?,
    );
    menu = menu.item(
        &MenuItemBuilder::with_id("tray:quit", "Quit")
            .build(app)
            .map_err(|e| e.to_string())?,
    );

    menu.build().map_err(|e| e.to_string())
}

/// Handle tray menu item clicks.
pub fn handle_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    let id = event.id().0.as_str();

    match id {
        id if id.starts_with("tray:res:") => {
            if let Some(dims) = id.strip_prefix("tray:res:") {
                let parts: Vec<&str> = dims.split('x').collect();
                if let (Some(Ok(w)), Some(Ok(h))) = (
                    parts.first().map(|s| s.parse::<u32>()),
                    parts.get(1).map(|s| s.parse::<u32>()),
                ) {
                    let app = app.clone();
                    std::thread::spawn(move || {
                        // set_resolution requires an exact supported mode
                        // (width×height@Hz). Pass the current refresh rate so
                        // the resolution changes at the rate already in use,
                        // matching the main app's dropdown behavior.
                        let current_hz = crate::display_info::query_current_display()
                            .map(|d| d.refresh_hz)
                            .unwrap_or(0);
                        match crate::display_info::set_resolution(w, h, current_hz) {
                            Ok(mode) => {
                                crate::flyout_window::show_after_ready(&app, mode).ok();
                            }
                            Err(e) => log::warn!("[tray] resolution change failed: {e}"),
                        }
                    });
                }
            }
        }
        id if id.starts_with("tray:hz:") => {
            if let Some(hz_str) = id.strip_prefix("tray:hz:") {
                if let Ok(hz) = hz_str.parse::<u32>() {
                    let app = app.clone();
                    std::thread::spawn(move || match crate::display_info::set_refresh_rate(hz) {
                        Ok(mode) => {
                            crate::flyout_window::show_after_ready(&app, mode).ok();
                        }
                        Err(e) => log::warn!("[tray] refresh rate change failed: {e}"),
                    });
                }
            }
        }
        "tray:fav:add" => {
            if let Some(display) = crate::display_info::query_current_display() {
                let mut config = crate::config::load_config(app.clone());
                let fav = crate::config::FavoriteDisplayMode {
                    width: display.width,
                    height: display.height,
                    hz: display.refresh_hz,
                };
                if !config.favorite_display_modes.contains(&fav) {
                    config.favorite_display_modes.push(fav);
                    crate::config::save_config(app.clone(), config).ok();
                    refresh_menu(app).ok();
                }
            }
        }
        "tray:fav:clear" => {
            let mut config = crate::config::load_config(app.clone());
            config.favorite_display_modes.clear();
            crate::config::save_config(app.clone(), config).ok();
            refresh_menu(app).ok();
        }
        id if id.starts_with("tray:fav:") => {
            if let Some(idx_str) = id.strip_prefix("tray:fav:") {
                if let Ok(idx) = idx_str.parse::<usize>() {
                    let config = crate::config::load_config(app.clone());
                    if let Some(fav) = config.favorite_display_modes.get(idx) {
                        let (w, h, hz) = (fav.width, fav.height, fav.hz);
                        let app = app.clone();
                        std::thread::spawn(move || {
                            match crate::display_info::set_resolution(w, h, hz) {
                                Ok(mode) => {
                                    crate::flyout_window::show_after_ready(&app, mode).ok();
                                }
                                Err(e) => log::warn!("[tray] favorite apply failed: {e}"),
                            }
                        });
                    }
                }
            }
        }
        "tray:restart" => crate::request_restart(app),
        "tray:quit" => crate::request_shutdown_and_exit(app),
        _ => {}
    }
}

/// Rebuild and re-attach the tray menu.
static LAST_REFRESH: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

pub fn refresh_menu(app: &AppHandle) -> Result<(), String> {
    // Debounce: the display-changed event can fire rapidly (WM_SETTINGCHANGE
    // broadcasts), and build_menu queries display info (EDID reads). Rebuild
    // at most once per second so a burst of events costs one refresh.
    {
        let mut last = LAST_REFRESH.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = *last {
            if prev.elapsed() < std::time::Duration::from_secs(1) {
                return Ok(());
            }
        }
        *last = Some(std::time::Instant::now());
    }

    let menu = build_menu(app)?;
    if let Some(tray) = app.tray_by_id("main-tray") {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
    }
    Ok(())
}
