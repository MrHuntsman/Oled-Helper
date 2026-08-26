//! Gamma ramp application: synchronous apply + periodic safety restore.
//!
//! The frontend calls `apply_gamma_ramp` on every slider change; the ramp is
//! applied immediately (no polling thread, no IPC round-trip delay). A
//! lightweight safety thread re-applies every 5 seconds so another app
//! resetting the system gamma (fullscreen games, driver updates) is corrected
//! without user interaction.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use tauri::State;

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
            if crate::monitors::monitor_key(&monitor) == *key
                && !monitor.device_name.is_empty()
            {
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
    crate::gamma_watch::sync(desired.load(Ordering::Relaxed), &values, brightness_by_monitor);
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
            if monitor.primary { primary_fallback } else { 0 }
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
    crate::hotkeys::update_hold_restore_locked(black_lift);
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
        crate::hotkeys::update_hold_restore_locked(black_lift);
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
