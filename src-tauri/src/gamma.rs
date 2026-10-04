//! Gamma ramp application: synchronous apply + periodic safety restore.
//!
//! The frontend calls `apply_gamma_ramp` on every slider change; the ramp is
//! applied immediately (no polling thread, no IPC round-trip delay). A
//! lightweight safety thread re-applies every 5 seconds so another app
//! resetting the system gamma (fullscreen games, driver updates) is corrected
//! without user interaction.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};

pub(crate) struct GammaShutdown {
    pub(crate) requested: AtomicBool,
    pub(crate) wake: Condvar,
    pub(crate) lock: Mutex<()>,
}

pub(crate) struct GammaState {
    pub(crate) desired: Arc<AtomicI32>,
    // Per-monitor black-lift values keyed by the monitor's resolution-
    // independent identity (EDID device id, sig fallback; see
    // monitors::monitor_key). The safety thread re-applies these to their own
    // devices alongside the global value, and shutdown resets every affected
    // monitor. Entries may contain an explicit zero so the screen protector
    // can distinguish it from an uninitialized profile.
    pub(crate) per_monitor: Arc<Mutex<HashMap<String, i32>>>,
    // Current screen-protector gamma brightness per monitor: 1000 = unmodified.
    pub(crate) screen_dim_brightness: Arc<Mutex<HashMap<String, u32>>>,
    pub(crate) shutdown: Arc<GammaShutdown>,
    pub(crate) safety_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for GammaState {
    fn drop(&mut self) {
        self.shutdown.requested.store(true, Ordering::Release);
        self.shutdown.wake.notify_one();
        if let Ok(mut guard) = self.safety_thread.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
        unsafe {
            crate::gamma_ramp::reset_display_ramp();
            reset_per_monitor_ramps(&self.per_monitor);
        }
    }
}

// Restores every monitor that has a per-monitor lift entry to the linear ramp.
// Entries are keyed by the resolution-independent monitor identity (EDID id,
// sig fallback), so a monitor whose resolution changed since the lift was
// applied still resolves to its current device; a monitor that is no longer
// connected cannot be reset (the driver clears its ramp on unplug anyway).
// Two identical panels can SHARE a key (same EDID id, same caps), so every
// connected monitor matching the key is reset, not just the first, or the
// sibling's ramp would stay applied.
pub(crate) fn reset_per_monitor_ramps(per_monitor: &Mutex<HashMap<String, i32>>) {
    let Ok(map) = per_monitor.lock() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    for key in map.keys() {
        for monitor in crate::monitors::all_monitors() {
            if crate::monitors::monitor_key(&monitor) == *key && !monitor.device_name.is_empty() {
                unsafe {
                    crate::gamma_ramp::reset_monitor_ramp(&monitor.device_name);
                }
            }
        }
    }
}

// Applies the current Black Crush profiles with an additional screen-level
// brightness multiplier. The screen protector uses this instead of a window
// overlay, so every connected monitor (including shell flyouts) is dimmed by
// the display gamma path.
pub(crate) fn apply_screen_gamma_ramps(
    desired: &AtomicI32,
    per_monitor: &Mutex<HashMap<String, i32>>,
    brightness_by_monitor: &HashMap<String, u32>,
    restore_unconfigured: bool,
) {
    if crate::vendor_info::nvidia_reference_mode_enabled() {
        return;
    }
    let _gamma_guard = crate::hotkeys::gamma_write_guard();
    let values: HashMap<String, i32> = per_monitor
        .lock()
        .map(|map| map.clone())
        .unwrap_or_default();
    // Keep the external gamma watchdog's record in step with the state this
    // helper is about to write. Edge-triggered inside sync, so the every-5s
    // safety tick never touches the disk after the first transition.
    crate::gamma_watch::sync(
        desired.load(Ordering::Relaxed),
        &values,
        brightness_by_monitor,
    );
    let primary_fallback = desired.load(Ordering::Relaxed);
    for monitor in crate::monitors::all_monitors() {
        if monitor.device_name.is_empty() {
            continue;
        }
        let key = crate::monitors::monitor_key(&monitor);
        let sig = monitor.sig();
        // The dimmer writes its brightness map keyed by the current sig (it
        // rebuilds it every tick), while lift entries are keyed by the stable
        // monitor id, so look up both keys.
        // The dimmer writes its brightness map keyed by the monitor's device
        // name (unique per physical monitor, so identical panels stay
        // distinct), while lift entries are keyed by the stable monitor id,
        // so look up all three keys.
        let brightness_thousandths = brightness_by_monitor
            .get(&monitor.device_name)
            .or_else(|| brightness_by_monitor.get(&key))
            .or_else(|| brightness_by_monitor.get(&sig))
            .copied()
            .unwrap_or(1000);
        let brightness = (brightness_thousandths.min(1000) as f32) / 1000.0;
        let dimmed = brightness_thousandths < 1000;
        // The primary fallback preserves the existing global Black Crush
        // value during startup. Every write below is still per-monitor; the
        // global SetDeviceGammaRamp path is never used for screen dimming.
        let lift = values.get(&key).copied().unwrap_or_else(|| {
            if monitor.primary {
                primary_fallback
            } else {
                0
            }
        });
        let has_profile = values.contains_key(&key);
        if (has_profile && (lift != 0 || dimmed || restore_unconfigured))
            || (!has_profile && monitor.primary && primary_fallback != 0)
            || dimmed
            || restore_unconfigured
        {
            let ramp = crate::gamma_ramp::build_dimmed_ramp(lift, brightness);
            unsafe {
                let _ = crate::gamma_ramp::set_monitor_ramp(&monitor.device_name, &ramp);
            }
        }
    }
}

pub(crate) fn gamma_safety_loop(
    desired: Arc<AtomicI32>,
    per_monitor: Arc<Mutex<HashMap<String, i32>>>,
    screen_dim_brightness: Arc<Mutex<HashMap<String, u32>>>,
    shutdown: Arc<GammaShutdown>,
) {
    loop {
        let guard = shutdown
            .lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_guard, _timeout) = shutdown
            .wake
            .wait_timeout_while(guard, std::time::Duration::from_secs(5), |_| {
                !shutdown.requested.load(Ordering::Acquire)
            })
            .unwrap_or_else(|error| error.into_inner());
        if shutdown.requested.load(Ordering::Acquire) {
            break;
        }
        // A timeout is the normal five-second safety tick. The predicate also
        // closes the lost-notification race during shutdown.
        // Hold-to-compare applies neutral gamma directly on the hardware
        // without updating `desired`. Don't overwrite its comparison view.
        if crate::hotkeys::is_hold_compare_active() {
            if crate::vendor_info::nvidia_reference_mode_enabled() {
                continue;
            }
            crate::hotkeys::recover_hold_restore();
            continue;
        }
        if crate::vendor_info::nvidia_reference_mode_enabled() {
            continue;
        }
        let brightness = screen_dim_brightness
            .lock()
            .map(|map| map.clone())
            .unwrap_or_default();
        // Re-apply only through each monitor's own GDI device. At full
        // brightness with no Black Crush values the helper remains a no-op.
        apply_screen_gamma_ramps(&desired, &per_monitor, &brightness, false);
        let global_lift = desired.load(Ordering::Relaxed);
        if global_lift != 0 {
            // The global Black Crush fallback targets the primary display
            // (GetDC(None)). Compose the primary's current screen-dimmer
            // brightness into it, otherwise this safety tick would overwrite
            // the primary's dimmed ramp with a full-brightness one every
            // five seconds and the dim would vanish. At full brightness
            // (dim off) build_dimmed_ramp equals build_ramp, so the
            // fallback behavior is unchanged for the plain Black Crush case.
            let primary_brightness = crate::monitors::all_monitors()
                .iter()
                .find(|m| m.primary)
                .and_then(|m| {
                    // The dim brightness map is keyed by the monitor's GDI
                    // device name (unique per physical monitor, so identical
                    // panels stay distinct); fall back to the sig for any
                    // stale entry written before the device-name re-key.
                    brightness
                        .get(&m.device_name)
                        .or_else(|| brightness.get(&m.sig()))
                        .copied()
                })
                .unwrap_or(1000);
            let ramp = crate::gamma_ramp::build_dimmed_ramp(
                global_lift,
                primary_brightness.min(1000) as f32 / 1000.0,
            );
            unsafe {
                let _ = crate::gamma_ramp::set_display_ramp(&ramp);
            }
        }
    }
}

