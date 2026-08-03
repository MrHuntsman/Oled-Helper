// Svelte 5 runes stores — $state object pattern
import { invoke } from '@tauri-apps/api/core';

export const app = $state({
  activeTab: 'crush',
  hdrStatus: 'unknown',  // set by pollHDRStatus via matchMedia
  hdrLuminance: null,    // { maxNits } | null, set by pollHDRLuminance
  displayInfo: { width: 0, height: 0, refreshRateHz: null, refreshRates: [], resolutions: [] },
  refreshRateBusy: false,
  displayModeBusy: false,
});

export const crush = $state({
  adjustment: 0,
  // Values are stored in tenths, matching the slider's internal range. The
  // persisted config converts them to the display units shown in the UI.
  blackProfiles: {},
  blackEnabled: true,
  squareCount: 12,
  fullscreen: 'normal',
});

// Config files from before refresh-rate profiles existed only had one
// `black_lift` value. Preserve it for the first detected refresh rate, then
// all subsequently unseen rates intentionally start at 0.
let legacyBlackLift = null;

export const taskbarDimmer = $state({
  enabled: false,
  opacity: 0.95,
  fadeInDurationMs: 300,
  fadeOutDurationMs: 300,
  available: true,
});

export const traySettings = $state({
  enabled: true,
});

export const runtimeLogSettings = $state({
  verbosePeriodic: false,
});

export function adjustmentDisplay() {
  if (!crush.blackEnabled) return 'OFF';
  return `${crush.adjustment > 0 ? '+' : ''}${(crush.adjustment / 10).toFixed(1)}`;
}
export function blackLift() {
  return crush.blackEnabled ? crush.adjustment : 0;
}

function profileKey(hz) {
  const value = Number(hz);
  return Number.isFinite(value) && value > 0 ? String(Math.round(value)) : null;
}

function clampAdjustment(value) {
  return Math.max(-150, Math.min(150, Math.round(Number(value) || 0)));
}

export function blackLevelForRefreshRate(hz) {
  const key = profileKey(hz);
  if (!key) return 0;
  return Object.prototype.hasOwnProperty.call(crush.blackProfiles, key)
    ? clampAdjustment(crush.blackProfiles[key])
    : 0;
}

export function blackLevelDisplayForRefreshRate(hz) {
  const key = profileKey(hz);
  const currentKey = profileKey(app.displayInfo.refreshRateHz);
  // Keep the open menu in sync with a slider drag even before its debounced
  // config save updates the profile map.
  if (key && key === currentKey) {
    const value = crush.adjustment;
    return `${value > 0 ? '+' : ''}${(value / 10).toFixed(1)}`;
  }
  // No saved profile for this rate yet — show nothing rather than a
  // misleading "0.0" that implies a deliberately-set neutral value.
  if (!key || !Object.prototype.hasOwnProperty.call(crush.blackProfiles, key)) return '';
  const value = blackLevelForRefreshRate(hz);
  return `${value > 0 ? '+' : ''}${(value / 10).toFixed(1)}`;
}

// Record the currently displayed value before a mode change or save. This is
// deliberately separate from the gamma effect so dragging remains cheap and
// profile switching cannot accidentally persist a stale value.
export function persistCurrentBlackLevelProfile() {
  const key = profileKey(app.displayInfo.refreshRateHz);
  if (key) crush.blackProfiles[key] = clampAdjustment(crush.adjustment);
}

export function applyBlackLevelProfileForRefreshRate(hz) {
  if (!configLoaded.v) return;
  const key = profileKey(hz);
  if (!key) return;
  if (!Object.prototype.hasOwnProperty.call(crush.blackProfiles, key)) {
    crush.blackProfiles[key] = legacyBlackLift !== null
      ? clampAdjustment(legacyBlackLift)
      : 0;
    legacyBlackLift = null;
  }
  crush.adjustment = blackLevelForRefreshRate(hz);
}

export const configLoaded = $state({ v: false });

