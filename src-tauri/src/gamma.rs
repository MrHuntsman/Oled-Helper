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
    // Per-monitor black-lift values keyed by the monitor's stable signature
    // (see monitors::AppMonitor::sig). The safety thread re-applies these to
    // their own devices alongside the global value, and shutdown resets every
    // affected monitor. Entries may contain an explicit zero so the screen
    // protector can distinguish it from an uninitialized profile.
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
pub(crate) fn reset_per_monitor_ramps(per_monitor: &Mutex<HashMap<String, i32>>) {
    let Ok(map) = per_monitor.lock() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    let devices = crate::monitors::monitor_device_names();
    for sig in map.keys() {
        if let Some(device) = devices.get(sig) {
            unsafe {
                crate::gamma_ramp::reset_monitor_ramp(device);
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
    let primary_fallback = desired.load(Ordering::Relaxed);
    for monitor in crate::monitors::all_monitors() {
        if monitor.device_name.is_empty() {
            continue;
        }
        let sig = monitor.sig();
        let brightness_thousandths = brightness_by_monitor.get(&sig).copied().unwrap_or(1000);
        let brightness = (brightness_thousandths.min(1000) as f32) / 1000.0;
        let dimmed = brightness_thousandths < 1000;
        // The primary fallback preserves the existing global Black Crush
        // value during startup. Every write below is still per-monitor; the
        // global SetDeviceGammaRamp path is never used for screen dimming.
        let lift = values.get(&sig).copied().unwrap_or_else(|| {
            if monitor.primary { primary_fallback } else { 0 }
        });
        let has_profile = values.contains_key(&sig);
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
                .and_then(|m| brightness.get(&m.sig()).copied())
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
    Ok(())
}

// Same as apply_gamma_ramp but targets a specific monitor (by stable
// signature) instead of the primary. Used by per-monitor Black Crush when the
// frontend has a monitor selected; the primary monitor goes through the global
// path above. The per-monitor table keeps explicit zero values too.
#[tauri::command]
pub fn apply_gamma_ramp_for(state: State<GammaState>, sig: String, black_lift: i32) -> Result<(), String> {
    let black_lift = black_lift.clamp(-150, 150);
    if crate::vendor_info::nvidia_reference_mode_enabled() {
        return Err("NVIDIA Override to reference mode is enabled; disable it in your NVIDIA driver settings under System → Displays to use Black Crush Tweak".into());
    }
    let _gamma_guard = crate::hotkeys::gamma_write_guard();
    if !crate::hotkeys::is_hold_compare_active() {
        let device = crate::monitors::monitor_device_names()
            .get(&sig)
            .cloned()
            .ok_or_else(|| format!("no display device for monitor {sig}"))?;
        let brightness = state
            .screen_dim_brightness
            .lock()
            .ok()
            .and_then(|map| map.get(&sig).copied())
            .unwrap_or(1000);
        let ramp = crate::gamma_ramp::build_dimmed_ramp(black_lift, brightness as f32 / 1000.0);
        let applied = unsafe { crate::gamma_ramp::set_monitor_ramp(&device, &ramp) };
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
        map.insert(sig, black_lift);
    }
    Ok(())
}