// Applies the gamma ramp synchronously, the display update happens on the
// caller's thread (the Tauri command thread) and returns immediately. No
// background polling, no IPC delay. A separate safety thread re-applies at
// 5-second intervals to recover from external gamma resets.
#[tauri::command]
pub fn apply_gamma_ramp(state: State<GammaState>, black_lift: i32) -> Result<(), String> {
    // Frontend clamps to ±150 already; clamp here too so a stray caller can
    // never drive the ramp math with an extreme value.
    let black_lift = black_lift.clamp(-150, 150);
    if crate::vendor_info::nvidia_reference_mode_enabled() {
        return Err("NVIDIA Override to reference mode is enabled; disable it in your NVIDIA driver settings under System → Displays to use Black Crush Tweak".into());
    }
    // Hold the same gamma lock across the state check, desired-value update,
    // and possible write. If release completes just before this command gets
    // the lock, this path applies the newest value instead of leaving the
    // linear compare ramp or an older restore value behind.
    let _gamma_guard = crate::hotkeys::gamma_write_guard();
    if !crate::hotkeys::is_hold_compare_active() {
        let ramp = crate::gamma_ramp::build_ramp(black_lift);
        let applied = unsafe { crate::gamma_ramp::set_display_ramp(&ramp) };
        if !applied {
            return Err("SetDeviceGammaRamp failed".into());
        }
    }
    state.desired.store(black_lift, Ordering::Relaxed);
    // Keep the primary monitor's per-monitor entry in step with the global
    // value. The frontend routes the primary through apply_gamma_ramp_for
    // whenever a monitor is selected, so a later global write (tray menu,
    // no-selection fallback) must overwrite that entry, otherwise the safety
    // thread re-applies the stale per-monitor lift within five seconds.
    if let Some(primary) = crate::monitors::all_monitors().iter().find(|m| m.primary) {
        let mut map = state
            .per_monitor
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        map.insert(crate::monitors::monitor_key(primary), black_lift);
    }
    // Reflect the new state in the external gamma watch record (first
    // non-default transition writes the record and spawns the child).
    crate::gamma_watch::sync(
        black_lift,
        &state
            .per_monitor
            .lock()
            .map(|map| map.clone())
            .unwrap_or_default(),
        &state
            .screen_dim_brightness
            .lock()
            .map(|map| map.clone())
            .unwrap_or_default(),
    );
    Ok(())
}

