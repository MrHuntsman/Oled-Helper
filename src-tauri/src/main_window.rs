//! The main application window, and the webview inside it.
//!
//! The window and its webview have separate lifetimes on purpose. The window is
//! created once, at startup, and lives for the whole session; the webview is
//! torn down whenever the window is hidden to the tray and rebuilt when it is
//! shown again.
//!
//! That is the only way to actually release WebView2's graphics memory while
//! the app sits idle: Chromium's browser and GPU processes live as long as any
//! webview exists, and nothing in the WebView2 API gives that memory back while
//! they do (measured: the GPU process holds ~270-340 MB committed with the app
//! in the tray, and it does not move for hiding, renderer suspension or tab
//! unmounting). Closing the last webview lets those processes exit, and the GPU
//! process is recreated on the next show.
//!
//! Keeping the *window* alive is what makes this cheap to bolt on: everything
//! that hangs off the HWND keeps working across a teardown, so none of it has
//! to be re-created or re-registered - the WM_HOTKEY / WM_DISPLAYCHANGE /
//! WM_SIZE subclass (lib.rs), the RegisterHotKey registrations (hotkeys.rs), the
//! crisp window icon, the window-state plugin's geometry tracking, the Text
//! tab's preview owner and the taskbar button.
//!
//! What it costs is latency and hotkeys:
//!   * showing the window again boots a fresh WebView2 and a fresh Svelte app
//!     (roughly a second) instead of revealing a pre-warmed webview;
//!   * while the webview is gone there is no JS to perform a global hotkey, so
//!     hotkeys::handle_hotkey routes them to hotkey_actions instead. The
//!     handoff is explicit: `set_frontend_alive(false)` here, and back to true
//!     from `main_frontend_ready` once the rebuilt frontend has its listeners
//!     attached (an event emitted into a webview that is still loading is lost,
//!     so the frontend has to say when it is ready).

use tauri::{
    webview::PageLoadEvent, AppHandle, Emitter, LogicalPosition, Manager, Webview, WebviewBuilder,
    WebviewUrl, Window,
};

/// Label shared by the window and its webview (Tauri keys both registries by
/// label, and the frontend's IPC is bound to the webview label).
pub(crate) const LABEL: &str = "main";

/// The main window's WebView2 browser arguments. These must stay byte-identical
/// across every webview the app creates or WebView2 refuses to share one
/// environment (see webview_env); the flyout and the tray menu read them from
/// here for exactly that reason. `--enable-hdr` and `--enable-unsafe-webgpu`
/// are what the calibration tabs need; this used to live in tauri.conf.json
/// while the window was declared there.
pub(crate) const BROWSER_ARGS: &str =
    "--enable-hdr --enable-unsafe-webgpu --disk-cache-size=67108864";

// The window's own geometry, moved out of tauri.conf.json when the window
// stopped being a config-declared WebviewWindow (Tauri would otherwise create
// one for us, and a window cannot be converted to have a detachable webview
// after the fact). Size/position are restored across launches by the
// window-state plugin; these are the first-run values.
const TITLE: &str = "OLED Helper";
const WIDTH: f64 = 940.0;
const HEIGHT: f64 = 700.0;
const MIN_WIDTH: f64 = 600.0;
const MIN_HEIGHT: f64 = 400.0;

/// Creates the main window and its webview. Called once from setup().
///
/// An autostart launch (--start-tray, Start on Boot) builds the window
/// hidden and with no webview: the process comes up in the app's own idle
/// tray state, the same one a close-to-tray reaches, so a login launch never
/// flashes a window or steals focus. Everything that hangs off the HWND (the
/// subclass, hotkeys, the icon) is created with the window; only the webview
/// is missing, and reveal() rebuilds it on the first open exactly as it does
/// after a tray teardown.
pub(crate) fn create(app: &AppHandle) -> Result<Window, String> {
    let start_in_tray = crate::process::start_in_tray();
    let window = tauri::window::WindowBuilder::new(app, LABEL)
        .title(TITLE)
        .inner_size(WIDTH, HEIGHT)
        .min_inner_size(MIN_WIDTH, MIN_HEIGHT)
        .resizable(true)
        .visible(!start_in_tray)
        // No native shadow/border: the app draws its own rounded chrome, and
        // Windows otherwise paints a hairline rectangle around it.
        .shadow(false)
        .build()
        .map_err(|error| format!("could not create the main window: {error}"))?;
    if start_in_tray {
        log::info!("[window] autostart launch: main window created hidden, webview deferred");
    } else {
        build_webview(&window)?;
    }
    Ok(window)
}

