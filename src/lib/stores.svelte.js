// Svelte 5 runes stores ,  $state object pattern
import { invoke } from '@tauri-apps/api/core';
import { HIGH_NITS_DEFAULT_MIN, HIGH_NITS_DEFAULT_STEP, HIGH_NITS_DEFAULT_MAX, HIGH_NITS_CELLS, HIGH_NITS_MAX_CEILING, HIGH_NITS_MAX_STOPS, highNitsStepForMax } from './calibration.js';

export const app = $state({
  activeTab: 'crush',
  hdrStatus: 'unknown',
  hdrLuminance: null,
  // Windows SDR white level in nits (80–480), read live for System Info and
  // the Windows HDR tab.
  sdrWhiteNits: 80,
  // The SDR white level the browser's HDR compositor captured at WebView
  // startup and froze for the session. The Brightness strip must encode
  // against THIS frozen value ,  Chromium/WebView2 scales the HDR canvas
  // relative to it and only re-reads it on restart, so compensating against
  // the live slider makes the strip drift whenever the SDR brightness moves.
  // Captured on the first successful read (null → fallback 80).
  sdrWhiteBaseNits: null,
  displayInfo: { width: 0, height: 0, deviceName: '', monitorName: '', monitorId: '', nativeWidth: 0, nativeHeight: 0, nativeSource: '', gpuModel: '', vendor: '', connector: '', colorFormat: '', bitsPerChannel: 0, desktopBits: 0, dynamicRange: '', vrr: '', referenceMode: false, refreshRateHz: null, refreshRates: [], resolutions: [] },
  refreshRateBusy: false,
  displayModeBusy: false,
  // Actual canvas rendering path reported by the WebGPU calibration surfaces.
  // This is diagnostic state only; it never controls HDR status or rendering.
  gpuRendering: {
    main: {},
    comparison: {},
  },
});

// Keep diagnostics per canvas. The main calibration canvas and the
// Windows HDR comparison canvas can initialize at different times; a single
// shared record would make System Info report whichever one initialized last.
export function setGpuRenderingInfo(source, info = {}) {
  if (source !== 'main' && source !== 'comparison') return;
  const target = app.gpuRendering[source];
  for (const key of ['api', 'runtime', 'adapter', 'format', 'colorSpace', 'toneMapping', 'hdrPath']) {
    if (typeof info[key] === 'string') target[key] = info[key];
  }
}
export function clearGpuRenderingInfo(source) {
  if (source === 'main' || source === 'comparison') app.gpuRendering[source] = {};
}

export const crush = $state({
  adjustment: 0,
  blackProfiles: {},
  blackEnabled: true,
  // Which calibration strip is active ('black' | 'highNits'). Session-only
  // (never persisted) ,  every launch starts on Black.
  stripType: 'black',
  // Per-strip square counts (zoom), session-only like squareCount was.
  // Only the Black strip zooms ,  the High Nits grid is always fixed.
  squareCounts: { black: 12, highNits: 8 },
  // High Nits strip range. The grid is always a fixed 3 rows × 7 columns
  // (21 cells) with no zoom; Min is fixed and the user picks Max via a
  // single slider. Step (nits between neighboring cells) derives silently
  // from Max on a round-number ladder (50/100/250/500/1000/...) below.
  // Persisted in the config file.
  highNitsMin: HIGH_NITS_DEFAULT_MIN,
  highNitsMax: HIGH_NITS_DEFAULT_MAX,
  highNitsStep: HIGH_NITS_DEFAULT_STEP,
  fullscreen: 'normal',
  // Hide the calibration strip interface (labels, controls) ,  right-click
  // menu on the strip. The info tooltip and fullscreen slider only hide in
  // fullscreen modes; the rest of the interface hides everywhere.
  hideCalibrationInterface: false,
  // Small gap between cells on the Black strip (right-click menu toggle).
  // On by default, matching the strip's existing look; the High Nits grid
  // is always edge-to-edge regardless of this setting.
  blackStripGapEnabled: true,
});

export function activeSquareCount() {
  return crush.squareCounts[crush.stripType] ?? 12;
}

export function setStripType(type) {
  if (type === 'black' || type === 'highNits') crush.stripType = type;
}

// High Nits range editing. The grid is a fixed 21 cells ending exactly at
// the Max, with region steps (see calibration.js highNitsGridCodes): 1000-
// step above 5000, 500-step from 1000 to 5000, 100-step below (finer when
// the Max is too low for 21 distinct round values). The user controls a
// single Max slider that rests on HIGH_NITS_MAX_STOPS; Step is derived from
// Max via the region rule (highNitsStepForMax) ,  never dragged directly.
const NITS_MIN_FLOOR = HIGH_NITS_DEFAULT_MIN;

function nearestStop(value) {
  let best = HIGH_NITS_MAX_STOPS[0];
  let bestDist = Infinity;
  for (const stop of HIGH_NITS_MAX_STOPS) {
    const dist = Math.abs(stop - value);
    if (dist < bestDist) { bestDist = dist; best = stop; }
  }
  return best;
}

export function highNitsMaxValue() {
  return crush.highNitsMax;
}