// Same as apply_gamma_ramp but targets a specific monitor (by stable
// signature, plus the GDI device name which uniquely identifies a physical
// monitor even when two identical panels share the same sig and EDID id)
// instead of the primary. Used by per-monitor Black Crush when the frontend
// has a monitor selected; the primary monitor goes through the global path
// above. The per-monitor table keeps explicit zero values too.
#[tauri::command]
pub fn apply_gamma_ramp_for(
    state: State<GammaState>,
    sig: String,
    device_name: Option<String>,
    black_lift: i32,
) -> Result<(), String> {
    let black_lift = black_lift.clamp(-150, 150);
    if crate::vendor_info::nvidia_reference_mode_enabled() {
        return Err("NVIDIA Override to reference mode is enabled; disable it in your NVIDIA driver settings under System → Displays to use Black Crush Tweak".into());
    }
    let _gamma_guard = crate::hotkeys::gamma_write_guard();
    // Resolve the input sig (or device name) to the live monitor and use the
    // stable resolution-independent key (EDID id, sig fallback) for the
    // table, so a mode switch cannot orphan the entry or strand the monitor's
    // ramp. A stale sig (the frontend racing a mode change) fails here; the
    // frontend re-applies with the fresh sig once the monitor list catches
    // up. The device name disambiguates identical panels that share the sig.
    let monitor = crate::monitors::resolve_monitor_for_apply(&sig, device_name.as_deref())
        .ok_or_else(|| format!("no display device for monitor {sig}"))?;
    let key = crate::monitors::monitor_key(&monitor);
    if !crate::hotkeys::is_hold_compare_active() {
        let brightness = state
            .screen_dim_brightness
            .lock()
            .ok()
            .and_then(|map| {
                map.get(&monitor.device_name)
                    .or_else(|| map.get(&key))
                    .or_else(|| map.get(&sig))
                    .copied()
            })
            .unwrap_or(1000);
        let ramp = crate::gamma_ramp::build_dimmed_ramp(black_lift, brightness as f32 / 1000.0);
        let applied = unsafe { crate::gamma_ramp::set_monitor_ramp(&monitor.device_name, &ramp) };
        if !applied {
            return Err("SetDeviceGammaRamp failed".into());
        }
    }
    {
        let mut map = state
            .per_monitor
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Keep zero as an explicit per-monitor value. Removing it would make
        // the screen protector fall back to the primary/global profile and
        // could reintroduce an old lift on a monitor intentionally set to 0.
        map.insert(key, black_lift);
    }
    // The primary's lift is authoritative in `desired` as well, not just the
    // per-monitor table: hold-to-compare and the safety thread read the
    // global value, so a per-monitor write targeting the primary must keep
    // it in sync. Otherwise a stale non-zero `desired` (seeded at startup or
    // left by an earlier global write) survives a disable or zero through
    // this path, and hold-to-compare keeps switching the primary's gamma
    // while the tweak is off.
    if monitor.primary {
        state.desired.store(black_lift, Ordering::Relaxed);
    }
    // Reflect the new state in the external gamma watch record.
    crate::gamma_watch::sync(
        state.desired.load(Ordering::Relaxed),
        &state
            .per_monitor
            .lock()
            .map(|map| map.clone())
            .unwrap_or_default(),
        &state
            .screen_dim_brightness
            .lock()
            .map(|map| map.clone())
            .unwrap_or_default(),
    );
    Ok(())
}

