// Svelte 5 runes stores ,  $state object pattern
import { invoke } from '@tauri-apps/api/core';
import { emit, listen } from '@tauri-apps/api/event';
import { HIGH_NITS_DEFAULT_MIN, HIGH_NITS_DEFAULT_STEP, HIGH_NITS_DEFAULT_MAX, HIGH_NITS_CELLS, HIGH_NITS_MAX_CEILING, HIGH_NITS_MAX_STOPS, highNitsStepForMax } from './calibration.js';

export const app = $state({
  activeTab: 'crush',
  // Timestamp (Date.now) of the last time the main window became visible
  // again (tray show or taskbar restore). The Text tab's native preview
  // reads it on mount so it can hold itself hidden until the window's
  // restore animation finishes, instead of appearing over the transition.
  // 0 = never restored (fresh launch), which means no hold.
  windowRestoredAt: 0,
  // Whether that last restore was from MINIMIZED (the Windows restore
  // animation plays) rather than from hidden (instant, e.g. close-to-tray).
  // The Text tab only waits out the animation in the animated case.
  windowRestoreAnimated: false,
  // Timestamp of the last backend-driven show (window-visibility event). The
  // Text tab uses it to tell a backend show (the animated flag decides)
  // from a taskbar restore (no event fires; a taskbar restore is always a
  // minimized restore, so it always animates).
  windowShownAt: 0,
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
  // Whether the fade-in/fade-out sliders are chained (one drag drives both).
  fadeLinked: true,
  available: true,
  hoverDetection: 'event',
  pollIntervalMs: 100,
});

// Text rendering tab settings persisted in config. The Explorer hook toggle
// decides whether the hook DLL fixes Explorer's text (Grayscale/Off
// modes) or leaves it at system ClearType. The three render knobs (all
// 0..100) tune LCD filter strength (0 = off, the default), WOLED crosstalk
// compensation, and faux boldness.
export const textSettings = $state({
  explorerHookEnabled: false,
  lcdFilterStrength: 0,
  crosstalk: 0,
  boldness: 0,
  // Per-monitor rendering (M3): master flag plus per-monitor mode
  // assignments keyed by the monitor's stable signature ("WxH|phys|dpi") →
  // mode id ("rgb", "rwbg", ...). Absent = use the global mode.
  perMonitorEnabled: false,
  perMonitorModes: {},
});

// Screen Dimmer settings persisted in config. The backend needs these at
// startup so the idle watcher and overlay use the saved values immediately.
export const screenDimmer = $state({
  enabled: false,
  seconds: 30,
  opacity: 0.8,
  fadeInDurationMs: 2000,
  fadeOutDurationMs: 500,
  // Whether the fade-in/fade-out sliders are chained (one drag drives both).
  fadeLinked: true,
});

// Debug-tab logging switches, independent of each other:
//   verbosePeriodic ,  vendor/EDID detailed diagnostics (set_detailed_logging)
//   dimmerVerbose   ,  taskbar dimmer tick/winevent/fade verbose logs
//                     (set_taskbar_dimmer_verbose_logging)
export const runtimeLogSettings = $state({ verbosePeriodic: false, dimmerVerbose: false });
export const debugSettings = $state({ contextMenuEnabled: false });