// Kept only so any remaining caller (e.g. an old persisted config shape)
// has somewhere safe to land ,  Min is no longer user-editable. Silently a
// no-op besides re-deriving step/max, since Min is always NITS_MIN_FLOOR.
export function setHighNitsMin(_value) {
  crush.highNitsMin = NITS_MIN_FLOOR;
  crush.highNitsStep = highNitsStepForMax(crush.highNitsMax);
  scheduleNitsRangeSave();
}
export function setHighNitsMax(value) {
  const raw = Math.max(HIGH_NITS_MAX_STOPS[0], Math.min(HIGH_NITS_MAX_CEILING, Number(value) || HIGH_NITS_MAX_CEILING));
  // Snap directly to the nearest slider stop ,  the slider only ever rests on
  // a stop, so every position is a clean round Max (100/500/1000-step).
  crush.highNitsMax = nearestStop(raw);
  crush.highNitsStep = highNitsStepForMax(crush.highNitsMax);
  scheduleNitsRangeSave();
}
// Re-clamps max/step against the stops and fixes Min to the floor ,  used
// after loading a persisted config, since an old save (from the previous
// uniform-step design, or a future manual edit of the config file) could
// have Min away from the floor or a Max that isn't on a slider stop.
function clampHighNitsRange() {
  crush.highNitsMin = NITS_MIN_FLOOR;
  const raw = Math.max(HIGH_NITS_MAX_STOPS[0], Math.min(HIGH_NITS_MAX_CEILING, crush.highNitsMax || HIGH_NITS_MAX_CEILING));
  crush.highNitsMax = nearestStop(raw);
  crush.highNitsStep = highNitsStepForMax(crush.highNitsMax);
}
// Debounced like setPollIntervalMs below: a slider fires many oninput events
// per drag (unlike the old +/- buttons, which only ever fired on discrete
// clicks or wheel ticks), so saving on every one would spam the config-file
// write. crush.highNitsMin/Step update immediately above ,  only the disk
// write and the invoke() call wait for the drag to settle.
let nitsRangeSaveTimer = null;
function scheduleNitsRangeSave() {
  clearTimeout(nitsRangeSaveTimer);
  nitsRangeSaveTimer = setTimeout(() => {
    nitsRangeSaveTimer = null;
    saveConfig();
  }, 400);
}

let legacyBlackLift = null;
// Legacy configs used one profile per Hz. Keep those values as a fallback while
// new edits are written into display-specific keys; this allows existing users
// to upgrade without losing their calibration.
let legacyBlackProfiles = {};

export const taskbarDimmer = $state({
  enabled: false,
  opacity: 0.95,
  fadeInDurationMs: 300,
  fadeOutDurationMs: 300,
  available: true,
  hoverDetection: 'event',
  pollIntervalMs: 100,
});

export const traySettings = $state({ enabled: true });
// Debug-tab logging switches, independent of each other:
//   verbosePeriodic ,  vendor/EDID detailed diagnostics (set_detailed_logging)
//   dimmerVerbose   ,  taskbar dimmer tick/winevent/fade verbose logs
//                     (set_taskbar_dimmer_verbose_logging)
export const runtimeLogSettings = $state({ verbosePeriodic: false, dimmerVerbose: false });
export const debugSettings = $state({ contextMenuEnabled: false });
// DWM EOTF gamma override (Experimental tab): the last successfully applied
// profile ("srgb"/"2.2"/"2.4"/"2.6", empty = no override) and whether the
// backend watcher should re-apply it after DWM restarts. Persisted in the
// config so the override survives app relaunches too.
export const dwmEotf = $state({ profile: '', autoReapply: true });

// Global hotkey bindings: action name → combo string ("Ctrl+Alt+F9", or
// "None" for unbound). Loaded from config on startup; changes go through
// setHotkey, which registers the combo in Rust (RegisterHotKey) and persists.
export const hotkeys = $state({});
// Save is deferred briefly after a bind change: the capture flow can fire
// several updates in quick succession (bind, then clear) and each save
// rewrites the whole config file.
let hotkeySaveTimer = null;
export function scheduleHotkeySave() {
  clearTimeout(hotkeySaveTimer);
  hotkeySaveTimer = setTimeout(() => saveConfig(), 300);
}
export async function setHotkey(action, combo) {
  const previous = hotkeys[action] || 'None';
  hotkeys[action] = combo;
  try {
    await invoke('set_hotkey', { action, combo });
    scheduleHotkeySave();
    return { ok: true };
  } catch (e) {
    // Registration failed (e.g. the combo is already in use) ,  revert the UI
    // to the previous binding so it never shows a combo that isn't live.
    if (previous === 'None') delete hotkeys[action];
    else hotkeys[action] = previous;
    return { ok: false, error: String(e) };
  }
}
export async function resetHotkeys() {
  try {
    await invoke('reset_hotkeys');
  } catch (e) {
    console.warn('[hotkeys] reset failed', e);
  }
  for (const action of Object.keys(hotkeys)) delete hotkeys[action];
  scheduleHotkeySave();
}

// Sort/grouping/tag preferences for the custom resolution and refresh-rate
// dropdowns, set via their right-click context menus. Persisted in the
// config file (matching display_menu_* fields in the Rust AppConfig) so the
// list sorting, grouping and tag options survive restarts.
export const displayMenuPrefs = $state({
  resolutionSort: 'desc',       // 'asc' | 'desc'
  resolutionGroupBy: 'none',    // 'none' | 'aspectRatio'
  showTags: true,               // resolution name tags (4K, QHD, UW badge, etc.)
  refreshRateSort: 'desc',      // 'asc' | 'desc'
});
export function setResolutionSort(dir) { displayMenuPrefs.resolutionSort = dir; saveConfig(); }
export function setResolutionGroupBy(mode) { displayMenuPrefs.resolutionGroupBy = mode; saveConfig(); }
export function setShowTags(enabled) { displayMenuPrefs.showTags = enabled; saveConfig(); }
export function setRefreshRateSort(dir) { displayMenuPrefs.refreshRateSort = dir; saveConfig(); }

export const dimmerDetections = $state({
  fullscreen: true,
  taskbarCache: true,
  zorder: true,
  cursor: false,
  mouseHook: true,
});

function detectionDefaultsForMode(mode) {
  const event = mode === 'event';
  return { fullscreen: true, taskbarCache: true, zorder: true, cursor: !event, mouseHook: event };
}

