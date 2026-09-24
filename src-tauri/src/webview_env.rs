//! Shared WebView2 environment and memory policy for the app's three windows.
//!
//! ## One environment, not three
//! WebView2 only shares one browser process between webviews that use the same
//! user data directory AND byte-identical browser arguments. Creating a second
//! webview with different arguments against a shared environment fails with
//! HRESULT 0x8007139F (see tauri-apps/tauri#11144), which is why the flyout
//! and the tray menu used to carry data directories of their own. That
//! isolation costs a whole extra browser process each, with its own GPU,
//! utility and crashpad children plus a profile folder on disk, and it is the
//! bulk of the app's idle memory. `shared_browser_args` hands the overlays the
//! main window's own flags so all three genuinely share one environment, which
//! is what Microsoft's WebView2 performance guidance recommends.
//!
//! ## Inactive windows give memory back
//! A hidden WebView2 keeps its processes and caches alive. Microsoft's
//! guidance for an inactive webview is to set the memory usage target level to
//! Low so the engine can evict caches and swap out, and to suspend the
//! renderer (TrySuspend) when it will not be used for a while. The policy
//! thread below applies exactly that, keyed on native visibility, because
//! there is no single Rust call to hook: the flyout's frontend hides its own
//! window, and the tray menu is hidden from a delayed fade thread.
//! https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/performance

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tauri::{AppHandle, Manager, Webview};

use crate::flyout_window::FLYOUT_LABEL;
use crate::main_window::LABEL as MAIN_LABEL;
use crate::tray_menu_window::LABEL as TRAY_MENU_LABEL;

// How often the policy re-checks which windows are on screen. Native
// visibility can only be polled, and a webview that stays hidden holds onto
// real memory, so one cheap query per second is a good trade.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

// Policy state per tracked window.
//
// `applied` is the last state requested (true = active), so neither the poll
// nor an explicit show/hide call repeats work the other one already did. Its
// initial value is "active" so nothing is trimmed before a window has had a
// chance to come up.
//
// `seq` orders the requests. A policy only reaches the webview asynchronously
// when it is requested off the main thread, so a request can still be in flight
// when a newer one lands (the tray menu hides on a delayed thread and can be
// re-shown by the very next click). Each dispatch carries the sequence it was
// created with and does nothing if a newer request has been made since, which
// is what keeps a stale suspend from freezing a popup that is already open.
struct Tracked {
    label: &'static str,
    applied: AtomicBool,
    seq: AtomicU64,
}

impl Tracked {
    const fn new(label: &'static str) -> Self {
        Self {
            label,
            applied: AtomicBool::new(true),
            seq: AtomicU64::new(0),
        }
    }
}

// Every tracked window is looked up through the *webview* registry, not the
// window one: the main window is a plain window whose webview is added and
// removed on demand (see main_window), so only its label is stable.
static TRACKED: [Tracked; 3] = [
    Tracked::new(MAIN_LABEL),
    Tracked::new(FLYOUT_LABEL),
    Tracked::new(TRAY_MENU_LABEL),
];

// Windows whose renderer is suspended while they are hidden. Only the two
// overlays qualify: each is shown on request and the show path resumes it
// first, nothing else depends on it running while it is off screen.
//
// The main window is deliberately NOT here even though it unmounts its tab
// content in the tray (the window-visibility gate in App.svelte). Its top
// level script keeps running while hidden, and hotkeys.rs only reports a
// global hotkey to the frontend ("hotkey-pressed"), which performs the action:
// suspending the renderer would queue those and make every hotkey look dead
// until the window was reopened. It still gets the Low memory target level,
// which is a caching/reclaim hint and does not stop script execution.
#[cfg(target_os = "windows")]
const SUSPEND_WHEN_HIDDEN: [&str; 2] = [FLYOUT_LABEL, TRAY_MENU_LABEL];

/// The main window's WebView2 browser arguments, read from the one place they
/// are defined so the overlays cannot drift from them. Two windows only share
/// one WebView2 environment when their arguments match exactly, which is the
/// whole point: one browser process instead of three.
pub(crate) fn shared_browser_args(_app: &AppHandle) -> Option<String> {
    Some(crate::main_window::BROWSER_ARGS.to_string())
}