/// Seeds the backend's Black Crush state from persisted config, so a session
/// that starts with the main webview deferred (autostart, or a restart into
/// the tray) still applies every monitor's saved profile. The frontend
/// resolves the same per-display keys once it runs, but until it does the
/// backend is the only writer; without this seed only the primary monitor
/// received anything (from the global fallback) and every other monitor's
/// Black Crush stayed unapplied until the app window was opened.
pub(crate) fn seed_black_crush_from_config(
    state: &GammaState,
    config: &crate::config::AppConfig,
) {
    let monitors = crate::monitors::list_monitors();
    resolve_profiles(state, config, &monitors);
}

// A Black Crush profile key in the pre-display-identity format: a bare
// refresh rate in Hz. Those entries are a migration fallback only (the
// frontend keeps them in `legacyBlackProfiles` for the same reason); every
// key written today is display-scoped.
fn is_legacy_rate_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_digit())
}

// Stored profile values are adjustment tenths; the runtime tables keep the
// integer ramp units the ramp builder wants.
fn lift_from_stored(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    (value * 10.0).round().clamp(-150.0, 150.0) as i32
}

// The profile key one monitor's CURRENT mode produces. Same
// "<identity>|WxH|HZ" shape the frontend's profileKey builds (see
// stores.svelte.js), with the identity being the monitor's profile id (the
// EDID id, disambiguated for identical panels). Monitors whose mode could not
// be read produce no key, so they keep the value they already had.
fn profile_key_for(monitor: &crate::monitors::ListedMonitor) -> Option<String> {
    if monitor.profile_id.is_empty() || monitor.width == 0 || monitor.height == 0 {
        return None;
    }
    if monitor.refresh_hz == 0 {
        return None;
    }
    Some(format!(
        "{}|{}x{}|{}",
        monitor.profile_id, monitor.width, monitor.height, monitor.refresh_hz
    ))
}

