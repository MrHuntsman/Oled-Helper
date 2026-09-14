// hotkey_actions.rs, the global hotkey actions, performed in Rust.
//
// The actions used to live entirely in the main window's JS: Rust only
// reported the press ("hotkey-pressed") and App.svelte's handleHotkeyAction()
// did the actual work. That cannot survive the main webview being torn down
// while the app sits in the tray to give its Chromium graphics memory back,
// so the tray path is implemented here.
//
// Which path runs is decided in hotkeys::handle_hotkey: while the main webview
// is alive the frontend keeps owning the actions (only it holds the live
// per-monitor UI state), and this module takes over once it is gone. Both must
// produce the same result, so a change to one belongs in the other too. This
// module deliberately works off the persisted config, the frontend writes it
// before it is torn down, and re-derives everything from it, which is also what
// makes the frontend's next load agree with what happened in the tray.
//
// Actions run on a worker thread rather than in the WM_HOTKEY handler: that
// handler runs inside the main window's message loop, and several of these
// actions block for a noticeable time (an SDR brightness read/write is a
// display-config round trip, a favorite apply re-modes the display for
// hundreds of milliseconds). Auto-repeating bindings would otherwise freeze
// the UI for as long as the key is held.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, OnceLock};

use tauri::{AppHandle, Emitter, Manager};

use crate::config::AppConfig;
use crate::monitors::ListedMonitor;

/// Step/min/max for the SDR Content Brightness hotkeys; must match
/// SDR_BRIGHTNESS_STEP_NITS / _MIN_NITS / _MAX_NITS in App.svelte.
const SDR_STEP_NITS: f32 = 50.0;
const SDR_MIN_NITS: f32 = 80.0;
const SDR_MAX_NITS: f32 = 480.0;

/// Black Crush range, matching the frontend's clampAdjustment.
const CRUSH_MIN: i32 = -150;
const CRUSH_MAX: i32 = 150;
const CRUSH_STEP: i32 = 10;

#[derive(Clone, Debug)]
enum Action {
    ToggleBlackCrush,
    StepBlackCrush(i32),
    ToggleTaskbarDimmer,
    ToggleScreenDimmer,
    ToggleHdr,
    StepSdrBrightness(f32),
    TurnOffDisplay,
    Favorite(String),
}

fn parse(action: &str) -> Option<Action> {
    Some(match action {
        "toggle_black_crush" => Action::ToggleBlackCrush,
        "decrease_black_crush" => Action::StepBlackCrush(-CRUSH_STEP),
        "increase_black_crush" => Action::StepBlackCrush(CRUSH_STEP),
        "toggle_taskbar_dimmer" => Action::ToggleTaskbarDimmer,
        "toggle_screen_dimmer" => Action::ToggleScreenDimmer,
        "toggle_hdr" => Action::ToggleHdr,
        "decrease_sdr_brightness" => Action::StepSdrBrightness(-SDR_STEP_NITS),
        "increase_sdr_brightness" => Action::StepSdrBrightness(SDR_STEP_NITS),
        "turn_off_display" => Action::TurnOffDisplay,
        other => {
            // Per-favorite resolution hotkeys: the action name embeds the
            // favorite's stable id (see hotkeys::is_favorite_action).
            let fav_id = other.strip_prefix("favorite_")?;
            if fav_id.is_empty() {
                return None;
            }
            Action::Favorite(fav_id.to_string())
        }
    })
}

// ── Work queue ───────────────────────────────────────────────────────────────

static APP: OnceLock<AppHandle> = OnceLock::new();
static QUEUE: OnceLock<(Mutex<VecDeque<Action>>, Condvar)> = OnceLock::new();
static WORKER_STARTED: OnceLock<()> = OnceLock::new();