// Debug-tab A/B toggles are fully independent: disabling the mouse hook does
// NOT auto-enable cursor polling here. (The normal Detection Method switch in
// the Taskbar Dimmer tab re-syncs both defaults on its own path.)
export function setDimmerDetection(name, enabled) {
  dimmerDetections[name] = enabled;
  invoke('set_taskbar_dimmer_detection', { name, enabled }).catch((e) => {
    console.warn('[dimmer-detection] toggle failed', e);
    dimmerDetections[name] = !enabled;
  });
}

export function bumpDebugEnv() { envTick.n++; }
export function adjustmentDisplay() {
  if (!crush.blackEnabled) return 'OFF';
  return `${crush.adjustment > 0 ? '+' : ''}${(crush.adjustment / 10).toFixed(1)}`;
}
export function blackLift() { return crush.blackEnabled ? crush.adjustment : 0; }
function hzKey(hz) {
  const value = Number(hz);
  return Number.isFinite(value) && value > 0 ? String(Math.round(value)) : null;
}
function displayIdentity(info = app.displayInfo) {
  const identity = String(info.monitorId || info.deviceName || '').trim();
  return identity || 'unknown-display';
}
function profileKey(hz, info = app.displayInfo) {
  const hzPart = hzKey(hz);
  if (!hzPart || !info.width || !info.height) return null;
  // Monitor identity + active mode makes profiles independent across both
  // physical/virtual displays and resolutions at the same refresh rate.
  return `${displayIdentity(info)}|${info.width}x${info.height}|${hzPart}`;
}
function clampAdjustment(value) { return Math.max(-150, Math.min(150, Math.round(Number(value) || 0))); }
function profileValue(key) {
  return key && Object.prototype.hasOwnProperty.call(crush.blackProfiles, key)
    ? clampAdjustment(crush.blackProfiles[key])
    : null;
}
export function blackLevelForRefreshRate(hz) {
  const key = profileKey(hz);
  const value = profileValue(key);
  if (value !== null) return value;
  const legacyKey = hzKey(hz);
  return legacyKey && Object.prototype.hasOwnProperty.call(legacyBlackProfiles, legacyKey)
    ? clampAdjustment(legacyBlackProfiles[legacyKey])
    : 0;
}
export function blackLevelDisplayForRefreshRate(hz) {
  const key = profileKey(hz);
  const currentKey = profileKey(app.displayInfo.refreshRateHz);
  const value = key && key === currentKey
    ? crush.adjustment
    : profileValue(key) ?? (hzKey(hz) && legacyBlackProfiles[hzKey(hz)]);
  if (!value) return '';
  return `${value > 0 ? '+' : ''}${(value / 10).toFixed(1)}`;
}
export function setHideCalibrationInterface(enabled) { crush.hideCalibrationInterface = enabled; saveConfig(); }
export function setBlackStripGapEnabled(enabled) { crush.blackStripGapEnabled = enabled; saveConfig(); }
export function persistCurrentBlackLevelProfile() {
  const key = profileKey(app.displayInfo.refreshRateHz);
  if (key) crush.blackProfiles[key] = clampAdjustment(crush.adjustment);
}
export function applyBlackLevelProfileForRefreshRate(hz) {
  if (!configLoaded.v) return;
  const key = profileKey(hz);
  if (!key) return;
  const existing = profileValue(key);
  if (existing !== null) {
      crush.adjustment = existing;
    invoke('apply_gamma_ramp', { blackLift: blackLift() }).catch((e) => console.warn('[profile] gamma apply failed', e));
    return;
  }
  const legacyKey = hzKey(hz);
  const fallback = legacyKey && Object.prototype.hasOwnProperty.call(legacyBlackProfiles, legacyKey)
    ? clampAdjustment(legacyBlackProfiles[legacyKey])
    : (legacyBlackLift !== null ? clampAdjustment(legacyBlackLift) : 0);
  crush.blackProfiles[key] = fallback;
  legacyBlackLift = null;
  crush.adjustment = fallback;
  // Profile application is owned here rather than by TabCrush, so a display
  // change while another tab is open still updates the primary display's gamma
  // immediately. The backend safely targets Windows' current primary display.
  invoke('apply_gamma_ramp', { blackLift: blackLift() }).catch((e) => console.warn('[profile] gamma apply failed', e));
}

export function currentBlackLevelProfileKey() {
  return profileKey(app.displayInfo.refreshRateHz);
}