/// Marks a window's webview as in use again: Normal memory target level, and
/// resume the renderer if it was suspended. Safe to call for a window that is
/// already active, and for one that does not exist yet. Takes the label rather
/// than a window handle so every caller can use it, including the window event
/// hooks whose `Window` is not a `WebviewWindow`.
pub(crate) fn set_active(app: &AppHandle, label: &str) {
    apply(app, label, true);
}

/// Marks a window's webview as hidden: Low memory target level, plus renderer
/// suspension for the windows listed in `SUSPEND_WHEN_HIDDEN`. Safe to call
/// for a window that is already inactive, and for one that does not exist yet.
pub(crate) fn set_inactive(app: &AppHandle, label: &str) {
    apply(app, label, false);
}

// The main window is different from the overlays. A taskbar minimize keeps
// its native window visible and its webview mounted, so it must not enter the
// inactive policy. Only a real tray hide makes the main webview inactive.
fn policy_active(label: &str, visible: bool, minimized: bool) -> bool {
    if label == MAIN_LABEL {
        visible
    } else {
        visible && !minimized
    }
}

fn apply(app: &AppHandle, label: &str, active: bool) {
    let Some(tracked) = TRACKED.iter().find(|tracked| tracked.label == label) else {
        return; // a window the policy does not track
    };
    let Some(webview) = app.get_webview(label) else {
        // No webview with this label right now: not created yet, or torn down
        // for the tray and about to be rebuilt (the main window's webview and
        // both popups are). Forget the recorded state so the next webview under
        // this label starts untrimmed and gets its own policy applied instead of
        // inheriting "inactive" from a webview that no longer exists.
        tracked.applied.store(true, Ordering::Release);
        return;
    };
    if tracked.applied.swap(active, Ordering::AcqRel) == active {
        return; // already in this state
    }
    let seq = tracked.seq.fetch_add(1, Ordering::AcqRel) + 1;
    platform::apply(&webview, tracked, active, seq);
}