// Load persisted config (black_lift in tenths-display units, e.g. 9 means
// +0.9, matching what's shown in the UI/slider) once at startup and apply
// it to the crush store before the UI reacts to it.
export async function loadConfig() {
  try {
    const cfg = await invoke('load_config');
    let loadedProfileCount = 0;
    if (cfg && cfg.black_lift_profiles && typeof cfg.black_lift_profiles === 'object') {
      const profiles = {};
      for (const [hz, value] of Object.entries(cfg.black_lift_profiles)) {
        if (profileKey(hz) && typeof value === 'number' && Number.isFinite(value)) {
          profiles[profileKey(hz)] = clampAdjustment(value * 10);
          loadedProfileCount++;
        }
      }
      crush.blackProfiles = profiles;
    }
    // Only migrate the old single value when no per-rate values were loaded.
    // New configs retain black_lift as a compatibility field, so using it
    // unconditionally would incorrectly seed every newly encountered Hz.
    if (loadedProfileCount === 0 && cfg && typeof cfg.black_lift === 'number' && Number.isFinite(cfg.black_lift)) {
      legacyBlackLift = clampAdjustment(cfg.black_lift * 10);
    }
    if (cfg && typeof cfg.black_enabled === 'boolean') {
      crush.blackEnabled = cfg.black_enabled;
    }
    if (cfg && typeof cfg.taskbar_dimmer_enabled === 'boolean') {
      taskbarDimmer.enabled = cfg.taskbar_dimmer_enabled;
    }
    if (cfg && typeof cfg.taskbar_dimmer_opacity === 'number') {
      taskbarDimmer.opacity = Math.max(0, Math.min(1, cfg.taskbar_dimmer_opacity));
    }
    if (cfg && typeof cfg.taskbar_dimmer_fade_in_duration_ms === 'number') {
      taskbarDimmer.fadeInDurationMs = Math.max(0, Math.min(1000, Math.round(cfg.taskbar_dimmer_fade_in_duration_ms)));
    }
    if (cfg && typeof cfg.taskbar_dimmer_fade_out_duration_ms === 'number') {
      taskbarDimmer.fadeOutDurationMs = Math.max(0, Math.min(1000, Math.round(cfg.taskbar_dimmer_fade_out_duration_ms)));
    }
    if (cfg && typeof cfg.tray_enabled === 'boolean') {
      traySettings.enabled = cfg.tray_enabled;
    }
    try {
      const status = await invoke('get_taskbar_dimmer_status');
      if (status) taskbarDimmer.available = status.available !== false;
    } catch (e) {
      taskbarDimmer.available = false;
    }
    try {
      await invoke('set_taskbar_dimmer', {
        enabled: taskbarDimmer.enabled,
        opacity: taskbarDimmer.opacity,
        fadeInDurationMs: taskbarDimmer.fadeInDurationMs,
        fadeOutDurationMs: taskbarDimmer.fadeOutDurationMs,
        immediate: true,
      });
    } catch (e) {
      taskbarDimmer.enabled = false;
    }
  } catch (e) {
    console.warn('[config] load failed', e);
  } finally {
    // Gate saveConfig() until after the initial load resolves, so TabCrush's
    // mount-time effect (which fires with the default adjustment=0 before
    // this async load completes) doesn't immediately overwrite the file
    // we just read from.
    configLoaded.v = true;
    applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
  }
}

// Fire-and-forget save, called whenever the black level or enabled state
// changes. black_lift is written in the same units shown in the UI (tenths
// of crush.adjustment), so the config file reads e.g. 9 instead of 90.
export function saveConfig() {
  if (!configLoaded.v) return;
  persistCurrentBlackLevelProfile();
  const blackLiftProfiles = Object.fromEntries(
    Object.entries(crush.blackProfiles).map(([hz, value]) => [hz, clampAdjustment(value) / 10]),
  );
  invoke('save_config', { config: {
    // Keep black_lift as a compatibility field for older builds; new builds
    // use black_lift_profiles for the per-refresh-rate values.
    black_lift: crush.adjustment / 10,
    black_lift_profiles: blackLiftProfiles,
    black_enabled: crush.blackEnabled,
    taskbar_dimmer_enabled: taskbarDimmer.enabled,
    taskbar_dimmer_opacity: taskbarDimmer.opacity,
    taskbar_dimmer_fade_in_duration_ms: taskbarDimmer.fadeInDurationMs,
    taskbar_dimmer_fade_out_duration_ms: taskbarDimmer.fadeOutDurationMs,
    tray_enabled: traySettings.enabled,
  } }).catch((e) => {
    console.warn('[config] save failed', e);
  });
}
export async function setTrayEnabled(enabled) {
  const previous = traySettings.enabled;
  traySettings.enabled = enabled;
  try {
    await invoke('set_tray_enabled', { enabled });
    saveConfig();
  } catch (e) {
    console.warn('[tray] update failed', e);
    traySettings.enabled = previous;
  }
}

let pendingTaskbarDimmerUpdate = null;
let taskbarDimmerUpdateInFlight = false;
let taskbarDimmerUpdateVersion = 0;