export const configLoaded = $state({ v: false });
export async function loadConfig(prefetchedCfg = null) {
  try {
    const cfg = prefetchedCfg ?? await invoke('load_config');
    let loadedProfileCount = 0;
    legacyBlackProfiles = {};
    if (cfg && cfg.black_lift_profiles && typeof cfg.black_lift_profiles === 'object') {
      const profiles = {};
      for (const [key, value] of Object.entries(cfg.black_lift_profiles)) {
        if (typeof value !== 'number' || !Number.isFinite(value)) continue;
        // A bare numeric key is the pre-display-identity format. Preserve it
        // only as a migration fallback; all newly saved keys are scoped.
        if (/^\d+$/.test(key)) legacyBlackProfiles[key] = clampAdjustment(value * 10);
        else { profiles[key] = clampAdjustment(value * 10); loadedProfileCount++; }
      }
      crush.blackProfiles = profiles;
    }
    if (loadedProfileCount === 0 && cfg && typeof cfg.black_lift === 'number' && Number.isFinite(cfg.black_lift)) legacyBlackLift = clampAdjustment(cfg.black_lift * 10);
    if (cfg && typeof cfg.black_enabled === 'boolean') crush.blackEnabled = cfg.black_enabled;
    if (cfg && typeof cfg.hide_calibration_interface === 'boolean') crush.hideCalibrationInterface = cfg.hide_calibration_interface;
    // Legacy migration: the old calibration_text flag meant "show text".
    else if (typeof cfg.calibration_text === 'boolean') crush.hideCalibrationInterface = !cfg.calibration_text;
    if (cfg && typeof cfg.black_strip_gap_enabled === 'boolean') crush.blackStripGapEnabled = cfg.black_strip_gap_enabled;
    if (cfg && typeof cfg.high_nits_min === 'number' && Number.isFinite(cfg.high_nits_min)) crush.highNitsMin = cfg.high_nits_min;
    if (cfg && typeof cfg.high_nits_max === 'number' && Number.isFinite(cfg.high_nits_max)) crush.highNitsMax = cfg.high_nits_max;
    else if (cfg && typeof cfg.high_nits_step === 'number' && Number.isFinite(cfg.high_nits_step)) {
      // Migrating an older config that only stored min/step: derive max the
      // way the old 3-slider design did, so existing users see the same
      // range they had before this control was simplified to min/max.
      crush.highNitsMax = crush.highNitsMin + cfg.high_nits_step * (HIGH_NITS_CELLS - 1);
    }
    clampHighNitsRange();
    if (cfg && typeof cfg.taskbar_dimmer_enabled === 'boolean') taskbarDimmer.enabled = cfg.taskbar_dimmer_enabled;
    if (cfg && typeof cfg.taskbar_dimmer_opacity === 'number') taskbarDimmer.opacity = Math.max(0, Math.min(1, cfg.taskbar_dimmer_opacity));
    if (cfg && typeof cfg.taskbar_dimmer_fade_in_duration_ms === 'number') taskbarDimmer.fadeInDurationMs = Math.max(0, Math.min(1000, Math.round(cfg.taskbar_dimmer_fade_in_duration_ms)));
    if (cfg && typeof cfg.taskbar_dimmer_fade_out_duration_ms === 'number') taskbarDimmer.fadeOutDurationMs = Math.max(0, Math.min(1000, Math.round(cfg.taskbar_dimmer_fade_out_duration_ms)));
    if (cfg && typeof cfg.tray_enabled === 'boolean') traySettings.enabled = cfg.tray_enabled;
    if (cfg && typeof cfg.taskbar_dimmer_hover_detection === 'string') {
      taskbarDimmer.hoverDetection = cfg.taskbar_dimmer_hover_detection;
      Object.assign(dimmerDetections, detectionDefaultsForMode(cfg.taskbar_dimmer_hover_detection));
      invoke('set_taskbar_dimmer_hover_mode', { mode: cfg.taskbar_dimmer_hover_detection }).catch(() => {});
    }
    if (cfg && typeof cfg.taskbar_dimmer_poll_interval_ms === 'number' && Number.isFinite(cfg.taskbar_dimmer_poll_interval_ms)) {
      taskbarDimmer.pollIntervalMs = Math.max(16, Math.min(200, Math.round(cfg.taskbar_dimmer_poll_interval_ms)));
      invoke('set_taskbar_dimmer_poll_interval', { ms: taskbarDimmer.pollIntervalMs }).catch(() => {});
    }
    if (cfg && (cfg.display_menu_resolution_sort === 'asc' || cfg.display_menu_resolution_sort === 'desc')) displayMenuPrefs.resolutionSort = cfg.display_menu_resolution_sort;
    if (cfg && (cfg.display_menu_resolution_group_by === 'none' || cfg.display_menu_resolution_group_by === 'aspectRatio')) displayMenuPrefs.resolutionGroupBy = cfg.display_menu_resolution_group_by;
    if (cfg && typeof cfg.display_menu_show_tags === 'boolean') displayMenuPrefs.showTags = cfg.display_menu_show_tags;
    if (cfg && (cfg.display_menu_refresh_rate_sort === 'asc' || cfg.display_menu_refresh_rate_sort === 'desc')) displayMenuPrefs.refreshRateSort = cfg.display_menu_refresh_rate_sort;
    if (cfg && cfg.hotkeys && typeof cfg.hotkeys === 'object') {
      for (const [action, combo] of Object.entries(cfg.hotkeys)) {
        if (typeof combo === 'string') hotkeys[action] = combo;
      }
    }
    if (cfg && typeof cfg.dwm_eotf_profile === 'string') dwmEotf.profile = cfg.dwm_eotf_profile;
    if (cfg && typeof cfg.dwm_eotf_auto_reapply === 'boolean') dwmEotf.autoReapply = cfg.dwm_eotf_auto_reapply;
    try {
      const status = await invoke('get_taskbar_dimmer_status');
      if (status) taskbarDimmer.available = status.available !== false;
    } catch { taskbarDimmer.available = false; }
    if (!taskbarDimmer.available) {
      // Transient false reading (tray restart, shell settling) ,  re-check with
      // backoff instead of latching "unavailable" for the rest of the session.
      clearTimeout(dimmerAvailabilityRetryTimer);
      dimmerAvailabilityRetryTimer = null;
      dimmerAvailabilityRetryEpoch++;
      scheduleDimmerAvailabilityRecheck(5, dimmerAvailabilityRetryEpoch);
    }
    try {
      await invoke('set_taskbar_dimmer', { enabled: taskbarDimmer.enabled, opacity: taskbarDimmer.opacity, fadeInDurationMs: taskbarDimmer.fadeInDurationMs, fadeOutDurationMs: taskbarDimmer.fadeOutDurationMs, immediate: true });
    } catch (e) {
      // Transient failure (the shell can still be settling Shell_TrayWnd right
      // after a tray restart). Do NOT overwrite the loaded toggle state with
      // false ,  that silently erased the persisted ON state. The availability
      // recheck re-applies the settings once the taskbar is reachable again.
      console.warn('[taskbar-dimmer] initial apply failed, keeping loaded state', e);
    }
  } catch (e) {
    console.warn('[config] load failed', e);
  } finally {
    configLoaded.v = true;
    applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
  }
}