/// Queues one hotkey action (a `hotkeys::ACTIONS` name, or `favorite_<id>`).
/// Returns false for an unknown action name, so the caller can report it.
pub fn dispatch(app: &AppHandle, action: &str) -> bool {
    let Some(action) = parse(action) else {
        return false;
    };
    let _ = APP.set(app.clone());
    let (queue, wake) = QUEUE.get_or_init(|| (Mutex::new(VecDeque::new()), Condvar::new()));
    if WORKER_STARTED.set(()).is_ok() {
        // Detached: the worker blocks on the queue for the life of the
        // process, exactly like the other always-on helpers.
        if let Err(error) = std::thread::Builder::new()
            .name("hotkey-actions".into())
            .spawn(worker)
        {
            log::error!("[hotkey] could not start the action worker: {error}");
        }
    }
    let Ok(mut pending) = queue.lock() else {
        return true;
    };
    // Fold a repeated step into the one already waiting: the auto-repeat
    // bindings are registered without MOD_NOREPEAT, so holding the key queues
    // several presses a second and the worker would never catch up. Applying
    // the summed step once is equivalent, because the value is clamped after
    // the whole step and stays monotonic inside the clamp.
    let merged = match (&action, pending.back_mut()) {
        (Action::StepBlackCrush(delta), Some(Action::StepBlackCrush(queued))) => {
            *queued += *delta;
            true
        }
        (Action::StepSdrBrightness(delta), Some(Action::StepSdrBrightness(queued))) => {
            *queued += *delta;
            true
        }
        _ => false,
    };
    if !merged {
        pending.push_back(action);
    }
    drop(pending);
    wake.notify_one();
    true
}

fn worker() {
    let Some((queue, wake)) = QUEUE.get() else {
        return;
    };
    loop {
        let action = {
            let Ok(pending) = queue.lock() else {
                return;
            };
            let mut pending = wake
                .wait_while(pending, |pending| pending.is_empty())
                .unwrap_or_else(|error| error.into_inner());
            pending.pop_front()
        };
        let Some(action) = action else {
            continue;
        };
        let Some(app) = APP.get() else {
            continue;
        };
        log::debug!("[hotkey] performing {action:?} in the tray (no frontend)");
        if let Err(error) = perform(app, action) {
            log::warn!("[hotkey] {error}");
        }
    }
}

fn perform(app: &AppHandle, action: Action) -> Result<(), String> {
    match action {
        Action::ToggleBlackCrush => set_black_crush(app, None),
        Action::StepBlackCrush(delta) => set_black_crush(app, Some(delta)),
        Action::ToggleTaskbarDimmer => toggle_taskbar_dimmer(app),
        Action::ToggleScreenDimmer => toggle_screen_dimmer(app),
        Action::ToggleHdr => toggle_hdr(),
        Action::StepSdrBrightness(delta) => step_sdr_brightness(app, delta),
        Action::TurnOffDisplay => crate::power::power_turn_off_display(),
        Action::Favorite(fav_id) => apply_favorite(app, &fav_id),
    }
}

// ── Target resolution ────────────────────────────────────────────────────────

/// The display an action applies to, resolved the way the frontend resolves
/// its own `activeMonitor`: the monitor the user last selected (persisted as
/// `active_monitor_sig`), else the primary display. `selected` is Some only in
/// the first case, because the frontend routes a selected monitor through the
/// per-monitor gamma path and everything else through the global one.
struct Target {
    /// Display identity the tray menu compares an event's `monitorKey`
    /// against (the frontend's monProfileId).
    identity: String,
    /// Black Crush profile key for the display's current mode
    /// ("<identity>|<W>x<H>|<hz>"), None when the display is not resolvable.
    profile_key: Option<String>,
    /// The selected monitor, when there is one: what the per-monitor gamma
    /// command needs to target it.
    selected: Option<SelectedMonitor>,
    /// Whether the resolved target is the primary display (the no-selection
    /// fallback, where the flat config fields have to follow along).
    primary: bool,
}

/// A monitor selected in the app, reduced to what the per-monitor commands
/// need to target it.
struct SelectedMonitor {
    sig: String,
    device_name: String,
}

fn resolve_target(config: &AppConfig) -> Option<Target> {
    let monitors = crate::monitors::list_monitors();
    let selected = monitors
        .iter()
        .find(|monitor| !config.active_monitor_sig.is_empty() && monitor.sig == config.active_monitor_sig);
    if let Some(monitor) = selected {
        return Some(Target {
            identity: monitor_profile_id(monitor),
            profile_key: crush_profile_key(
                &monitor.profile_id,
                &monitor.monitor_id,
                &monitor.device_name,
                monitor.width,
                monitor.height,
                monitor.refresh_hz,
            ),
            selected: Some(SelectedMonitor {
                sig: monitor.sig.clone(),
                device_name: monitor.device_name.clone(),
            }),
            primary: monitor.primary,
        });
    }
    // Nothing selected (or the selected monitor is gone): the frontend falls
    // back to app.displayInfo, which is the primary display.
    let info = crate::display_info::query_current_display()?;
    Some(Target {
        identity: monitor_profile_id_of(&info.profile_id, &info.monitor_id, &info.device_name),
        profile_key: crush_profile_key(
            &info.profile_id,
            &info.monitor_id,
            &info.device_name,
            info.width,
            info.height,
            info.refresh_hz,
        ),
        selected: None,
        primary: true,
    })
}