// Global hotkey bindings: action name → combo string ("Ctrl+Alt+F9", or
// "None" for unbound). Loaded from config on startup; changes go through
// setHotkey, which registers the combo in Rust (RegisterHotKey) and persists.
export const hotkeys = $state({});
// Save is deferred briefly after a bind change: the capture flow can fire
// several updates in quick succession (bind, then clear) and each save
// rewrites the whole config file.
let hotkeySaveTimer = null;
function scheduleHotkeySave() {
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
function emitDisplayMenuSortingChanged() {
  emit('display-menu-sorting-changed', {
    resolutionSort: displayMenuPrefs.resolutionSort,
    refreshRateSort: displayMenuPrefs.refreshRateSort,
  }).catch(() => {});
}
export function setResolutionSort(dir) {
  if (dir !== 'asc' && dir !== 'desc') return;
  // One sort direction drives both the resolution and refresh-rate lists;
  // the refresh dropdown reuses the resolution context menu and its sort.
  // refreshRateSort is kept in sync for config/tray backward compatibility.
  displayMenuPrefs.resolutionSort = dir;
  displayMenuPrefs.refreshRateSort = dir;
  saveConfig();
  emitDisplayMenuSortingChanged();
}
export function setResolutionGroupBy(mode) {
  if (mode !== 'none' && mode !== 'aspectRatio') return;
  displayMenuPrefs.resolutionGroupBy = mode;
  saveConfig();
  // Keep the custom tray popup's resolution submenu in sync immediately (it
  // listens on the frontend side), mirroring the tags/sorting events.
  emit('display-menu-groupby-changed', mode).catch(() => {});
}
export function setShowTags(enabled) {
  displayMenuPrefs.showTags = enabled;
  saveConfig();
  // Keep both tray implementations in sync immediately; the custom tray may
  // already be prewarmed, while the native menu is rebuilt from this event.
  emit('display-menu-tags-changed', enabled).catch(() => {});
}
// Kept as a public API for callers that adjust the refresh-rate list sort
// directly; the UI no longer calls it (the refresh dropdown reuses the
// resolution sort), so it simply keeps both fields in sync.
export function setRefreshRateSort(dir) {
  if (dir !== 'asc' && dir !== 'desc') return;
  displayMenuPrefs.resolutionSort = dir;
  displayMenuPrefs.refreshRateSort = dir;
  saveConfig();
  emitDisplayMenuSortingChanged();
}

// User-saved resolution@hz combos for the status-bar favorites menu. Items
// are { width, height, hz }; applying one goes through the same safe
// set_resolution path as the resolution dropdown. Persisted as
// favorite_display_modes in the config file.
export const displayFavorites = $state([]);
export function displayModeFavoriteKey(width, height, hz) {
  return `${Math.round(Number(width) || 0)}x${Math.round(Number(height) || 0)}@${Math.round(Number(hz) || 0)}`;
}
export function isDisplayModeFavorited(width, height, hz) {
  const key = displayModeFavoriteKey(width, height, hz);
  return displayFavorites.some((f) => displayModeFavoriteKey(f.width, f.height, f.hz) === key);
}
let favoritesSaveTimer = null;
function scheduleFavoritesSave() {
  clearTimeout(favoritesSaveTimer);
  favoritesSaveTimer = setTimeout(() => { favoritesSaveTimer = null; saveConfig(); }, 300);
}
function emitFavoritesChanged() {
  emit('favorites-changed', displayFavorites.map((fav) => ({
    width: fav.width,
    height: fav.height,
    hz: fav.hz,
  }))).catch(() => {});
}
export function addFavoriteDisplayMode(width, height, hz) {
  const w = Math.round(Number(width) || 0);
  const h = Math.round(Number(height) || 0);
  const r = Math.round(Number(hz) || 0);
  if (w <= 0 || h <= 0 || r <= 0) return;
  if (isDisplayModeFavorited(w, h, r)) return;
  displayFavorites.push({ width: w, height: h, hz: r });
  emitFavoritesChanged();
  scheduleFavoritesSave();
}
export function removeFavoriteDisplayMode(width, height, hz) {
  const key = displayModeFavoriteKey(width, height, hz);
  const index = displayFavorites.findIndex((f) => displayModeFavoriteKey(f.width, f.height, f.hz) === key);
  if (index === -1) return;
  displayFavorites.splice(index, 1);
  emitFavoritesChanged();
  scheduleFavoritesSave();
}
// Replaces the favorites list from the backend (tray menu toggles emit
// favorites-changed). The main window only reads config at load time, so a
// tray-side add/remove would otherwise stay stale in the resolution dropdown
// and favorites menu until restart. Mutating in place keeps the same array
// identity, so reactive consumers re-render without a state swap.
export function syncFavoritesFromBackend(list) {
  displayFavorites.length = 0;
  if (Array.isArray(list)) {
    for (const fav of list) {
      if (fav && Number.isFinite(fav.width) && Number.isFinite(fav.height) && Number.isFinite(fav.hz)
        && fav.width > 0 && fav.height > 0 && fav.hz > 0) {
        displayFavorites.push({ width: Math.round(fav.width), height: Math.round(fav.height), hz: Math.round(fav.hz) });
      }
    }
  }
}

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
  // `null` (rather than '') so callers can distinguish "no profile" from a
  // real formatted value without relying on string-vs-number falsiness.
  if (!value) return null;
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
    if (cfg && typeof cfg.explorer_hook_enabled === 'boolean') textSettings.explorerHookEnabled = cfg.explorer_hook_enabled;
    if (cfg && typeof cfg.lcd_filter_strength === 'number') textSettings.lcdFilterStrength = Math.max(0, Math.min(100, Math.round(cfg.lcd_filter_strength)));
    if (cfg && typeof cfg.crosstalk === 'number') textSettings.crosstalk = Math.max(0, Math.min(100, Math.round(cfg.crosstalk)));
    if (cfg && typeof cfg.boldness === 'number') textSettings.boldness = Math.max(0, Math.min(100, Math.round(cfg.boldness)));
    if (cfg && typeof cfg.per_monitor_enabled === 'boolean') textSettings.perMonitorEnabled = cfg.per_monitor_enabled;
    if (cfg && typeof cfg.per_monitor_modes === 'object' && cfg.per_monitor_modes) textSettings.perMonitorModes = { ...cfg.per_monitor_modes };
    if (cfg && typeof cfg.screen_protector_enabled === 'boolean') {
      screenDimmer.enabled = cfg.screen_protector_enabled;
    }
    if (cfg && typeof cfg.screen_protector_seconds === 'number') {
      screenDimmer.seconds = Math.max(5, Math.min(180, Math.round(cfg.screen_protector_seconds)));
    } else if (cfg && typeof cfg.screen_protector_minutes === 'number') {
      // Migrate configs written before this control changed from minutes to
      // seconds. The new control intentionally caps the migrated value at
      // three minutes.
      screenDimmer.seconds = Math.max(5, Math.min(180, Math.round(cfg.screen_protector_minutes * 60)));
    }
    if (cfg && typeof cfg.screen_protector_opacity === 'number') {
      screenDimmer.opacity = Math.max(0, Math.min(1, cfg.screen_protector_opacity));
    }
    if (cfg && typeof cfg.screen_protector_fade_in_duration_ms === 'number') {
      screenDimmer.fadeInDurationMs = Math.max(0, Math.min(5000, Math.round(cfg.screen_protector_fade_in_duration_ms)));
    }
    if (cfg && typeof cfg.screen_protector_fade_out_duration_ms === 'number') {
      screenDimmer.fadeOutDurationMs = Math.max(0, Math.min(5000, Math.round(cfg.screen_protector_fade_out_duration_ms)));
    }
    if (cfg && typeof cfg.screen_protector_fade_linked === 'boolean') {
      screenDimmer.fadeLinked = cfg.screen_protector_fade_linked;
    }
    if (cfg && typeof cfg.taskbar_dimmer_opacity === 'number') taskbarDimmer.opacity = Math.max(0, Math.min(1, cfg.taskbar_dimmer_opacity));
    if (cfg && typeof cfg.taskbar_dimmer_fade_in_duration_ms === 'number') taskbarDimmer.fadeInDurationMs = Math.max(0, Math.min(1000, Math.round(cfg.taskbar_dimmer_fade_in_duration_ms)));
    if (cfg && typeof cfg.taskbar_dimmer_fade_out_duration_ms === 'number') taskbarDimmer.fadeOutDurationMs = Math.max(0, Math.min(1000, Math.round(cfg.taskbar_dimmer_fade_out_duration_ms)));
    if (cfg && typeof cfg.taskbar_dimmer_fade_linked === 'boolean') taskbarDimmer.fadeLinked = cfg.taskbar_dimmer_fade_linked;
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
    displayFavorites.length = 0;
    if (cfg && Array.isArray(cfg.favorite_display_modes)) {
      for (const fav of cfg.favorite_display_modes) {
        if (fav && Number.isFinite(fav.width) && Number.isFinite(fav.height) && Number.isFinite(fav.hz)
          && fav.width > 0 && fav.height > 0 && fav.hz > 0) {
          displayFavorites.push({ width: Math.round(fav.width), height: Math.round(fav.height), hz: Math.round(fav.hz) });
        }
      }
    }
  } catch (e) {
    console.warn('[config] load failed', e);
  } finally {
    configLoaded.v = true;
    applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
  }
  // Deferred and non-blocking: the dimmer availability snapshot and initial
  // apply can be slow while the shell settles (right after a tray restart),
  // so they must not delay the first paint. See initTaskbarDimmerRuntime.
  initTaskbarDimmerRuntime();
}