export function saveConfig() {
  if (!configLoaded.v) return;
  persistCurrentBlackLevelProfile();
  const blackLiftProfiles = Object.fromEntries(Object.entries(crush.blackProfiles).map(([key, value]) => [key, clampAdjustment(value) / 10]));
  invoke('save_config', { config: {
    black_lift: crush.adjustment / 10,
    black_lift_profiles: blackLiftProfiles,
    black_enabled: crush.blackEnabled,
    hide_calibration_interface: crush.hideCalibrationInterface,
    black_strip_gap_enabled: crush.blackStripGapEnabled,
    high_nits_min: crush.highNitsMin,
    high_nits_max: crush.highNitsMax,
    high_nits_step: crush.highNitsStep,
    taskbar_dimmer_enabled: taskbarDimmer.enabled,
    taskbar_dimmer_opacity: taskbarDimmer.opacity,
    taskbar_dimmer_fade_in_duration_ms: taskbarDimmer.fadeInDurationMs,
    taskbar_dimmer_fade_out_duration_ms: taskbarDimmer.fadeOutDurationMs,
    taskbar_dimmer_hover_detection: taskbarDimmer.hoverDetection || 'event',
    taskbar_dimmer_poll_interval_ms: taskbarDimmer.pollIntervalMs,
    tray_enabled: traySettings.enabled,
    display_menu_resolution_sort: displayMenuPrefs.resolutionSort,
    display_menu_resolution_group_by: displayMenuPrefs.resolutionGroupBy,
    display_menu_show_tags: displayMenuPrefs.showTags,
    display_menu_refresh_rate_sort: displayMenuPrefs.refreshRateSort,
    hotkeys: { ...hotkeys },
    dwm_eotf_profile: dwmEotf.profile,
    dwm_eotf_auto_reapply: dwmEotf.autoReapply,
  } }).catch((e) => console.warn('[config] save failed', e));
}
export function setDwmEotfProfile(profile) {
  dwmEotf.profile = profile;
  saveConfig();
}
export function setDwmEotfAutoReapply(enabled) {
  dwmEotf.autoReapply = !!enabled;
  saveConfig();
}
export async function setTrayEnabled(enabled) {
  const previous = traySettings.enabled;
  traySettings.enabled = enabled;
  try { await invoke('set_tray_enabled', { enabled }); saveConfig(); }
  catch (e) { console.warn('[tray] update failed', e); traySettings.enabled = previous; }
}

let pendingTaskbarDimmerUpdate = null;
let taskbarDimmerUpdateInFlight = false;
let taskbarDimmerUpdateVersion = 0;
// A persist=true update that gets superseded by a newer (possibly
// persist=false) update would lose its save under the plain version check , 
// e.g. toggling the dimmer right as the startup availability recovery
// re-applies settings, which silently dropped the toggle save and left the
// config stuck on the old value. Remember the save intent and flush it once
// the pipeline settles.
let taskbarDimmerSaveQueued = false;
function flushTaskbarDimmerUpdate() {
  if (taskbarDimmerUpdateInFlight) return;
  const update = pendingTaskbarDimmerUpdate;
  if (!update) {
    // Pipeline drained ,  honor any save that a superseded update requested.
    if (taskbarDimmerSaveQueued) {
      taskbarDimmerSaveQueued = false;
      saveConfig();
    }
    return;
  }
  pendingTaskbarDimmerUpdate = null;
  taskbarDimmerUpdateInFlight = true;
  invoke('set_taskbar_dimmer', { enabled: update.enabled, opacity: update.opacity, fadeInDurationMs: update.fadeInDurationMs, fadeOutDurationMs: update.fadeOutDurationMs, immediate: update.immediate })
    .then(() => {
      if (update.version === taskbarDimmerUpdateVersion) {
        // This is the latest update ,  it owns the save decision. Also honor a
        // save that an earlier persist=true update queued.
        if (update.persist || taskbarDimmerSaveQueued) {
          taskbarDimmerSaveQueued = false;
          saveConfig();
        }
        return;
      }
      // Superseded by a newer update. If this one wanted to persist, remember
      // it so the save isn't lost when the newer update has persist=false.
      if (update.persist) taskbarDimmerSaveQueued = true;
    })
    .catch((e) => {
      console.warn('[taskbar-dimmer] update failed', e);
      if (update.version === taskbarDimmerUpdateVersion) taskbarDimmer.enabled = false;
      // A failed persist=true update still represents the user's intent ,  queue
      // the save so the drained pipeline writes the current store state.
      if (update.persist) taskbarDimmerSaveQueued = true;
    })
    .finally(() => { taskbarDimmerUpdateInFlight = false; flushTaskbarDimmerUpdate(); });
}
export function setTaskbarDimmer(enabled = taskbarDimmer.enabled, opacity = taskbarDimmer.opacity, fadeInDurationMs = taskbarDimmer.fadeInDurationMs, fadeOutDurationMs = taskbarDimmer.fadeOutDurationMs, persist = true, immediate = false) {
  taskbarDimmer.enabled = enabled;
  taskbarDimmer.opacity = Math.max(0, Math.min(1, opacity));
  taskbarDimmer.fadeInDurationMs = Math.max(0, Math.min(1000, Math.round(fadeInDurationMs)));
  taskbarDimmer.fadeOutDurationMs = Math.max(0, Math.min(1000, Math.round(fadeOutDurationMs)));
  taskbarDimmerUpdateVersion++;
  pendingTaskbarDimmerUpdate = { enabled: taskbarDimmer.enabled, opacity: taskbarDimmer.opacity, fadeInDurationMs: taskbarDimmer.fadeInDurationMs, fadeOutDurationMs: taskbarDimmer.fadeOutDurationMs, persist, immediate, version: taskbarDimmerUpdateVersion };
  flushTaskbarDimmerUpdate();
}
export function restoreTaskbarDimmerDefaults() { setTaskbarDimmer(false, 0.95, 300, 300, true, true); setHoverDetection('event'); setPollIntervalMs(100); }