/// The frontend's monProfileId: the monitor's profile identity, the identity
/// per-monitor profiles are keyed against.
fn monitor_profile_id(monitor: &ListedMonitor) -> String {
    monitor_profile_id_of(&monitor.profile_id, &monitor.monitor_id, &monitor.sig)
}

fn monitor_profile_id_of(profile_id: &str, monitor_id: &str, fallback: &str) -> String {
    if !profile_id.trim().is_empty() {
        profile_id.trim().to_string()
    } else if !monitor_id.trim().is_empty() {
        monitor_id.trim().to_string()
    } else {
        fallback.trim().to_string()
    }
}

/// The frontend's displayIdentity + profileKey: the profile identity, the
/// panel's current resolution and its refresh rate. None when the display or
/// its mode is not resolvable, which is exactly when the frontend stops
/// keying a profile and only keeps the flat value.
fn crush_profile_key(
    profile_id: &str,
    monitor_id: &str,
    device_name: &str,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> Option<String> {
    let identity = if !profile_id.trim().is_empty() {
        profile_id.trim()
    } else if !monitor_id.trim().is_empty() {
        monitor_id.trim()
    } else if !device_name.trim().is_empty() {
        device_name.trim()
    } else {
        "unknown-display"
    };
    if width == 0 || height == 0 || refresh_hz == 0 {
        return None;
    }
    Some(format!("{identity}|{width}x{height}|{refresh_hz}"))
}

fn clamp_adjustment(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    (value.round() as i32).clamp(CRUSH_MIN, CRUSH_MAX)
}

// ── Black Crush ──────────────────────────────────────────────────────────────

fn set_black_crush(app: &AppHandle, step: Option<i32>) -> Result<(), String> {
    let mut config = crate::config::load_config(app.clone());
    let target = resolve_target(&config);
    let identity = target
        .as_ref()
        .map(|target| target.identity.clone())
        .unwrap_or_default();
    let profile_key = target.as_ref().and_then(|target| target.profile_key.clone());
    // The target's current value for this mode, else the flat value: the same
    // fallback applyBlackLevelProfileForRefreshRate applies when it loads a
    // profile that does not exist yet.
    let mut adjustment = profile_key
        .as_ref()
        .and_then(|key| config.black_lift_profiles.get(key).copied())
        .map(|value| clamp_adjustment(value * 10.0))
        .unwrap_or_else(|| clamp_adjustment(config.black_lift * 10.0));
    match step {
        // The frontend clamps the stepped value to the same range.
        Some(delta) => adjustment = (adjustment + delta).clamp(CRUSH_MIN, CRUSH_MAX),
        None => config.black_enabled = !config.black_enabled,
    }
    if let Some(key) = &profile_key {
        config.black_lift_profiles.insert(key.clone(), adjustment as f64 / 10.0);
    }
    // The frontend persists the active monitor's view in the flat field too
    // (saveConfig writes black_lift unconditionally), so it stays the
    // no-selection fallback for the value last edited.
    config.black_lift = adjustment as f64 / 10.0;
    // applyGamma(blackLift()): the selected monitor's own device gets the
    // per-monitor write, everything else goes through the global path. A
    // failure here is only a warning on the frontend path as well (NVIDIA
    // reference mode rejects the write, for one), so the config still records
    // the user's new level.
    let lift = if config.black_enabled { adjustment } else { 0 };
    let gamma = app.state::<crate::gamma::GammaState>();
    let applied = match target.as_ref().and_then(|target| target.selected.as_ref()) {
        Some(monitor) => crate::gamma::apply_gamma_ramp_for(
            gamma,
            monitor.sig.clone(),
            Some(monitor.device_name.clone()),
            lift,
        ),
        None => crate::gamma::apply_gamma_ramp(gamma, lift),
    };
    if let Err(error) = applied {
        log::warn!("[hotkey] black crush gamma apply failed: {error}");
    }
    crate::config::save_config(app.clone(), config.clone())?;
    // The tray menu keeps its Black Crush row in step with this event.
    let _ = app.emit(
        "black-level-main-changed",
        serde_json::json!({
            "adjustment": adjustment,
            "enabled": config.black_enabled,
            "monitorKey": identity,
        }),
    );
    Ok(())
}

// ── Taskbar Dimmer ───────────────────────────────────────────────────────────

fn toggle_taskbar_dimmer(app: &AppHandle) -> Result<(), String> {
    let mut config = crate::config::load_config(app.clone());
    let Some(target) = resolve_target(&config) else {
        return Err("no display to toggle the taskbar dimmer on".to_string());
    };
    let key = target.identity.clone();
    // The live per-monitor state, read through the runtime profile map so a
    // toggle keeps the monitor's own opacity and fade settings - the hotkey
    // flips the selected monitor's dimmer, not every monitor's.
    let state = app.state::<crate::taskbar_dimmer::TaskbarDimmerState>();
    let status = crate::taskbar_dimmer::get_taskbar_dimmer_status(state, Some(key.clone()));
    let enabled = !status.enabled;
    crate::taskbar_dimmer::set_taskbar_dimmer(
        Some(key.clone()),
        enabled,
        status.opacity,
        status.fade_in_duration_ms,
        status.fade_out_duration_ms,
        false,
    )?;
    let entry = config.taskbar_dimmer_profiles.entry(key.clone()).or_default();
    entry.enabled = enabled;
    entry.opacity = status.opacity;
    entry.fade_in_duration_ms = status.fade_in_duration_ms;
    entry.fade_out_duration_ms = status.fade_out_duration_ms;
    // The flat fields mirror the primary monitor's profile (the tray menu has
    // no selector and reads them as its fallback).
    if target.primary {
        config.taskbar_dimmer_enabled = enabled;
        config.taskbar_dimmer_opacity = status.opacity;
        config.taskbar_dimmer_fade_in_duration_ms = status.fade_in_duration_ms;
        config.taskbar_dimmer_fade_out_duration_ms = status.fade_out_duration_ms;
    }
    crate::config::save_config(app.clone(), config)?;
    let _ = app.emit(
        "taskbar-dimmer-changed",
        serde_json::json!({
            "monitorKey": key,
            "enabled": enabled,
            "opacity": status.opacity,
            "fadeInDurationMs": status.fade_in_duration_ms,
            "fadeOutDurationMs": status.fade_out_duration_ms,
        }),
    );
    Ok(())
}

// ── Screen Dimmer ────────────────────────────────────────────────────────────

fn toggle_screen_dimmer(app: &AppHandle) -> Result<(), String> {
    let mut config = crate::config::load_config(app.clone());
    let Some(target) = resolve_target(&config) else {
        return Err("no display to toggle the screen dimmer on".to_string());
    };
    let key = target.identity.clone();
    // `enabled` on this status is the global Screen Dimmer master toggle, the
    // same value screenDimmer.enabled mirrors in the frontend.
    let status = crate::power::power_get_screen_dimmer_status(Some(key.clone()));
    let enabled = !status.enabled;
    crate::power::power_set_screen_protector(
        app.clone(),
        Some(key.clone()),
        enabled,
        status.seconds,
        status.opacity,
        status.fade_in_duration_ms,
        status.fade_out_duration_ms,
        status.fullscreen_detection,
    )?;
    config.screen_protector_enabled = enabled;
    let entry = config.screen_dimmer_profiles.entry(key).or_default();
    entry.seconds = status.seconds;
    entry.opacity = status.opacity;
    entry.fade_in_duration_ms = status.fade_in_duration_ms;
    entry.fade_out_duration_ms = status.fade_out_duration_ms;
    entry.fullscreen_detection = status.fullscreen_detection;
    if target.primary {
        config.screen_protector_seconds = status.seconds;
        config.screen_protector_opacity = status.opacity;
        config.screen_protector_fade_in_duration_ms = status.fade_in_duration_ms;
        config.screen_protector_fade_out_duration_ms = status.fade_out_duration_ms;
        config.screen_protector_fullscreen_detection = status.fullscreen_detection;
    }
    crate::config::save_config(app.clone(), config)?;
    if enabled {
        // Enabling registers the privileged helper task the dimmer needs for
        // media-playback suppression. The frontend warns and carries on when
        // the one-time elevation is declined: idle dimming does not need it.
        if let Err(error) = crate::helper::ensure_registered() {
            log::warn!(
                "[hotkey] privileged helper setup unavailable; media detection disabled: {error}"
            );
        }
    }
    Ok(())
}

// ── HDR + SDR Content Brightness ─────────────────────────────────────────────

fn toggle_hdr() -> Result<(), String> {
    // An unknown status is a no-op on the frontend path too (it is still
    // probing the display), so never guess in the other direction.
    let Some(status) = crate::hdr_toggle::get_windows_hdr_status()? else {
        log::debug!("[hotkey] HDR status unknown; ignoring the toggle");
        return Ok(());
    };
    crate::hdr_toggle::set_windows_hdr_enabled(!status.enabled)
}

fn step_sdr_brightness(app: &AppHandle, delta: f32) -> Result<(), String> {
    // SDR Content Brightness is a Windows setting, not an app setting, so read
    // it fresh on every step: Windows Settings or another tool may have moved
    // it while the app sat in the tray.
    let Some(info) = crate::hdr_toggle::get_sdr_white_level()? else {
        return Ok(());
    };
    let current = info.nits;
    if !current.is_finite() {
        return Ok(());
    }
    let target = (current + delta).clamp(SDR_MIN_NITS, SDR_MAX_NITS);
    if (target - current).abs() < f32::EPSILON {
        return Ok(());
    }
    let applied = crate::hdr_toggle::set_sdr_brightness_nits(target)?;
    let _ = app.emit(
        "sdr-content-brightness-main-changed",
        serde_json::json!({ "nits": applied.nits }),
    );
    Ok(())
}

// ── Favorite display modes ───────────────────────────────────────────────────

fn apply_favorite(app: &AppHandle, fav_id: &str) -> Result<(), String> {
    let config = crate::config::load_config(app.clone());
    // A binding whose favorite was removed resolves to nothing (the frontend
    // prunes those bindings), so this is just the belt-and-braces no-op.
    let Some(favorite) = config
        .favorite_display_modes
        .iter()
        .find(|favorite| favorite.fav_id == fav_id)
    else {
        return Ok(());
    };
    let device = favorite_target_device(favorite)?;
    // The same opt-in the frontend uses: only a favorite that was explicitly
    // confirmed on the monitor it was saved on may skip the dialog.
    let skip_confirmation = config.skip_favorite_confirmation
        && !favorite.confirmed_monitor_id.is_empty()
        && !favorite.monitor_id.is_empty()
        && favorite.confirmed_monitor_id == favorite.monitor_id;
    // Already running this mode? The frontend returns early so a hotkey press
    // cannot re-mode the display (or open a pointless dialog).
    let current = match &device {
        Some(device_name) => crate::display_info::query_display_for(device_name),
        None => crate::display_info::query_current_display(),
    };
    if let Some(info) = current {
        if info.width == favorite.width
            && info.height == favorite.height
            && info.refresh_hz == favorite.hz
        {
            log::debug!("[hotkey] favorite {fav_id} is already the active mode");
            return Ok(());
        }
    }
    let applied = crate::display_info::set_resolution(
        favorite.width,
        favorite.height,
        favorite.hz,
        device,
        skip_confirmation,
    )?;
    if applied.skip_confirmation {
        // Opted out of the dialog: confirm immediately, exactly like the
        // frontend's skipConfirmation path.
        crate::display_info::confirm_mode_change()?;
    } else {
        // The "Keep display mode?" dialog is its own window, so it works with
        // the main webview gone; this is the same path the tray menu's
        // resolution rows take. The display module's watchdog reverts the
        // mode if nothing confirms it.
        crate::flyout_window::show_mode_confirmation(
            app.clone(),
            applied.width,
            applied.height,
            applied.refresh_hz,
            Some(applied.monitor_name.clone()),
            Some(applied.confirmation_seconds),
            Some(applied.confirmation_deadline_ms),
        )?;
    }
    Ok(())
}

/// Resolve a favorite's target monitor the way the frontend does: the GDI
/// device name of the monitor it was saved on, None for a legacy favorite
/// saved without a monitor (apply to the primary), or an error when that
/// monitor is not currently connected.
fn favorite_target_device(favorite: &crate::config::FavoriteDisplayMode) -> Result<Option<String>, String> {
    if favorite.monitor_device.is_empty()
        && favorite.monitor_id.is_empty()
        && favorite.monitor_sig.is_empty()
    {
        return Ok(None);
    }
    let monitors = crate::monitors::list_monitors();
    let monitor = monitors.iter().find(|monitor| {
        (!favorite.monitor_device.is_empty() && monitor.device_name == favorite.monitor_device)
            || (!favorite.monitor_id.is_empty() && monitor.monitor_id == favorite.monitor_id)
            || (!favorite.monitor_sig.is_empty() && monitor.sig == favorite.monitor_sig)
            || (!favorite.monitor_sig.is_empty()
                && stable_sig_part(&monitor.sig) == stable_sig_part(&favorite.monitor_sig))
    });
    match monitor {
        Some(monitor) if !monitor.device_name.is_empty() => Ok(Some(monitor.device_name.clone())),
        _ => Err(format!(
            "the monitor for favorite {} is not connected",
            favorite.fav_id
        )),
    }
}

/// The resolution-independent portion of a monitor signature
/// ("3840x2160|1600x900|192x192" -> "1600x900|192x192"): physical size + DPI
/// do not change with the mode, so a legacy favorite saved without an EDID
/// monitor id still matches its panel after a resolution switch.
fn stable_sig_part(sig: &str) -> String {
    let parts: Vec<&str> = sig.split('|').collect();
    if parts.len() >= 3 {
        format!("{}|{}", parts[1], parts[2])
    } else {
        sig.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_actions() {
        assert!(matches!(parse("toggle_black_crush"), Some(Action::ToggleBlackCrush)));
        assert!(matches!(
            parse("increase_black_crush"),
            Some(Action::StepBlackCrush(step)) if step == CRUSH_STEP
        ));
        assert!(matches!(
            parse("decrease_black_crush"),
            Some(Action::StepBlackCrush(step)) if step == -CRUSH_STEP
        ));
        assert!(matches!(
            parse("increase_sdr_brightness"),
            Some(Action::StepSdrBrightness(step)) if step == SDR_STEP_NITS
        ));
        assert!(matches!(parse("turn_off_display"), Some(Action::TurnOffDisplay)));
        assert!(matches!(
            parse("favorite_abc123"),
            Some(Action::Favorite(id)) if id == "abc123"
        ));
    }

    #[test]
    fn rejects_unknown_actions() {
        assert!(parse("not_a_hotkey").is_none());
        // The bare prefix is not a favorite id.
        assert!(parse("favorite_").is_none());
        // hold_compare is handled entirely in hotkeys.rs (it owns no config or
        // UI state), so it must not resolve here.
        assert!(parse("hold_compare").is_none());
    }

    #[test]
    fn profile_key_matches_the_frontend_format() {
        assert_eq!(
            crush_profile_key("MONITOR\\LGD05XX", "", "", 3840, 2160, 144),
            Some("MONITOR\\LGD05XX|3840x2160|144".to_string())
        );
        // Falls back through monitor id and device name.
        assert_eq!(
            crush_profile_key("", "MONITOR\\LGD05XX", r"\\.\DISPLAY1", 1920, 1080, 60),
            Some("MONITOR\\LGD05XX|1920x1080|60".to_string())
        );
        assert_eq!(
            crush_profile_key("", "", "unknown-display", 1920, 1080, 60),
            Some("unknown-display|1920x1080|60".to_string())
        );
        // No mode to scope the profile to.
        assert_eq!(crush_profile_key("MONITOR\\LGD05XX", "", "", 3840, 2160, 0), None);
        assert_eq!(crush_profile_key("MONITOR\\LGD05XX", "", "", 0, 0, 144), None);
    }

    #[test]
    fn clamps_adjustments_like_the_frontend() {
        assert_eq!(clamp_adjustment(150.4), 150);
        assert_eq!(clamp_adjustment(-150.6), -150);
        assert_eq!(clamp_adjustment(7.6), 8);
        assert_eq!(clamp_adjustment(f64::NAN), 0);
    }

    #[test]
    fn stable_sig_part_drops_the_resolution() {
        assert_eq!(stable_sig_part("3840x2160|1600x900|192x192"), "1600x900|192x192");
        assert_eq!(stable_sig_part("3840x2160"), "3840x2160");
        assert_eq!(stable_sig_part(""), "");
    }
}