// The value one monitor resolves to for the mode it is in, mirroring the
// frontend's applyBlackLevelProfileForRefreshRate: the scoped
// "<identity>|WxH|HZ" profile first, then the legacy per-rate entry, then the
// flat legacy value, and zero when display-scoped profiles exist but none of
// them covers this mode (the frontend's own fallback for a new mode).
fn resolve_profile_lift(
    config: &crate::config::AppConfig,
    key: &str,
    hz: u32,
    has_scoped_profiles: bool,
) -> i32 {
    if let Some(value) = config.black_lift_profiles.get(key) {
        return lift_from_stored(*value);
    }
    if let Some(value) = config.black_lift_profiles.get(&hz.to_string()) {
        return lift_from_stored(*value);
    }
    if has_scoped_profiles {
        return 0;
    }
    lift_from_stored(config.black_lift)
}

// Re-resolves every connected monitor's Black Crush profile for the mode it
// is in and installs the result in the runtime tables. Returns whether
// anything changed, which the caller uses to decide whether the follow-up
// ramp write also has to touch the monitors that resolve to zero.
//
// The tables are keyed by the resolution-independent monitor id, so a mode
// change does not re-key them: without this pass the outgoing mode's lift
// stays in place and the safety thread re-applies it every five seconds until
// a frontend resolves the incoming mode's profile, which only happens while
// the main webview exists. Resolving here makes the screen follow the mode
// from the tray, with no window open.
fn resolve_profiles(
    state: &GammaState,
    config: &crate::config::AppConfig,
    monitors: &[crate::monitors::ListedMonitor],
) -> bool {
    let previous = state
        .per_monitor
        .lock()
        .map(|map| map.clone())
        .unwrap_or_default();
    let previous_desired = state.desired.load(Ordering::Acquire);
    let mut next: HashMap<String, i32> = HashMap::new();
    let mut primary_lift = None;
    // Black Crush off means every profile resolves to zero, matching the
    // frontend's blackLift() gate; do not resurrect a lift the user disabled.
    if config.black_enabled {
        let has_scoped_profiles = config
            .black_lift_profiles
            .keys()
            .any(|key| !is_legacy_rate_key(key));
        for monitor in monitors {
            let Some(key) = profile_key_for(monitor) else {
                continue;
            };
            let lift = resolve_profile_lift(config, &key, monitor.refresh_hz, has_scoped_profiles);
            if monitor.primary {
                primary_lift = Some(lift);
            }
            next.insert(monitor.profile_id.clone(), lift);
        }
    }
    // The primary's newly resolved value also becomes the global fallback
    // (`desired`): the safety loop's global write targets the primary through
    // `set_display_ramp`, so a stale `desired` would otherwise overwrite the
    // primary's per-monitor ramp on every tick. `black_lift` remains the
    // legacy fallback when the primary's own mode is unknown.
    let desired = if config.black_enabled {
        primary_lift.unwrap_or_else(|| lift_from_stored(config.black_lift))
    } else {
        0
    };
    let changed = next != previous || desired != previous_desired;
    // Replace the table rather than merge into it, so a lift the user turned
    // off, or an entry for a monitor that is gone, cannot be re-applied by a
    // later tick.
    if let Ok(mut map) = state.per_monitor.lock() {
        *map = next;
    }
    state.desired.store(desired, Ordering::Release);
    changed
}

// A display change arrives as a burst (hotplug plus the mode change itself
// broadcast several messages), so each new change takes over the wait from the
// one before it. The generation check makes the superseded wait drop out
// instead of racing the current one.
static REAPPLY_GENERATION: AtomicU64 = AtomicU64::new(0);
// A mode change broadcasts WM_DISPLAYCHANGE when the switch starts, before the
// driver has activated the incoming mode, so the first read after the burst
// can still see the outgoing mode.
const REAPPLY_SETTLE_DELAY: Duration = Duration::from_millis(400);
const REAPPLY_SETTLE_POLL: Duration = Duration::from_millis(150);
// Bounded so a display that keeps flapping (a driver resetting the mode
// repeatedly) cannot spin here.
const REAPPLY_SETTLE_ATTEMPTS: usize = 20;