function flushTaskbarDimmerUpdate() {
  if (taskbarDimmerUpdateInFlight || !pendingTaskbarDimmerUpdate) return;
  const update = pendingTaskbarDimmerUpdate;
  pendingTaskbarDimmerUpdate = null;
  taskbarDimmerUpdateInFlight = true;

  invoke('set_taskbar_dimmer', {
    enabled: update.enabled,
    opacity: update.opacity,
    fadeInDurationMs: update.fadeInDurationMs,
    fadeOutDurationMs: update.fadeOutDurationMs,
    immediate: update.immediate,
  }).then(() => {
    if (update.persist && update.version === taskbarDimmerUpdateVersion) saveConfig();
  }).catch((e) => {
    console.warn('[taskbar-dimmer] update failed', e);
    if (update.version === taskbarDimmerUpdateVersion) taskbarDimmer.enabled = false;
  }).finally(() => {
    taskbarDimmerUpdateInFlight = false;
    flushTaskbarDimmerUpdate();
  });
}

export function setTaskbarDimmer(enabled = taskbarDimmer.enabled, opacity = taskbarDimmer.opacity, fadeInDurationMs = taskbarDimmer.fadeInDurationMs, fadeOutDurationMs = taskbarDimmer.fadeOutDurationMs, persist = true, immediate = false) {
  taskbarDimmer.enabled = enabled;
  taskbarDimmer.opacity = Math.max(0, Math.min(1, opacity));
  taskbarDimmer.fadeInDurationMs = Math.max(0, Math.min(1000, Math.round(fadeInDurationMs)));
  taskbarDimmer.fadeOutDurationMs = Math.max(0, Math.min(1000, Math.round(fadeOutDurationMs)));
  taskbarDimmerUpdateVersion++;
  pendingTaskbarDimmerUpdate = {
    enabled: taskbarDimmer.enabled,
    opacity: taskbarDimmer.opacity,
    fadeInDurationMs: taskbarDimmer.fadeInDurationMs,
    fadeOutDurationMs: taskbarDimmer.fadeOutDurationMs,
    persist,
    immediate,
    version: taskbarDimmerUpdateVersion,
  };
  flushTaskbarDimmerUpdate();
}

export function restoreTaskbarDimmerDefaults() {
  setTaskbarDimmer(false, 0.95, 300, 300, true, true);
}

// The taskbar dimmer overlay is an always-on-top native window over the
// taskbar. While the calibration strip is in monitor fullscreen that overlay
// keeps sitting on top of the taskbar, and Windows won't auto-hide the
// taskbar while a visible topmost window covers it — so the taskbar stays
// rendered on top of the fullscreen strip. Ask the Rust overlay to hide
// itself for the duration (dimmer settings stay untouched); it reappears on
// its own once fullscreen ends. Only 'monitor' fullscreen needs this: in-app
// fullscreen keeps the OS window/taskbar intact.
export function setDimmerSuspendedForFullscreen(suspended) {
  invoke('set_taskbar_dimmer_suspended', { suspended }).catch(() => {});
}

export function setVerbosePeriodicLogging(enabled) {
  runtimeLogSettings.verbosePeriodic = enabled;
  invoke('set_taskbar_dimmer_verbose_logging', { enabled }).catch((e) => {
    console.warn('[runtime-log] verbose logging update failed', e);
  });
}

export function accentColor() {
  return app.hdrStatus === 'hdr' ? '#FFCC00' : '#4CA6FF';
}

export function pollHDRStatus() {
  try {
    const mq = window.matchMedia('(dynamic-range: high)');
    app.hdrStatus = mq.matches ? 'hdr' : 'sdr';
  } catch {
    // matchMedia not available — stay unknown (very old browser)
  }
}

export function pollHDRLuminance() {
  invoke('get_hdr_luminance')
    .then((info) => {
      if (!info) {
        app.hdrLuminance = null;
        return;
      }
      app.hdrLuminance = {
        maxNits: info.max_nits ?? null,
      };
    })
    .catch(() => {
      app.hdrLuminance = null;
    });
}

let displayPollSequence = 0;