/// The window, if it exists. Always Some after setup(); the window is never
/// destroyed while the process runs (a close goes to the tray).
pub(crate) fn window(app: &AppHandle) -> Option<Window> {
    app.get_window(LABEL)
}

/// The window's webview, if it is currently built.
pub(crate) fn webview(app: &AppHandle) -> Option<Webview> {
    app.get_webview(LABEL)
}

/// Whether the frontend is currently available to handle events. False while
/// the app is in the tray with its webview torn down.
pub(crate) fn has_webview(app: &AppHandle) -> bool {
    webview(app).is_some()
}

fn build_webview(window: &Window) -> Result<Webview, String> {
    if let Some(existing) = window.app_handle().get_webview(LABEL) {
        return Ok(existing); // already built
    }
    let builder = WebviewBuilder::new(LABEL, WebviewUrl::App("index.html".into()))
        .additional_browser_args(BROWSER_ARGS)
        // Follow the window's size. Without this the child webview keeps the
        // size it was created with: the runtime's auto-resize pass only runs
        // for webviews whose recorded bounds are Some, and `add_child` sets
        // the initial bounds without ever enabling the auto_resize flag
        // (tauri WebviewBuilder::build only assigns bounds; the rate table
        // behind WebviewMessage::SetAutoResize is what tauri-runtime-wry's
        // WM_SIZE handler resizes from). The symptom was a window that could
        // be resized while an empty gap stayed where the webview ended.
        .auto_resize()
        // Belt and braces for the hotkey handoff. The frontend calls
        // main_frontend_ready once it has mounted, which is the precise signal;
        // this covers the case where that call never arrives (a JS error before
        // mount, or a command that is not registered), because then hotkeys
        // would keep running in Rust while the window is up and its edits would
        // drift from what the UI shows. Reaching the load event means the
        // page's module script has run, which is when its listeners attach.
        .on_page_load(|_webview, payload| {
            if payload.event() == PageLoadEvent::Finished {
                crate::hotkeys::set_frontend_alive(true);
            }
        });
    let size = window
        .inner_size()
        .map_err(|error| format!("could not read the main window size: {error}"))?;
    let webview = window
        .add_child(builder, LogicalPosition::new(0.0, 0.0), size)
        .map_err(|error| format!("could not create the main webview: {error}"))?;
    log::info!(
        "[window] main webview built ({}x{})",
        size.width,
        size.height
    );
    Ok(webview)
}

/// Shows and focuses the window, rebuilding the webview first if the tray
/// teardown took it away. Called from the tray icon, the tray-menu Open item
/// and the second-instance watcher, so the "reopen from the tray" path always
/// looks the same.
pub(crate) fn reveal(app: &AppHandle) {
    if !has_webview(app) {
        let app_handle = app.clone();
        std::thread::spawn(move || {
            if let Err(error) = rebuild(&app_handle) {
                log::error!("[window] could not rebuild the main webview: {error}");
            }
            // Shown either way: an empty window the user can close beats
            // nothing happening at all after clicking the tray icon.
            show_and_focus(&app_handle);
        });
        return;
    }
    show_and_focus(app);
}

fn show_and_focus(app: &AppHandle) {
    let Some(window) = window(app) else {
        return;
    };
    let was_minimized = window.is_minimized().unwrap_or(false);
    if was_minimized {
        let _ = window.unminimize();
    }
    // Resume the webview before the window is visible again, so the first frame
    // it paints is live rather than a stale suspended surface. A freshly
    // rebuilt webview has no recorded policy state, so this is a no-op then.
    crate::webview_env::set_active(app, LABEL);
    if window.show().is_ok() {
        let _ = window.set_focus();
        // animated = the window was minimized, so restoring replays the Windows
        // restore animation; a merely hidden window (close-to-tray) appears
        // instantly. The frontend uses this to decide whether the Text tab's
        // native preview must wait out the animation before showing.
        let _ = app.emit(
            "window-visibility",
            serde_json::json!({ "visible": true, "animated": was_minimized }),
        );
    }
}