// Rust retries overlay creation in the background if it fails at startup
// (e.g. the taskbar/shell hadn't settled yet) and emits this event once that
// retry succeeds. get_taskbar_dimmer_status in loadConfig only runs once, at
// startup, before a background retry can possibly finish ,  without this the
// tab would keep showing "unavailable" for the rest of the session even
// after the overlay comes online. Re-apply the user's current settings too,
// since any set_taskbar_dimmer calls made while the overlay was missing
// would have failed silently.
export function handleTaskbarDimmerAvailable() {
  taskbarDimmer.available = true;
  setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, taskbarDimmer.fadeInDurationMs, taskbarDimmer.fadeOutDurationMs, false, true);
}

// Startup self-heal. The availability snapshot in loadConfig can briefly read
// "unavailable" right after a tray restart even though the overlay was created
// fine (e.g. the shell momentarily hid/recreated Shell_TrayWnd while the new
// process settled). The backend only retries when overlay *creation* fails, so
// without this nothing would ever re-check and the Taskbar Dimmer tab would
// stay disabled for the rest of the session. Re-query with backoff after a
// false reading and latch the overlay once it reports available.
let dimmerAvailabilityRetryTimer = null;
// Epoch guard: cancelling the pending timer in loadConfig (restart of the
// retry window, e.g. restoreAllDefaults) can't stop an invoke that already
// fired ,  without this a stale resolution could schedule a second chain.
let dimmerAvailabilityRetryEpoch = 0;
function scheduleDimmerAvailabilityRecheck(attemptsLeft, epoch) {
  if (attemptsLeft <= 0) return;
  // 1s, 2s, 3s, 4s, 5s after the initial false reading ,  bounded, so a
  // genuinely missing taskbar just gives up quietly.
  const delayMs = 1000 * (6 - attemptsLeft);
  dimmerAvailabilityRetryTimer = setTimeout(() => {
    dimmerAvailabilityRetryTimer = null;
    invoke('get_taskbar_dimmer_status')
      .then((status) => {
        if (epoch !== dimmerAvailabilityRetryEpoch) return;
        if (status && status.available) {
          // Re-apply the user's settings ,  any set_taskbar_dimmer call made
          // while the overlay looked unavailable failed silently. Same
          // recovery path as the backend's taskbar-dimmer-available event.
          handleTaskbarDimmerAvailable();
          console.log('[taskbar-dimmer] overlay availability recovered after startup recheck');
          return;
        }
        scheduleDimmerAvailabilityRecheck(attemptsLeft - 1, epoch);
      })
      .catch(() => {
        if (epoch !== dimmerAvailabilityRetryEpoch) return;
        scheduleDimmerAvailabilityRecheck(attemptsLeft - 1, epoch);
      });
  }, delayMs);
}

export async function restoreAllDefaults() {
  const defaults = await invoke('reset_config');
  await loadConfig(defaults);
  crush.squareCounts = { black: 12, highNits: 8 };
  crush.highNitsMin = HIGH_NITS_DEFAULT_MIN;
  crush.highNitsMax = HIGH_NITS_DEFAULT_MAX;
  crush.highNitsStep = HIGH_NITS_DEFAULT_STEP;
  crush.stripType = 'black';
  crush.fullscreen = 'normal';
  crush.blackStripGapEnabled = true;
  await resetHotkeys();
  invoke('set_tray_enabled', { enabled: true }).catch((e) => console.warn('[restore-all] tray update failed', e));
  invoke('apply_gamma_ramp', { blackLift: 0 }).catch((e) => console.warn('[restore-all] gamma reset failed', e));
}
export function setDimmerSuspendedForFullscreen(suspended) { invoke('set_taskbar_dimmer_suspended', { suspended }).catch(() => {}); }
export function setHoverDetection(mode) {
  const previous = taskbarDimmer.hoverDetection;
  taskbarDimmer.hoverDetection = mode;
  Object.assign(dimmerDetections, detectionDefaultsForMode(mode));
  invoke('set_taskbar_dimmer_hover_mode', { mode }).catch((e) => { console.warn('[hover-detection] toggle failed', e); taskbarDimmer.hoverDetection = previous; Object.assign(dimmerDetections, detectionDefaultsForMode(previous)); });
}
let pollIntervalSaveTimer = null;
let pollIntervalInvokeTimer = null;
export function setPollIntervalMs(ms) {
  const clamped = Math.max(16, Math.min(200, Math.round(ms)));
  taskbarDimmer.pollIntervalMs = clamped;
  // Debounce the IPC call too: the slider fires on every input event during a
  // drag, and the polling cadence only matters once the user settles on one.
  clearTimeout(pollIntervalInvokeTimer);
  pollIntervalInvokeTimer = setTimeout(() => {
    pollIntervalInvokeTimer = null;
    invoke('set_taskbar_dimmer_poll_interval', { ms: clamped }).catch((e) => console.warn('[poll-interval] update failed', e));
  }, 150);
  clearTimeout(pollIntervalSaveTimer);
  pollIntervalSaveTimer = setTimeout(() => saveConfig(), 400);
}
export function setVerbosePeriodicLogging(enabled) {
  runtimeLogSettings.verbosePeriodic = enabled;
  invoke('set_detailed_logging', { enabled }).catch((e) => console.warn('[runtime-log] detailed logging update failed', e));
}
export function setDimmerVerboseLogging(enabled) {
  runtimeLogSettings.dimmerVerbose = enabled;
  invoke('set_taskbar_dimmer_verbose_logging', { enabled }).catch((e) => console.warn('[runtime-log] dimmer verbose logging update failed', e));
}
export function setContextMenuEnabled(enabled) {
  debugSettings.contextMenuEnabled = enabled;
}
export function accentColor() { return app.hdrStatus === 'hdr' ? '#FFCC00' : '#4CA6FF'; }
export function pollHDRStatus() {
  try { app.hdrStatus = window.matchMedia('(dynamic-range: high)').matches ? 'hdr' : 'sdr'; } catch { }
}
export function pollHDRLuminance() {
  invoke('get_hdr_luminance').then((info) => { app.hdrLuminance = info ? { maxNits: info.max_nits ?? null } : null; }).catch(() => { app.hdrLuminance = null; });
}