// The connected monitors' current modes, read until two consecutive reads
// agree. Reading once would resolve against a mode Windows is still
// switching, which is what left the wrong profile in place in the first
// place. Returns the last read when the wait expires, and the previous read
// when a newer change took over.
fn wait_for_settled_modes(generation: u64) -> Vec<crate::monitors::ListedMonitor> {
    let mut previous: Vec<crate::monitors::ListedMonitor> = Vec::new();
    for attempt in 0..REAPPLY_SETTLE_ATTEMPTS {
        std::thread::sleep(if attempt == 0 {
            REAPPLY_SETTLE_DELAY
        } else {
            REAPPLY_SETTLE_POLL
        });
        if REAPPLY_GENERATION.load(Ordering::Acquire) != generation {
            return previous;
        }
        let current = crate::monitors::list_monitors();
        if attempt > 0 && mode_signature(&current) == mode_signature(&previous) {
            return current;
        }
        previous = current;
    }
    previous
}

// Identity plus mode for every monitor, so two reads can be compared for the
// topology and mode changes the profiles are keyed by.
fn mode_signature(monitors: &[crate::monitors::ListedMonitor]) -> Vec<(String, u32, u32, u32)> {
    monitors
        .iter()
        .map(|monitor| {
            (
                monitor.profile_id.clone(),
                monitor.width,
                monitor.height,
                monitor.refresh_hz,
            )
        })
        .collect()
}