// Taskbar dimmer availability snapshot + initial apply. Kept out of the
// awaited loadConfig path on purpose: get_taskbar_dimmer_status and the first
// set_taskbar_dimmer call can block briefly while Shell_TrayWnd is being
// recreated (tray restart, resolution change), and delaying the main window's
// first paint for that is a worse failure than a momentarily-stale dimmer tab.
export async function initTaskbarDimmerRuntime() {
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
    explorer_hook_enabled: textSettings.explorerHookEnabled,
    lcd_filter_strength: textSettings.lcdFilterStrength,
    crosstalk: textSettings.crosstalk,
    boldness: textSettings.boldness,
    per_monitor_enabled: textSettings.perMonitorEnabled,
    per_monitor_modes: { ...textSettings.perMonitorModes },
    screen_protector_enabled: screenDimmer.enabled,
    screen_protector_seconds: screenDimmer.seconds,
    screen_protector_opacity: screenDimmer.opacity,
    screen_protector_fade_in_duration_ms: screenDimmer.fadeInDurationMs,
    screen_protector_fade_out_duration_ms: screenDimmer.fadeOutDurationMs,
    screen_protector_fade_linked: screenDimmer.fadeLinked,
    taskbar_dimmer_opacity: taskbarDimmer.opacity,
    taskbar_dimmer_fade_in_duration_ms: taskbarDimmer.fadeInDurationMs,
    taskbar_dimmer_fade_out_duration_ms: taskbarDimmer.fadeOutDurationMs,
    taskbar_dimmer_fade_linked: taskbarDimmer.fadeLinked,
    taskbar_dimmer_hover_detection: taskbarDimmer.hoverDetection || 'event',
    taskbar_dimmer_poll_interval_ms: taskbarDimmer.pollIntervalMs,
    display_menu_resolution_sort: displayMenuPrefs.resolutionSort,
    display_menu_resolution_group_by: displayMenuPrefs.resolutionGroupBy,
    display_menu_show_tags: displayMenuPrefs.showTags,
    display_menu_refresh_rate_sort: displayMenuPrefs.refreshRateSort,
    hotkeys: { ...hotkeys },
    favorite_display_modes: displayFavorites.map((f) => ({ width: f.width, height: f.height, hz: f.hz })),
  } }).catch((e) => console.warn('[config] save failed', e));
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
function emitTaskbarDimmerChanged() {
  emit('taskbar-dimmer-changed', {
    enabled: taskbarDimmer.enabled,
    opacity: taskbarDimmer.opacity,
    fadeInDurationMs: taskbarDimmer.fadeInDurationMs,
    fadeOutDurationMs: taskbarDimmer.fadeOutDurationMs,
  }).catch(() => {});
}
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
        emitTaskbarDimmerChanged();
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
      if (update.version === taskbarDimmerUpdateVersion) {
        // Keep the user's requested state rather than forcing 'off'. A
        // transient failure means the overlay wasn't reachable yet; the
        // availability recheck (and the backend's taskbar-dimmer-available
        // event) re-applies this same state once the overlay returns, so
        // wiping `enabled` here would silently drop the persisted ON intent.
        emitTaskbarDimmerChanged();
      }
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