// Read the Windows SDR white level and publish it to the shared store.
// Chromium/WebView2 renders the HDR calibration canvas relative to this
// value, so the Brightness strip compensates for it (see calibration.js).
// Falls back to 80 nits when the display is in SDR or the query fails.
export function refreshSdrWhiteLevel(asBase = false) {
  invoke('get_sdr_white_level')
    .then((info) => {
      const nits = Number(info?.nits);
      // Match the backend's accepted range (see hdr_toggle.rs: 80..=480).
      if (Number.isFinite(nits) && nits >= 80 && nits <= 480) {
        app.sdrWhiteNits = nits;
        // The first successful read captures the browser's frozen base. Keep it
        // frozen for the whole session ,  the WebView still scales the canvas
        // with that value while the Windows slider moves, so re-capturing on
        // slider changes would drift the strip again. asBase re-captures after
        // an event that DID make the WebView re-read its display info (HDR
        // toggle, display topology change), keeping the base matched to it.
        if (asBase || app.sdrWhiteBaseNits === null) app.sdrWhiteBaseNits = nits;
      }
    })
    .catch(() => {});
}

export function setSdrWhiteNits(nits) {
  const v = Number(nits);
  if (Number.isFinite(v) && v >= 80 && v <= 480) app.sdrWhiteNits = v;
}

// Driver control panels can change dynamic range or VRR without emitting a
// Windows display-topology notification. Refresh only the small vendor-info
// payload; do not re-enumerate modes, EDID, and connector data each time.
let vendorDisplayRefreshInFlight = false;
let vendorDisplayRefreshQueued = false;
let vendorDisplayRefreshSequence = 0;
export function refreshVendorDisplayInfo() {
  if (vendorDisplayRefreshInFlight) {
    // A display-change refresh can arrive while the normal visibility/system
    // refresh is still in flight. Do not drop that request: run one more
    // lightweight query after the current call finishes.
    vendorDisplayRefreshQueued = true;
    return Promise.resolve();
  }
  vendorDisplayRefreshInFlight = true;
  const sequence = ++vendorDisplayRefreshSequence;
  return invoke('refresh_vendor_display_info').then((info) => {
    if (sequence !== vendorDisplayRefreshSequence || !info) return;
    app.displayInfo.dynamicRange = typeof info.dynamic_range === 'string' ? info.dynamic_range : '';
    app.displayInfo.vrr = typeof info.vrr === 'string' ? info.vrr : '';
    app.displayInfo.referenceMode = info.reference_mode === true;
    // Keep the last-known model ,  a transient vendor refresh returning an
    // empty name should never wipe a good one.
    if (typeof info.gpu_model === 'string' && info.gpu_model) app.displayInfo.gpuModel = info.gpu_model;
    // A display-mode transition can briefly return an incomplete vendor
    // payload while the driver is settling. Never erase a known vendor on an
    // empty response; a later non-empty refresh can still update it when the
    // actual adapter changes.
    if (typeof info.vendor === 'string' && info.vendor) app.displayInfo.vendor = info.vendor;
  }).catch(() => {}).finally(() => {
    vendorDisplayRefreshInFlight = false;
    if (vendorDisplayRefreshQueued) {
      vendorDisplayRefreshQueued = false;
      setTimeout(() => refreshVendorDisplayInfo(), 0);
    }
  });
}

let displayPollSequence = 0;
let displayInfoRetryCount = 0;
let displayInfoRefreshTimer = null;