/// Re-resolves and re-applies the Black Crush profiles after a display mode or
/// topology change (wired from lib.rs's display-change notification). Without
/// this the screen keeps the outgoing mode's profile: the runtime table is
/// keyed per monitor, the safety thread re-asserts it every five seconds, and
/// only opening the main window (whose frontend resolves the incoming mode's
/// key) ever corrected it. Runs on a worker thread, since the notification
/// itself is delivered on the main thread.
pub(crate) fn reapply_after_display_change(app: &AppHandle) {
    let generation = REAPPLY_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        let monitors = wait_for_settled_modes(generation);
        // A newer change landed while the modes settled; that one owns the
        // re-resolve.
        if REAPPLY_GENERATION.load(Ordering::Acquire) != generation {
            return;
        }
        // Hold-to-compare owns the hardware while it is down, and the safety
        // thread skips its tick for the same reason; the next tick picks the
        // resolved values up once the hold is released.
        if crate::hotkeys::is_hold_compare_active() {
            return;
        }
        let config = crate::config::load_config(app.clone());
        let state = app.state::<GammaState>();
        let changed = resolve_profiles(&state, &config, &monitors);
        let brightness = state
            .screen_dim_brightness
            .lock()
            .map(|map| map.clone())
            .unwrap_or_default();
        // Write now instead of leaving the outgoing mode's ramp on screen until
        // the safety thread's next tick. The forced pass is what clears a mode
        // that resolves to zero (no profile covers it): apply_screen_gamma_ramps
        // leaves a zero lift alone otherwise, and the tick would keep
        // re-asserting the value this pass just replaced. It is only needed
        // when a profile actually changed, so a mode change that keeps the
        // same value never writes a ramp to a monitor we do not manage.
        apply_screen_gamma_ramps(&state.desired, &state.per_monitor, &brightness, changed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Leaked on purpose: GammaState's Drop resets the real display gamma, and
    // a unit test has no business touching the gamma of the machine running
    // it. Each call is a fresh state, and the test process is short lived.
    fn test_state() -> &'static GammaState {
        Box::leak(Box::new(GammaState {
            desired: Arc::new(AtomicI32::new(0)),
            per_monitor: Arc::new(Mutex::new(HashMap::new())),
            screen_dim_brightness: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Arc::new(GammaShutdown {
                requested: AtomicBool::new(false),
                wake: Condvar::new(),
                lock: Mutex::new(()),
            }),
            safety_thread: Mutex::new(None),
        }))
    }

    fn monitor(profile_id: &str, width: u32, height: u32, hz: u32, primary: bool) -> crate::monitors::ListedMonitor {
        crate::monitors::ListedMonitor {
            sig: format!("sig-{profile_id}"),
            friendly_name: "Test Panel".into(),
            primary,
            width,
            height,
            monitor_id: profile_id.into(),
            device_name: format!(r"\\.\{profile_id}"),
            edid_serial: String::new(),
            profile_id: profile_id.into(),
            refresh_hz: hz,
        }
    }

    fn lift_of(state: &GammaState, key: &str) -> i32 {
        *state.per_monitor.lock().unwrap().get(key).unwrap_or(&0)
    }

    #[test]
    fn a_mode_without_a_profile_resolves_to_zero() {
        let state = test_state();
        let mut config = crate::config::AppConfig::default();
        config
            .black_lift_profiles
            .insert("panel-a|2560x1440|120".into(), 1.5);
        // The same panel at a rate nothing is stored for: the outgoing mode's
        // lift must not survive the switch.
        let monitors = vec![monitor("panel-a", 2560, 1440, 60, true)];
        assert!(resolve_profiles(state, &config, &monitors));
        assert_eq!(lift_of(state, "panel-a"), 0);
        assert_eq!(state.desired.load(Ordering::Acquire), 0);
    }

    #[test]
    fn the_incoming_mode_replaces_the_outgoing_one() {
        let state = test_state();
        let mut config = crate::config::AppConfig::default();
        config
            .black_lift_profiles
            .insert("panel-a|2560x1440|60".into(), 1.5);
        config
            .black_lift_profiles
            .insert("panel-a|2560x1440|120".into(), -0.8);
        // A second monitor with no profile of its own resolves to zero and is
        // still tracked, so a later pass can tell a zero from an unknown
        // monitor.
        let monitors = vec![
            monitor("panel-a", 2560, 1440, 60, true),
            monitor("panel-b", 1920, 1080, 60, false),
        ];
        assert!(resolve_profiles(state, &config, &monitors));
        assert_eq!(lift_of(state, "panel-a"), 15);
        assert_eq!(lift_of(state, "panel-b"), 0);
        assert_eq!(state.desired.load(Ordering::Acquire), 15);
        // Same monitors, panel-a switched to 120 Hz.
        let switched = vec![
            monitor("panel-a", 2560, 1440, 120, true),
            monitor("panel-b", 1920, 1080, 60, false),
        ];
        assert!(resolve_profiles(state, &config, &switched));
        assert_eq!(lift_of(state, "panel-a"), -8);
        assert_eq!(state.desired.load(Ordering::Acquire), -8);
        // Re-resolving the same mode changes nothing, so the caller does not
        // force a ramp write onto monitors it does not manage.
        assert!(!resolve_profiles(state, &config, &switched));
    }

    #[test]
    fn legacy_profile_formats_still_resolve() {
        let state = test_state();
        let mut config = crate::config::AppConfig::default();
        // A pre-display-identity config: bare refresh rates plus the flat
        // value, with no display-scoped key anywhere.
        config.black_lift = 0.5;
        config.black_lift_profiles.insert("120".into(), 2.0);
        let monitors = vec![
            monitor("panel-a", 2560, 1440, 120, true),
            monitor("panel-b", 1920, 1080, 60, false),
        ];
        assert!(resolve_profiles(state, &config, &monitors));
        assert_eq!(lift_of(state, "panel-a"), 20);
        assert_eq!(lift_of(state, "panel-b"), 5);
    }

    #[test]
    fn disabling_crush_clears_the_table() {
        let state = test_state();
        let mut config = crate::config::AppConfig::default();
        config
            .black_lift_profiles
            .insert("panel-a|2560x1440|60".into(), 1.5);
        let monitors = vec![monitor("panel-a", 2560, 1440, 60, true)];
        assert!(resolve_profiles(state, &config, &monitors));
        assert_eq!(lift_of(state, "panel-a"), 15);
        config.black_enabled = false;
        assert!(resolve_profiles(state, &config, &monitors));
        assert!(state.per_monitor.lock().unwrap().is_empty());
        assert_eq!(state.desired.load(Ordering::Acquire), 0);
    }

    #[test]
    fn monitors_without_a_readable_mode_are_left_alone() {
        let state = test_state();
        let mut config = crate::config::AppConfig::default();
        config
            .black_lift_profiles
            .insert("panel-a|2560x1440|60".into(), 1.5);
        // A rate the driver did not report: no key can be built, so the entry
        // the previous pass left stays put instead of resolving to zero.
        let monitors = vec![monitor("panel-a", 2560, 1440, 0, true)];
        let changed = resolve_profiles(state, &config, &monitors);
        assert!(!changed);
        assert!(state.per_monitor.lock().unwrap().is_empty());
    }
}