// ── App updater (portable self-updater) ─────────────────────────────
// updateState.status is one of:
//   'idle'       never checked yet (or check not started)
//   'checking'   a check is in flight
//   'available'  a newer version exists (updateInfo filled)
//   'uptodate'   checked, no newer version
//   'error'      the check failed (offline, no endpoint, bad manifest)
// Check failures don't disturb the rest of the app (no modal, no status-bar
// indicator); they only set status 'error', which the About tab surfaces as
// an inline message with a retry. The update itself is handled by the
// backend (updater.rs): it streams the new exe next to the running one,
// verifies the manifest sha256, then hands off to the staged exe which
// swaps the files and relaunches the app.
export const updateState = $state({
  status: 'idle',
  // Populated when status === 'available'.
  version: '',
  notes: '',
  date: '',
  // Download details passed back to the backend on install.
  url: '',
  sha256: '',
  // Install progress 0..1, or null while not downloading.
  progress: null,
  installing: false,
  error: '',
  // Whether the update modal is open. Shared by the status-bar version
  // chip (App.svelte) and the About tab so both can open the same dialog.
  modalOpen: false,
});

let updateInFlight = false;
let lastCheckedAt = 0;
const UPDATE_COOLDOWN_MS = 30 * 60 * 1000; // re-check at most every 30 min