// Coalesce a burst of native display notifications (virtual-monitor attach
// and detach commonly produces several WM_* messages) into one query. This is
// event-driven debouncing, not polling: no refresh occurs unless Rust reports
// a display-related Windows message.
export function scheduleDisplayInfoRefresh() {
  if (displayInfoRefreshTimer !== null) return;
  displayInfoRefreshTimer = setTimeout(() => {
    displayInfoRefreshTimer = null;
    pollDisplayInfo();
  }, 100);
}
function scheduleDisplayInfoRetry() { if (displayInfoRetryCount >= 3) return; displayInfoRetryCount++; setTimeout(() => pollDisplayInfo(), 2000); }
export function pollDisplayInfo() {
  const requestSequence = ++displayPollSequence;
  const s = window.screen;
  if (!s) return;
  const dpr = window.devicePixelRatio || 1;
  const prevWidth = app.displayInfo.width;
  const prevHeight = app.displayInfo.height;
  const prevRefreshRateHz = app.displayInfo.refreshRateHz ?? null;
  const prevRefreshRates = app.displayInfo.refreshRates;
  const prevResolutions = app.displayInfo.resolutions;
  const prevDeviceName = app.displayInfo.deviceName;
  const prevMonitorName = app.displayInfo.monitorName;
  const prevMonitorId = app.displayInfo.monitorId;
  const prevNativeWidth = app.displayInfo.nativeWidth;
  const prevNativeHeight = app.displayInfo.nativeHeight;
  const prevNativeSource = app.displayInfo.nativeSource;
  const prevConnector = app.displayInfo.connector;
  const prevColorFormat = app.displayInfo.colorFormat;
  const prevBitsPerChannel = app.displayInfo.bitsPerChannel;
  const prevDesktopBits = app.displayInfo.desktopBits;
  const prevDynamicRange = app.displayInfo.dynamicRange;
  const prevVrr = app.displayInfo.vrr;
  const prevReferenceMode = app.displayInfo.referenceMode;
  const prevGpuModel = app.displayInfo.gpuModel;
  const prevVendor = app.displayInfo.vendor;
  app.displayInfo = { width: Math.round(s.width * dpr), height: Math.round(s.height * dpr), deviceName: prevDeviceName, monitorName: prevMonitorName, monitorId: prevMonitorId, nativeWidth: prevNativeWidth, nativeHeight: prevNativeHeight, nativeSource: prevNativeSource, gpuModel: prevGpuModel, vendor: prevVendor, connector: prevConnector, colorFormat: prevColorFormat, bitsPerChannel: prevBitsPerChannel, desktopBits: prevDesktopBits, dynamicRange: prevDynamicRange, vrr: prevVrr, referenceMode: prevReferenceMode, refreshRateHz: prevRefreshRateHz, refreshRates: prevRefreshRates, resolutions: prevResolutions };
  invoke('get_display_info').then((info) => {
    if (requestSequence !== displayPollSequence) return;
    if (!info) { scheduleDisplayInfoRetry(); return; }
    if (!info.refresh_hz && (!info.refresh_rates || info.refresh_rates.length === 0)) { scheduleDisplayInfoRetry(); return; }
    displayInfoRetryCount = 0;
    const modeChangeBusy = app.displayModeBusy || app.refreshRateBusy;
    const nextRefreshRate = modeChangeBusy ? app.displayInfo.refreshRateHz : (info.refresh_hz || null);
    const previousRefreshRate = app.displayInfo.refreshRateHz;
    const nextDeviceName = typeof info.device_name === 'string' ? info.device_name : '';
    const nextMonitorId = typeof info.monitor_id === 'string' ? info.monitor_id : '';
    const nextWidth = app.displayModeBusy ? app.displayInfo.width : info.width;
    const nextHeight = app.displayModeBusy ? app.displayInfo.height : info.height;
    const displayOrModeChanged = nextDeviceName !== prevDeviceName
      || nextMonitorId !== prevMonitorId
      || nextWidth !== prevWidth
      || nextHeight !== prevHeight;
    if (nextRefreshRate && (nextRefreshRate !== previousRefreshRate || displayOrModeChanged)) {
      // Save the outgoing monitor/mode before replacing displayInfo, then load
      // the scoped profile for the newly detected display/mode.
      persistCurrentBlackLevelProfile();
    }
    app.displayInfo = { width: nextWidth, height: nextHeight, deviceName: nextDeviceName, monitorName: typeof info.monitor_name === 'string' ? info.monitor_name : '', monitorId: nextMonitorId, nativeWidth: Number(info.native_width) || 0, nativeHeight: Number(info.native_height) || 0, nativeSource: typeof info.native_source === 'string' ? info.native_source : '', gpuModel: typeof info.gpu_model === 'string' && info.gpu_model ? info.gpu_model : prevGpuModel, vendor: prevVendor, connector: typeof info.connector === 'string' ? info.connector : '', colorFormat: typeof info.color_format === 'string' ? info.color_format : '', bitsPerChannel: Number(info.bits_per_channel) || 0, desktopBits: Number(info.desktop_bits) || 0, dynamicRange: typeof info.dynamic_range === 'string' ? info.dynamic_range : '', vrr: typeof info.vrr === 'string' ? info.vrr : '', referenceMode: info.reference_mode === true, refreshRateHz: nextRefreshRate, refreshRates: Array.isArray(info.refresh_rates) ? info.refresh_rates : [], resolutions: Array.isArray(info.resolutions) ? info.resolutions : [] };
    if (nextRefreshRate && (nextRefreshRate !== previousRefreshRate || displayOrModeChanged)) applyBlackLevelProfileForRefreshRate(nextRefreshRate);
  }).catch(() => scheduleDisplayInfoRetry());
}
export const envTick = $state({ n: 0 });
export function bumpEnvTick() { envTick.n++; }

// Cross-component dismiss tick for the right-click context menus. The status
// bar's res/hz context menu handlers stopPropagation() the contextmenu event
// so their own just-opened menu doesn't get closed by its document listener,
// but that also blocks the calibration strip's document-level close listener
// in TabCrush. Bumping this tick lets TabCrush close its strip menu when one
// of the other menus opens.
export const ctxMenuDismissTick = $state({ n: 0 });
export function dismissCtxMenus() { ctxMenuDismissTick.n++; }

// Runtime log is a plain bounded ring buffer. It is populated only by the
// canonical `log://log` listener in main.js; it never writes back to Rust.
const GPU_LOG_MAX = 5000;
let gpuLogStart = 0;
export const gpuLogEntries = [];
export const gpuLogVersion = $state({ n: 0 });
export function pushGpuLog(level, message, ts) {
  // `ts` is only supplied by the startup replay (Rust capture time); live
  // records default to the arrival time.
  const entry = { level, message, ts: ts || Date.now() };
  if (gpuLogEntries.length < GPU_LOG_MAX) gpuLogEntries.push(entry);
  else { gpuLogEntries[gpuLogStart] = entry; gpuLogStart = (gpuLogStart + 1) % GPU_LOG_MAX; }
  if (app.activeTab === 'debug') gpuLogVersion.n++;
}
export function gpuLogSnapshot() {
  if (gpuLogEntries.length < GPU_LOG_MAX || gpuLogStart === 0) return [...gpuLogEntries];
  return gpuLogEntries.slice(gpuLogStart).concat(gpuLogEntries.slice(0, gpuLogStart));
}
export function clearGpuLog() {
  gpuLogEntries.length = 0;
  gpuLogStart = 0;
  if (app.activeTab === 'debug') gpuLogVersion.n++;
}