/// Starts the background thread that keeps every tracked window's WebView2
/// memory policy in sync with its native visibility. Started once from setup(),
/// next to the other app-lifetime worker threads.
pub(crate) fn start_policy(app: AppHandle) {
    // Says outright which environment the overlays were built on: they only
    // share one browser process when these args match the main window's, so
    // this is the line to check if the per-window process sets ever come back.
    log::info!(
        "[webview] memory policy active (shared browser args: {})",
        match shared_browser_args(&app) {
            Some(args) => args,
            None => "none".to_owned(),
        }
    );
    let spawned = std::thread::Builder::new()
        .name("webview-memory-policy".into())
        .spawn(move || loop {
            std::thread::sleep(POLL_INTERVAL);
            for tracked in TRACKED.iter() {
                let label = tracked.label;
                let Some(window) = app.get_window(label) else {
                    continue;
                };
                // Keep the main webview active through a taskbar minimize. The
                // overlays still go inactive when minimized because they have
                // no state that must survive a taskbar restore.
                let visible = window.is_visible().unwrap_or(true);
                let minimized = window.is_minimized().unwrap_or(false);
                let active = policy_active(label, visible, minimized);
                if active {
                    set_active(&app, label);
                } else {
                    set_inactive(&app, label);
                }
            }
        });
    if let Err(error) = spawned {
        log::warn!("[webview] could not start the memory policy thread: {error}");
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{Tracked, Webview, SUSPEND_WHEN_HIDDEN};
    use std::sync::atomic::Ordering;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2_19, ICoreWebView2_3,
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    };
    use windows_core::Interface;

    pub(super) fn apply(webview: &Webview, tracked: &'static Tracked, active: bool, seq: u64) {
        let label = tracked.label;
        let suspend_when_hidden = SUSPEND_WHEN_HIDDEN.contains(&label);
        let log_label = label.to_owned();
        let result = webview.with_webview(move |webview| {
            // Superseded while this request was in flight (a hide that was
            // followed by a show before the message was delivered, say): the
            // newer request owns the webview now, so this one must not run.
            if tracked.seq.load(Ordering::Acquire) != seq {
                log::debug!("[webview] {label}: skipping superseded memory policy {seq}");
                return;
            }
            let controller = webview.controller();
            // WebView2 tracks visibility on the *controller*, and Tauri only
            // ever hides the window: WebviewWindow::hide() goes to
            // WindowMessage::Hide (tao ShowWindow), never to wry's
            // WebviewMessage::Hide, which is the only thing that sets this
            // property. So a controller for a window that is off screen still
            // reports visible, which keeps Chromium compositing the hidden
            // page and, worse, makes TrySuspend fail with
            // HRESULT_FROM_WIN32(ERROR_INVALID_STATE) = 0x8007139F: the docs
            // require IsVisible to be FALSE when TrySuspend is called. Mirror
            // the window state onto the controller before asking for anything.
            if let Err(error) = unsafe { controller.SetIsVisible(active) } {
                log::warn!("[webview] {label}: could not set webview visibility ({error})");
            }
            let core = match unsafe { controller.CoreWebView2() } {
                Ok(core) => core,
                Err(error) => {
                    log::warn!("[webview] {label}: no CoreWebView2 for the memory policy ({error})");
                    return;
                }
            };
            // The Low/Normal target level arrived in WebView2 114; on an older
            // runtime the cast fails and the level simply stays untouched.
            match core.cast::<ICoreWebView2_19>() {
                Ok(core19) => {
                    let level = if active {
                        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
                    } else {
                        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
                    };
                    if let Err(error) = unsafe { core19.SetMemoryUsageTargetLevel(level) } {
                        log::warn!("[webview] {label}: memory usage target level failed ({error})");
                    }
                }
                Err(_) => {
                    log::debug!("[webview] {label}: WebView2 runtime has no memory target level");
                }
            }
            if active {
                // Resume lives on ICoreWebView2_3, the same interface that
                // owns TrySuspend.
                if let Ok(core3) = core.cast::<ICoreWebView2_3>() {
                    if let Err(error) = unsafe { core3.Resume() } {
                        log::warn!("[webview] {label}: renderer resume failed ({error})");
                    }
                }
            } else if suspend_when_hidden {
                suspend_renderer(&core, &label);
            }
            if crate::detailed_logging_enabled() {
                log::info!(
                    "[webview] {label}: memory policy {}",
                    if active { "active" } else { "inactive" }
                );
            }
        });
        if let Err(error) = result {
            log::warn!("[webview] {log_label}: memory policy could not reach the webview ({error})");
        }
    }

    // Pauses this webview's renderer while it is hidden. TrySuspend is
    // best-effort: WebView2 may decline (for example while a page is still
    // loading), which is harmless because the Low memory target level above
    // was already applied.
    fn suspend_renderer(core: &ICoreWebView2, label: &str) {
        use webview2_com::TrySuspendCompletedHandler;

        let Ok(core3) = core.cast::<ICoreWebView2_3>() else {
            return;
        };
        let label = label.to_owned();
        let log_label = label.clone();
        let handler = TrySuspendCompletedHandler::create(Box::new(move |error, suspended| {
            match error {
                Ok(()) if suspended => {
                    // Info, not debug: this is the one line that proves the
                    // hidden overlay actually gave its memory back, and it only
                    // appears on a real hide transition.
                    log::info!("[webview] {label}: renderer suspended while hidden");
                }
                Ok(()) => {
                    log::debug!("[webview] {label}: WebView2 declined to suspend the renderer");
                }
                Err(error) => {
                    log::warn!("[webview] {label}: renderer suspend failed ({error})");
                }
            }
            Ok(())
        }));
        if let Err(error) = unsafe { core3.TrySuspend(&handler) } {
            log::warn!("[webview] {log_label}: TrySuspend call failed ({error})");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{policy_active, FLYOUT_LABEL, MAIN_LABEL, TRAY_MENU_LABEL};

    #[test]
    fn main_taskbar_minimize_stays_active() {
        assert!(policy_active(MAIN_LABEL, true, true));
    }

    #[test]
    fn main_tray_hide_becomes_inactive() {
        assert!(!policy_active(MAIN_LABEL, false, false));
    }

    #[test]
    fn overlay_minimize_becomes_inactive() {
        assert!(!policy_active(FLYOUT_LABEL, true, true));
        assert!(!policy_active(TRAY_MENU_LABEL, true, true));
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::{Tracked, Webview};

    pub(super) fn apply(
        _webview: &Webview,
        _tracked: &'static Tracked,
        _active: bool,
        _seq: u64,
    ) {
    }
}