// Checks for a newer version against the GitHub release manifest. Silent on
// failure; only surfaces the 'available' state to the UI. Cooldown prevents
// repeated checks on every tray show / window focus. The cooldown is only
// stamped on success, so a transient offline check (which fails) does not
// lock out the auto re-check for the whole cooldown window.
export async function checkForUpdates(force = false) {
  if (updateInFlight) return;
  if (!force && Date.now() - lastCheckedAt < UPDATE_COOLDOWN_MS) return;
  updateInFlight = true;
  if (updateState.status !== 'available') updateState.status = 'checking';
  updateState.error = '';
  try {
    const update = await invoke('check_for_update');
    if (update) {
      updateState.status = 'available';
      updateState.version = update.version || '';
      updateState.notes = update.notes || '';
      updateState.date = update.date || '';
      updateState.url = update.url || '';
      updateState.sha256 = update.sha256 || '';
    } else {
      // Clear any fields from a previous 'available' state so a later
      // up-to-date check never leaves stale update details behind.
      updateState.status = 'uptodate';
      updateState.version = '';
      updateState.notes = '';
      updateState.date = '';
      updateState.url = '';
      updateState.sha256 = '';
    }
    lastCheckedAt = Date.now();
  } catch (e) {
    // Surface the failure in the About tab's update row so the user knows the
    // check could not complete (offline, blocked endpoint, bad manifest). The
    // status bar stays silent; it only reacts to 'available'.
    updateState.status = 'error';
    updateState.error = String(e);
  } finally {
    updateInFlight = false;
  }
}

// Downloads and installs the available update, reporting progress 0..1 into
// updateState.progress. On success the backend hands off to the staged exe
// and shuts the app down, so this resolves right as the app exits.
export async function installUpdate() {
  if (updateState.status !== 'available' || updateState.installing) return false;
  updateState.installing = true;
  updateState.error = '';
  updateState.progress = 0;
  let unlisten = null;
  try {
    unlisten = await listen('update-progress', (event) => {
      const { downloaded = 0, total = 0 } = event.payload || {};
      if (total > 0) updateState.progress = Math.min(1, downloaded / total);
    });
    await invoke('download_update', { url: updateState.url, sha256: updateState.sha256 });
    updateState.progress = 1;
    await invoke('apply_update');
    return true;
  } catch (e) {
    updateState.error = String(e);
    updateState.status = 'available';
    return false;
  } finally {
    unlisten?.();
    updateState.installing = false;
  }
}