/// Rebuilds the main webview after a teardown. The caller is responsible for
/// showing the window; this only puts the webview back so the window cannot be
/// revealed empty.
///
/// `Window::add_child` waits for work it queues onto the main thread, so it
/// must not run inside a main-thread callback (the tray click handler, a window
/// event); call this from a worker thread.
pub(crate) fn rebuild(app: &AppHandle) -> Result<(), String> {
    let window = window(app).ok_or_else(|| "the main window no longer exists".to_string())?;
    if app.get_webview(LABEL).is_some() {
        return Ok(()); // already built (a concurrent show)
    }
    build_webview(&window)?;
    Ok(())
}

/// Releases every webview, and with them WebView2's browser and GPU processes,
/// while the app stays resident in the tray. The frontend stops being able to
/// perform hotkey actions, so this must be the point of no return: hotkeys and
/// every UI-driven event go through hotkey_actions until the webview is rebuilt
/// and the frontend reports in.
pub(crate) fn tear_down(app: &AppHandle) {
    // Before the webviews go: a hotkey arriving mid-teardown must not be
    // emitted into a webview that is about to die.
    crate::hotkeys::set_frontend_alive(false);
    // The overlays are built on demand, so dropping them here costs one slower
    // first open afterwards and nothing else. They have to go too: one live
    // webview is enough to keep Chromium alive, which is the whole point.
    let flyout = crate::flyout_window::FLYOUT_LABEL;
    let tray_menu = crate::tray_menu_window::LABEL;
    for label in [flyout, tray_menu] {
        if label == tray_menu && crate::tray_menu_window::keep_tray_menu_warm() {
            log::info!("[tray-menu] keeping tray menu warm while the main webview is in the tray");
            continue;
        }
        let Some(window) = app.get_webview_window(label) else {
            continue;
        };
        if label == flyout {
            // The flyout's HWND is handed to the taskbar dimmer's z-order
            // checks; clear it before the window goes away.
            crate::taskbar_dimmer::set_flyout_hwnd(0);
        }
        // destroy() rather than close(): these are full WebviewWindows, and a
        // window left behind without a webview would be an empty always-on-top
        // popup still capturing clicks.
        if let Err(error) = window.destroy() {
            log::warn!("[window] could not destroy the {label} window: {error}");
        }
    }
    // Both popups keep per-window session state (the tray menu's show/hide
    // phase, the flyout's handshake signals) that describes the window that
    // just died. Forget it here rather than in their create paths so the state
    // is only ever reset for a window this teardown removed.
    crate::flyout_window::reset_session_state();
    crate::tray_menu_window::reset_session_state();
    // The Text tab's native preview is an owned popup child of the main HWND:
    // it survives the window merely being hidden, and the frontend that would
    // have hidden it is going away with the webview. Drop it here so closing to
    // the tray can never leave a stray sample window on screen.
    if let Err(error) = crate::text_rendering::destroy_text_preview(app) {
        log::debug!("[window] text preview teardown: {error}");
    }
    if let Some(webview) = webview(app) {
        if let Err(error) = webview.close() {
            log::warn!("[window] could not close the main webview: {error}");
        } else {
            log::info!("[window] main webview closed; hotkeys now run in Rust");
        }
    }
    // The close above is queued to the main thread, so a reopen inside that
    // window sees a live webview, shows the window, and only then loses it -
    // leaving a blank window with no frontend. Let the queue drain, then put the
    // webview back if the window ended up visible again.
    std::thread::sleep(std::time::Duration::from_millis(300));
    if let Some(window) = window(app) {
        if window.is_visible().unwrap_or(false) && !has_webview(app) {
            log::info!("[window] reopened while the tray teardown was settling; rebuilding");
            match rebuild(app) {
                Ok(()) => crate::webview_env::set_active(app, LABEL),
                Err(error) => log::error!("[window] could not rebuild the main webview: {error}"),
            }
        }
    }
}