export function pollDisplayInfo() {
  const requestSequence = ++displayPollSequence;
  const s = window.screen;
  if (!s) return;
  const dpr = window.devicePixelRatio || 1;
  // Fall back to browser-reported size immediately so the UI has something
  // to show while the async Tauri call resolves.
  app.displayInfo = {
    width: Math.round(s.width * dpr),
    height: Math.round(s.height * dpr),
    refreshRateHz: app.displayInfo.refreshRateHz ?? null,
    refreshRates: app.displayInfo.refreshRates,
    resolutions: app.displayInfo.resolutions,
  };
  // Browsers don't expose real refresh rate, so ask the Tauri backend
  // (Win32 EnumDisplaySettingsW) for accurate resolution + Hz.
  invoke('get_display_info').then((info) => {
    // Polls can overlap while Windows is changing display modes. Only let
    // the newest request update the dropdown, otherwise a slower old result
    // could snap the selection back after a successful change.
    if (requestSequence !== displayPollSequence || !info) return;
      const modeChangeBusy = app.displayModeBusy || app.refreshRateBusy;
    const nextRefreshRate = modeChangeBusy
      ? app.displayInfo.refreshRateHz
      : (info.refresh_hz || null);
    const previousRefreshRate = app.displayInfo.refreshRateHz;
    if (nextRefreshRate && nextRefreshRate !== previousRefreshRate) {
      persistCurrentBlackLevelProfile();
    }
    app.displayInfo = {
      // Keep the user's pending choice visible until Windows finishes the
      // mode switch and the next poll confirms the actual value.
      width: app.displayModeBusy ? app.displayInfo.width : info.width,
      height: app.displayModeBusy ? app.displayInfo.height : info.height,
      refreshRateHz: nextRefreshRate,
      refreshRates: Array.isArray(info.refresh_rates) ? info.refresh_rates : [],
      resolutions: Array.isArray(info.resolutions) ? info.resolutions : [],
    };
    if (nextRefreshRate && nextRefreshRate !== previousRefreshRate) {
      applyBlackLevelProfileForRefreshRate(nextRefreshRate);
    }
  }).catch(() => {});
}

// Plain tick counter bumped alongside the poll timer in App.svelte, so any
// component can derive off it to re-read non-reactive globals (navigator.*,
// screen.*, devicePixelRatio) on a schedule without needing its own timer.
export const envTick = $state({ n: 0 });
export function bumpEnvTick() {
  envTick.n++;
}

// ── Runtime log — plain (non-reactive) ring buffer + a reactive version counter.
// Entries are retained independently of the Debug tab. The reactive version
// only changes while Debug is open, so logging continues in the background
// without causing UI work or rerenders in other tabs.
//
// ⚠️ KNOWN FOOTGUN (caused a real effect_update_depth_exceeded crash — see git
// history / TabCrush.svelte around initGPU): pushGpuLog() writes gpuLogVersion.n,
// a $state value. NEVER call pushGpuLog / dlog / dwarn / derr synchronously from
// inside a $effect body's own execution (e.g. as the first line of an $effect,
// or anywhere before the first `await` in a function an $effect calls directly).
// That write-inside-an-effect's-own-tracked-scope is exactly the "effect reads
// and writes the same piece of state" pattern Svelte's loop guard trips on —
// it doesn't need to be gpuLogVersion specifically being *read* by that effect;
// the repeated re-entrant write alone is enough to blow the update-depth limit.
// Safe: calling these from inside async functions after an await, from event
// handlers (oninput/onclick/etc), or from setTimeout/setInterval callbacks.
// Unsafe: calling these as the first synchronous statement in an $effect(() => {...}).
// If you need to log effect-fire diagnostics for debugging, use plain
// console.log/console.warn there instead — they don't touch reactive state.
// 5,000 entries is a bounded history large enough for long debugging sessions
// without allowing an unattended app to grow memory forever. Since the array
// is plain and the reactive version is gated below, inactive tabs pay only the
// small cost of storing each compact log record.
const GPU_LOG_MAX = 5000;
export const gpuLogEntries = [];
export const gpuLogVersion = $state({ n: 0 });

export function pushGpuLog(level, message, { persist = true } = {}) {
  gpuLogEntries.push({ level, message, ts: Date.now() });
  if (gpuLogEntries.length > GPU_LOG_MAX) gpuLogEntries.shift();
  // Only invalidate the Debug tab while it is mounted/open. In other tabs the
  // plain buffer still retains the entry, but Svelte does no log-list work.
  if (app.activeTab === 'debug') gpuLogVersion.n++;
  // Best-effort mirror to the on-disk log (see src-tauri log_frontend command)
  // so the runtime sequence survives even if the app hangs/crashes before
  // the user can open the in-app Debug tab. Set persist=false when the caller
  // is already being mirrored by main.js to avoid duplicate file entries.
  if (persist) {
    try { invoke('log_frontend', { level, message }).catch(() => {}); } catch (_) {}
  }
}

export function clearGpuLog() {
  gpuLogEntries.length = 0;
  if (app.activeTab === 'debug') gpuLogVersion.n++;
}
