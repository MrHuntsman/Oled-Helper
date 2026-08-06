// Svelte 5 runes stores — $state object pattern
import { invoke } from '@tauri-apps/api/core';

export const app = $state({
  activeTab: 'crush',
  hdrStatus: 'unknown',
  hdrLuminance: null,
  displayInfo: { width: 0, height: 0, deviceName: '', monitorName: '', monitorId: '', nativeWidth: 0, nativeHeight: 0, nativeSource: '', refreshRateHz: null, refreshRates: [], resolutions: [] },
  refreshRateBusy: false,
  displayModeBusy: false,
});

export const crush = $state({
  adjustment: 0,
  blackProfiles: {},
  blackEnabled: true,
  squareCount: 12,
  fullscreen: 'normal',
  // Hide the calibration strip interface (labels, controls) — right-click
  // menu on the strip. The info tooltip and fullscreen slider only hide in
  // fullscreen modes; the rest of the interface hides everywhere.
  hideCalibrationInterface: false,
});

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
export const runtimeLogSettings = $state({ verbosePeriodic: false });
export const debugSettings = $state({ contextMenuEnabled: false });

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
    // Registration failed (e.g. the combo is already in use) — revert the UI
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
    invoke('apply_gamma_ramp', { blackLift: blackLift() }).catch(() => {});
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
  invoke('apply_gamma_ramp', { blackLift: blackLift() }).catch(() => {});
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
    try {
      const status = await invoke('get_taskbar_dimmer_status');
      if (status) taskbarDimmer.available = status.available !== false;
    } catch { taskbarDimmer.available = false; }
    if (!taskbarDimmer.available) {
      // Transient false reading (tray restart, shell settling) — re-check with
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
      // false — that silently erased the persisted ON state. The availability
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
  } }).catch((e) => console.warn('[config] save failed', e));
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
// persist=false) update would lose its save under the plain version check —
// e.g. toggling the dimmer right as the startup availability recovery
// re-applies settings, which silently dropped the toggle save and left the
// config stuck on the old value. Remember the save intent and flush it once
// the pipeline settles.
let taskbarDimmerSaveQueued = false;
function flushTaskbarDimmerUpdate() {
  if (taskbarDimmerUpdateInFlight) return;
  const update = pendingTaskbarDimmerUpdate;
  if (!update) {
    // Pipeline drained — honor any save that a superseded update requested.
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
        // This is the latest update — it owns the save decision. Also honor a
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
      // A failed persist=true update still represents the user's intent — queue
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
// startup, before a background retry can possibly finish — without this the
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
// fired — without this a stale resolution could schedule a second chain.
let dimmerAvailabilityRetryEpoch = 0;
function scheduleDimmerAvailabilityRecheck(attemptsLeft, epoch) {
  if (attemptsLeft <= 0) return;
  // 1s, 2s, 3s, 4s, 5s after the initial false reading — bounded, so a
  // genuinely missing taskbar just gives up quietly.
  const delayMs = 1000 * (6 - attemptsLeft);
  dimmerAvailabilityRetryTimer = setTimeout(() => {
    dimmerAvailabilityRetryTimer = null;
    invoke('get_taskbar_dimmer_status')
      .then((status) => {
        if (epoch !== dimmerAvailabilityRetryEpoch) return;
        if (status && status.available) {
          // Re-apply the user's settings — any set_taskbar_dimmer call made
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
  crush.squareCount = 12;
  crush.fullscreen = 'normal';
  await resetHotkeys();
  invoke('set_tray_enabled', { enabled: true }).catch((e) => console.warn('[restore-all] tray update failed', e));
  invoke('apply_gamma_ramp', { blackLift: 0 }).catch(() => {});
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
  invoke('set_taskbar_dimmer_verbose_logging', { enabled }).catch((e) => console.warn('[runtime-log] verbose logging update failed', e));
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
  app.displayInfo = { width: Math.round(s.width * dpr), height: Math.round(s.height * dpr), deviceName: prevDeviceName, monitorName: prevMonitorName, monitorId: prevMonitorId, nativeWidth: prevNativeWidth, nativeHeight: prevNativeHeight, nativeSource: prevNativeSource, refreshRateHz: prevRefreshRateHz, refreshRates: prevRefreshRates, resolutions: prevResolutions };
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
    app.displayInfo = { width: nextWidth, height: nextHeight, deviceName: nextDeviceName, monitorName: typeof info.monitor_name === 'string' ? info.monitor_name : '', monitorId: nextMonitorId, nativeWidth: Number(info.native_width) || 0, nativeHeight: Number(info.native_height) || 0, nativeSource: typeof info.native_source === 'string' ? info.native_source : '', refreshRateHz: nextRefreshRate, refreshRates: Array.isArray(info.refresh_rates) ? info.refresh_rates : [], resolutions: Array.isArray(info.resolutions) ? info.resolutions : [] };
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
