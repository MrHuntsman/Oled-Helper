// Svelte 5 runes stores, $state object pattern
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
  // Whether this session runs with administrator rights. null until the
  // startup probe answers; the Screen Dimmer tab shows a restart-as-admin
  // prompt when the dimmer is enabled but the session is not elevated.
  isElevated: null,
  hdrStatus: 'unknown',
  hdrLuminance: null,
  // Windows SDR white level in nits (80-480), read live for System Info and
  // the Windows HDR tab.
  sdrWhiteNits: 80,
  // The SDR white level the browser's HDR compositor captured at WebView
  // startup and froze for the session. The Brightness strip must encode
  // against THIS frozen value, Chromium/WebView2 scales the HDR canvas
  // relative to it and only re-reads it on restart, so compensating against
  // the live slider makes the strip drift whenever the SDR brightness moves.
  // Captured on the first successful read (null → fallback 80).
  sdrWhiteBaseNits: null,
  displayInfo: { width: 0, height: 0, deviceName: '', monitorName: '', monitorId: '', profileId: '', nativeWidth: 0, nativeHeight: 0, nativeSource: '', gpuModel: '', vendor: '', connector: '', colorFormat: '', bitsPerChannel: 0, desktopBits: 0, dynamicRange: '', vrr: '', referenceMode: false, refreshRateHz: null, refreshRates: [], resolutions: [] },
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
  // Which calibration strip is active ('black' | 'grey' | 'highNits').
  // Session-only (never persisted), every launch starts on Black.
  stripType: 'black',
  // Per-strip square counts (zoom), session-only like squareCount was.
  // Only the Black strip zooms; the High Nits grid is always fixed.
  squareCounts: { black: 12, highNits: 8 },
  // Selected grey uniformity level (percent of SDR white), session-only
  // like stripType. The Grey strip's ladder picks the fixed 50-95% levels;
  // the fullscreen sweep slider can go anywhere in 0-100%.
  greyLevel: 50,
  // High Nits strip range. The grid is always a fixed 3 rows × 7 columns
  // (21 cells) with no zoom; Min is fixed and the user picks Max via a
  // single slider. Step (nits between neighboring cells) derives silently
  // from Max on a round-number ladder (50/100/250/500/1000/...) below.
  // Persisted in the config file.
  highNitsMin: HIGH_NITS_DEFAULT_MIN,
  highNitsMax: HIGH_NITS_DEFAULT_MAX,
  highNitsStep: HIGH_NITS_DEFAULT_STEP,
  fullscreen: 'normal',
  // Hide the calibration strip interface (labels, controls), right-click
  // menu on the strip. The info tooltip and fullscreen slider only hide in
  // fullscreen modes; the rest of the interface hides everywhere.
  hideCalibrationInterface: false,
  // Small gap between cells on the Black strip (right-click menu toggle).
  // On by default, matching the strip's existing look; the High Nits grid
  // is always edge-to-edge regardless of this setting.
  blackStripGapEnabled: true,
});

// ── Per-monitor selector (M3) ────────────────────────────────────────────
// The monitor chosen in the top bar, every connected monitor is listed
// (defaults to the primary), and each tab reads/writes the selected monitor's
// profiles. Null only before the monitor list has loaded.
// Svelte 5: exported $state bindings cannot be reassigned, so the selection
// lives as a property of a never-reassigned object (the codebase pattern).
export const monitors = $state([]);
export const activeMonitor = $state({ mon: null });

export async function loadMonitors({ retry = true } = {}) {
  try {
    const list = await invoke('list_monitors');
    // Sunshine and other virtual-display drivers can briefly report no
    // attached monitors while the topology is being rebuilt. Treat that as
    // an incomplete snapshot: keep the last valid list and retry instead of
    // making the selector permanently empty.
    if (!Array.isArray(list) || list.length === 0) {
      if (monitors.length === 0) {
        console.warn('[monitors] enumeration returned no monitors');
      } else {
        console.warn('[monitors] transient empty enumeration; retaining current monitor list');
      }
      if (retry) scheduleMonitorRefreshRetry();
      return false;
    }
    monitorRefreshRetryCount = 0;
    clearTimeout(monitorRefreshRetryTimer);
    monitorRefreshRetryTimer = null;
    monitors.splice(0, monitors.length, ...list);
    // A selected monitor that disappeared (unplug/reconnect) falls back to
    // the primary rather than pointing at a stale signature.
    if (activeMonitor.mon && !monitors.some((m) => m.sig === activeMonitor.mon.sig)) {
      activeMonitor.mon = monitors.find((m) => m.primary) || monitors[0] || null;
    }
    // Legacy favorites predate monitor_id (they only carry the sig, whose
    // resolution component changes on every mode switch). Attach the EDID id
    // now that the monitor list is live so their identity becomes stable.
    backfillFavoriteMonitorIds();
    // Same re-keying for the Text tab's per-monitor profiles: entries saved
    // under a sig stop matching after a resolution change, so move them to
    // the EDID id while the monitor list is live.
    backfillTextMonitorKeys();
    // Migrate legacy flat Taskbar Dimmer fields into the primary monitor's
    // profile, and re-key sig-keyed profiles to the EDID id.
    backfillTaskbarDimmerProfiles();
    // Point the dimmer tab at the selected monitor's profile.
    syncTaskbarDimmerView();
    // Same migration + view sync for the Screen Dimmer's per-monitor
    // profiles (legacy flat fields seed the primary, sig entries re-keyed).
    backfillScreenDimmerProfiles();
    syncScreenDimmerView();
    return true;
  } catch (e) {
    console.warn('[monitors] list failed', e);
    if (retry) scheduleMonitorRefreshRetry();
    return false;
  }
}

function scheduleMonitorRefreshRetry() {
  if (monitorRefreshRetryTimer !== null || monitorRefreshRetryCount >= 8) return;
  const attempt = monitorRefreshRetryCount++;
  const delay = Math.min(8000, 500 * (2 ** attempt));
  monitorRefreshRetryTimer = setTimeout(() => {
    monitorRefreshRetryTimer = null;
    loadMonitors().catch(() => {});
  }, delay);
}

export function setActiveMonitor(mon) {
  activeMonitor.mon = mon || null;
  // Black Crush profiles use the same selected-monitor model. Re-apply the
  // current mode's profile immediately so the gamma backend's per-monitor
  // table follows the selector, even when the Crush tab is not mounted.
  if (configLoaded.v && app.displayInfo.refreshRateHz) {
    applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
  }
  saveConfig();
  // Keep the hook's per-monitor table in sync from any tab.
  syncTextPerMonitor();
  // Reload the Taskbar Dimmer tab's view to the newly selected monitor's
  // profile.
  syncTaskbarDimmerView();
  // Same for the Screen Dimmer: the sliders edit the selected monitor's
  // profile, so switching monitors must reload the view.
  syncScreenDimmerView();
}

// Push the per-monitor text flag + all assignments to the hook so its
// shared-memory table matches the profiles. The flag is derived from the
// profiles themselves: per-monitor resolution only runs once any monitor has
// an explicit mode assignment (before that, every monitor uses the system
// default and the hook stays on the cheap lazy path). Assignments are keyed
// by the monitor's stable signature.
export function syncTextPerMonitor() {
  // Per-monitor resolution turns on only once ANY monitor has an explicit
  // mode, knob, or custom-offset assignment; before that, every monitor
  // uses the system defaults and the hook stays on the cheap lazy path.
  const enabled = Object.keys(textSettings.perMonitorModes).length > 0
    || Object.keys(textSettings.perMonitorKnobs).length > 0
    || Object.keys(textSettings.perMonitorCustom).length > 0;
  invoke('explorer_hook_set_per_monitor', { enabled }).catch(() => {});
  for (const [sig, mode] of Object.entries(textSettings.perMonitorModes)) {
    invoke('explorer_hook_set_monitor_mode', { sig, mode }).catch(() => {});
  }
  for (const [sig, k] of Object.entries(textSettings.perMonitorKnobs)) {
    invoke('explorer_hook_set_monitor_knobs', {
      sig,
      boldness: k.boldness,
      damping: k.damping,
    }).catch(() => {});
  }
  for (const [sig, offs] of Object.entries(textSettings.perMonitorCustom)) {
    invoke('explorer_hook_set_monitor_custom', {
      sig,
      rX: offs.rx,
      rY: offs.ry,
      gX: offs.gx,
      gY: offs.gy,
      bX: offs.bx,
      bY: offs.by,
    }).catch(() => {});
  }
}

// Push the hook scope (all-apps flag + blocklist) to the backend so its
// shared-memory values match the store. The backend applies scope at startup
// from the config, so this only needs to run on user changes.
export function syncTextScope() {
  invoke('explorer_hook_set_scope', {
    hookAll: textSettings.hookAllApps,
    blocklist: [...textSettings.hookBlocklist],
  }).catch(() => {});
}

export function activeMonitorSig() {
  return activeMonitor.mon ? activeMonitor.mon.sig : '';
}

// Route a black-lift apply to the selected monitor's device; before the
// selector has a monitor (or with none connected), fall back to the primary.
// One decision point for every caller. The GDI device name is passed too so
// the backend can tell two identical panels apart even when they share the
// same sig and EDID id.
export function applyGamma(blackLiftValue) {
  if (activeMonitor.mon) {
    return invoke('apply_gamma_ramp_for', {
      sig: activeMonitor.mon.sig,
      deviceName: activeMonitor.mon.device_name || null,
      blackLift: blackLiftValue,
    });
  }
  return invoke('apply_gamma_ramp', { blackLift: blackLiftValue });
}

export function activeSquareCount() {
  return crush.squareCounts[crush.stripType] ?? 12;
}

export function setStripType(type) {
  if (type === 'black' || type === 'highNits' || type === 'grey') crush.stripType = type;
}

// High Nits range editing. The grid is a fixed 21 cells ending exactly at
// the Max, with region steps (see calibration.js highNitsGridCodes): 1000-
// step above 5000, 500-step from 1000 to 5000, 100-step below (finer when
// the Max is too low for 21 distinct round values). The user controls a
// single Max slider that rests on HIGH_NITS_MAX_STOPS; Step is derived from
// Max via the region rule (highNitsStepForMax), never dragged directly.
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
  // Snap directly to the nearest slider stop, the slider only ever rests on
  // a stop, so every position is a clean round Max (100/500/1000-step).
  crush.highNitsMax = nearestStop(raw);
  crush.highNitsStep = highNitsStepForMax(crush.highNitsMax);
  scheduleNitsRangeSave();
}
// Re-clamps max/step against the stops and fixes Min to the floor, used
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
// write. crush.highNitsMin/Step update immediately above, only the disk
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

// Canonical dimmer defaults, the single source the state initializers and
// the "restore defaults" handlers read from, so changing a default can't
// leave the restore out of sync. The Rust serde defaults in
// src-tauri/src/config.rs (default_screen_protector_*, default_dimmer_*,
// default_hover_detection, default_poll_interval) must match these: they
// only apply to config files missing the fields, but a mismatch would make
// a fresh install disagree with the UI.
export const SCREEN_DIMMER_DEFAULTS = Object.freeze({
  enabled: false,
  seconds: 30,
  opacity: 0.8,
  fadeInDurationMs: 1000,
  fadeOutDurationMs: 200,
  // Whether the fade-in/fade-out sliders are chained (one drag drives both).
  fadeLinked: true,
  // Whether a fullscreen app on a monitor suppresses the dim there.
  fullscreenDetection: true,
});

export const TASKBAR_DIMMER_DEFAULTS = Object.freeze({
  enabled: false,
  opacity: 0.95,
  fadeInDurationMs: 300,
  fadeOutDurationMs: 300,
  // Whether the fade-in/fade-out sliders are chained (one drag drives both).
  fadeLinked: true,
  hoverDetection: 'event',
  pollIntervalMs: 100,
});

export const taskbarDimmer = $state({
  ...TASKBAR_DIMMER_DEFAULTS,
  // Runtime-only: whether the overlay is available (not a persisted default).
  available: true,
});

// Per-monitor Taskbar Dimmer profiles, keyed by the monitor's profile
// identity (see monProfileId), same keying as the Text-tab profiles.
// `taskbarDimmer` above is the *view* of the active monitor's profile: the
// sliders edit the selected monitor's entry through the helpers below, and
// switching monitors reloads the view from this map. A monitor without an
// entry uses TASKBAR_DIMMER_DEFAULTS (mirroring the backend's
// DimmerProfile::default()). The detection fields (hoverDetection /
// pollIntervalMs) are global, not per monitor, so they never live in a
// profile entry.
export const taskbarDimmerProfiles = $state({});

// The profile key for a monitor: the backend-computed profile identity (the
// EDID device id, suffixed with the connector when identical panels share
// the id so each resolves as its own profile), falling back to the id/sig.
// The backend normalizes to the same key (monitor_key_for_taskbar /
// resolve_profile_key / canonical_profile_id).
export function monProfileId(mon) {
  return (mon && (mon.profile_id || mon.monitor_id || mon.sig)) || '';
}
export function dimmerMonKey(mon = activeMonitor.mon) {
  return monProfileId(mon);
}

// Load the active monitor's profile into the view fields. Called when the
// monitor selector changes and after config load resolves the selection.
// The detection fields (hoverDetection / pollIntervalMs) are global, so
// switching monitors leaves them untouched.
export function syncTaskbarDimmerView() {
  const key = dimmerMonKey();
  const p = key ? taskbarDimmerProfiles[key] : null;
  if (p) {
    taskbarDimmer.enabled = p.enabled;
    taskbarDimmer.opacity = p.opacity;
    taskbarDimmer.fadeInDurationMs = p.fadeInDurationMs;
    taskbarDimmer.fadeOutDurationMs = p.fadeOutDurationMs;
  }
}

// The active monitor's profile entry, creating it from the defaults on first
// edit so an untouched monitor gets the canonical defaults written back.
function currentDimmerProfile() {
  const key = dimmerMonKey();
  if (!key) return null;
  let p = taskbarDimmerProfiles[key];
  if (!p) {
    p = {
      enabled: TASKBAR_DIMMER_DEFAULTS.enabled,
      opacity: TASKBAR_DIMMER_DEFAULTS.opacity,
      fadeInDurationMs: TASKBAR_DIMMER_DEFAULTS.fadeInDurationMs,
      fadeOutDurationMs: TASKBAR_DIMMER_DEFAULTS.fadeOutDurationMs,
    };
    taskbarDimmerProfiles[key] = p;
  }
  return p;
}

// Text rendering tab settings persisted in config. The Explorer hook toggle
// decides whether the hook DLL fixes Explorer's text (Grayscale/Off
// modes) or leaves it at system ClearType. The two render knobs (all
// 0..100, boldness capped at 30) tune faux boldness and fringe damping.
// (The LCD filter knob was removed.)
export const textSettings = $state({
  explorerHookEnabled: false,
  // Hook scope: whether the swash detour applies to every GUI process
  // (all apps, the default; GDI apps like 7-Zip force ClearType and ignore
  // the registry setting) or only the shell/Control Panel hosts. The
  // blocklist is the escape hatch: exe names excluded from the detour.
  hookAllApps: true,
  hookBlocklist: [],
  // Per-monitor mode assignments keyed by the monitor's stable signature
  // ("WxH|phys|dpi") → mode id ("rgb", "bgr", "custom", ...). Absent = the
  // system default ("Default" in the UI). Whether per-monitor resolution is
  // active is derived from whether any assignment exists, no stored flag.
  perMonitorModes: {},
  // Per-monitor render knob pairs, sig → { boldness, damping } (all
  // 0..=100). Absent = the defaults (no bold, no damping). Like modes, the
  // presence of any knob entry activates per-monitor resolution.
  perMonitorKnobs: {},
  // Per-monitor custom subpixel centers, sig → { rx, ry, gx, gy, bx, by }
  // (per-channel x/y fractions of a pixel). Used when the monitor's mode is
  // 'custom', the live calibration controls. Absent = the mode's default
  // layout.
  perMonitorCustom: {},
});

// Screen Dimmer settings persisted in config. The backend needs these at
// startup so the idle watcher and overlay use the saved values immediately.
export const screenDimmer = $state({
  ...SCREEN_DIMMER_DEFAULTS,
});

// Per-monitor Screen Dimmer profiles, keyed by the monitor's profile
// identity (see monProfileId), same keying as the Taskbar Dimmer profiles.
// `screenDimmer` above is the *view* of the active monitor's profile: the
// sliders edit the selected monitor's entry through the helpers below, and
// switching monitors reloads the view from this map. A monitor without an
// entry uses SCREEN_DIMMER_DEFAULTS (mirroring the backend's
// ScreenDimmerProfile::default()). The master enabled toggle stays global.
export const screenDimmerProfiles = $state({});

// Load the active monitor's profile into the view fields. Called when the
// monitor selector changes and after config load resolves the selection.
// The enabled toggle is global, so it is never overwritten here.
export function syncScreenDimmerView() {
  const key = dimmerMonKey();
  const p = key ? screenDimmerProfiles[key] : null;
  if (p) {
    screenDimmer.seconds = p.seconds;
    screenDimmer.opacity = p.opacity;
    screenDimmer.fadeInDurationMs = p.fadeInDurationMs;
    screenDimmer.fadeOutDurationMs = p.fadeOutDurationMs;
    screenDimmer.fullscreenDetection = p.fullscreenDetection;
  }
}

// The active monitor's profile entry, creating it from the defaults on first
// edit so an untouched monitor gets the canonical defaults written back.
function currentScreenDimmerProfile() {
  const key = dimmerMonKey();
  if (!key) return null;
  let p = screenDimmerProfiles[key];
  if (!p) {
    p = {
      seconds: SCREEN_DIMMER_DEFAULTS.seconds,
      opacity: SCREEN_DIMMER_DEFAULTS.opacity,
      fadeInDurationMs: SCREEN_DIMMER_DEFAULTS.fadeInDurationMs,
      fadeOutDurationMs: SCREEN_DIMMER_DEFAULTS.fadeOutDurationMs,
      fullscreenDetection: SCREEN_DIMMER_DEFAULTS.fullscreenDetection,
    };
    screenDimmerProfiles[key] = p;
  }
  return p;
}

// App-level settings (Settings tab). No feature needs an elevated app
// session anymore (privileged steps run in the transient helper child), so
// only the flags below are persisted.
export const appSettings = $state({
  skipFavoriteConfirmation: false,
});

// Debug-tab logging switches, independent of each other:
//   verbosePeriodic: vendor/EDID detailed diagnostics (set_detailed_logging)
//   dimmerVerbose: taskbar dimmer tick/winevent/fade verbose logs
//                     (set_taskbar_dimmer_verbose_logging)
export const runtimeLogSettings = $state({ verbosePeriodic: false, dimmerVerbose: false });
export const debugSettings = $state({
  contextMenuEnabled: false,
});

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
    // Registration failed (e.g. the combo is already in use), revert the UI
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
  // Restore Tab Defaults clears every hotkey combo, including the per-
  // favorite resolution hotkeys (the user chose "clear everything"). The
  // saved favorites themselves are kept.
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
// are { width, height, hz, favId, monitorId, monitorSig, monitorName };
// applying one goes through the same safe set_resolution path as the
// resolution dropdown but targets the monitor it was saved on. Persisted as
// favorite_display_modes in the config file. Favorites are scoped to the
// monitor they were saved on: monitorId is the EDID device id (stable across
// resolution changes, unlike the sig whose resolution component changes when
// the mode changes), and monitorSig (resolution+physical size+DPI) is kept as
// a fallback for legacy favorites saved before monitor_id existed, resolved
// to a GDI device name at apply time via the live monitors list.
export const displayFavorites = $state([]);
// Stable per-favorite identity. Random UUIDs for new favorites; legacy
// entries without one get a deterministic id derived from their key so slot
// assignments survive restarts without persisting anything new.
export function favoriteIdFor(width, height, hz, monitorSig = '', existing = '') {
  if (typeof existing === 'string' && existing) return existing;
  return `legacy:${String(monitorSig || '')}|${displayModeFavoriteKey(width, height, hz)}`;
}
function newFavoriteId() {
  try {
    if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
      return crypto.randomUUID();
    }
  } catch { /* fall through to the local fallback */ }
  return `fav-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}
export function displayModeFavoriteKey(width, height, hz, monitorId = '', monitorSig = '') {
  // Prefer the EDID device id: it identifies the physical panel regardless of
  // the current mode, so the same combo saved at 4K still dedupes/star-checks
  // after the user switches to an ultrawide mode on the same monitor. Legacy
  // favorites without one fall back to the sig.
  const identity = String(monitorId || '') || String(monitorSig || '');
  return `${identity}|${Math.round(Number(width) || 0)}x${Math.round(Number(height) || 0)}@${Math.round(Number(hz) || 0)}`;
}
export function isDisplayModeFavorited(width, height, hz, monitorId = '', monitorSig = '') {
  const key = displayModeFavoriteKey(width, height, hz, monitorId, monitorSig);
  return displayFavorites.some((f) => displayModeFavoriteKey(f.width, f.height, f.hz, f.monitorId, f.monitorSig) === key);
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
    fav_id: fav.favId,
    monitor_id: fav.monitorId,
    monitor_sig: fav.monitorSig,
    monitor_device: fav.monitorDevice,
    monitor_name: fav.monitorName,
    confirmed_monitor_id: fav.confirmedMonitorId || '',
  }))).catch(() => {});
}
// Accepts both the internal camelCase shape and the config/tray snake_case
// shape (fav_id, monitor_id, monitor_sig, monitor_device, monitor_name), so
// config loads and favorites-changed events from the tray round-trip without
// losing the monitor-scoping fields.
function normalizeFavorite(fav) {
  const w = Math.round(Number(fav?.width) || 0);
  const h = Math.round(Number(fav?.height) || 0);
  const r = Math.round(Number(fav?.hz) || 0);
  if (w <= 0 || h <= 0 || r <= 0) return null;
  const favId = typeof fav?.favId === 'string' && fav.favId
    ? fav.favId
    : (typeof fav?.fav_id === 'string' ? fav.fav_id : '');
  const monId = typeof fav?.monitorId === 'string'
    ? fav.monitorId
    : (typeof fav?.monitor_id === 'string' ? fav.monitor_id : '');
  const sig = typeof fav?.monitorSig === 'string'
    ? fav.monitorSig
    : (typeof fav?.monitor_sig === 'string' ? fav.monitor_sig : '');
  const device = typeof fav?.monitorDevice === 'string'
    ? fav.monitorDevice
    : (typeof fav?.monitor_device === 'string' ? fav.monitor_device : '');
  const name = typeof fav?.monitorName === 'string'
    ? fav.monitorName
    : (typeof fav?.monitor_name === 'string' ? fav.monitor_name : '');
  return {
    width: w,
    height: h,
    hz: r,
    favId: favoriteIdFor(w, h, r, sig, favId),
    monitorId: monId,
    monitorSig: sig,
    monitorDevice: device,
    monitorName: name,
    confirmedMonitorId: typeof fav?.confirmedMonitorId === 'string'
      ? fav.confirmedMonitorId
      : (typeof fav?.confirmed_monitor_id === 'string' ? fav.confirmed_monitor_id : ''),
  };
}
export function markFavoriteConfirmed(fav, monitorId) {
  if (!fav || !monitorId) return;
  fav.confirmedMonitorId = monitorId;
  scheduleFavoritesSave();
}

export function addFavoriteDisplayMode(width, height, hz, monitorSig = '', monitorName = '', monitorId = '', monitorDevice = '') {
  const w = Math.round(Number(width) || 0);
  const h = Math.round(Number(height) || 0);
  const r = Math.round(Number(hz) || 0);
  const sig = typeof monitorSig === 'string' ? monitorSig : '';
  const monId = typeof monitorId === 'string' ? monitorId : '';
  const device = typeof monitorDevice === 'string' ? monitorDevice : '';
  if (w <= 0 || h <= 0 || r <= 0) return;
  if (isDisplayModeFavorited(w, h, r, monId, sig)) return;
  displayFavorites.push({
    width: w,
    height: h,
    hz: r,
    favId: newFavoriteId(),
    monitorId: monId,
    monitorSig: sig,
    monitorDevice: device,
    monitorName: typeof monitorName === 'string' ? monitorName : '',
    confirmedMonitorId: '',
  });
  emitFavoritesChanged();
  scheduleFavoritesSave();
}
export function removeFavoriteDisplayMode(width, height, hz, monitorId = '', monitorSig = '') {
  const key = displayModeFavoriteKey(width, height, hz, monitorId, monitorSig);
  const index = displayFavorites.findIndex((f) => displayModeFavoriteKey(f.width, f.height, f.hz, f.monitorId, f.monitorSig) === key);
  if (index === -1) return;
  displayFavorites.splice(index, 1);
  reconcileFavoriteHotkeys();
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
      const normalized = normalizeFavorite(fav);
      if (normalized) displayFavorites.push(normalized);
    }
  }
  reconcileFavoriteHotkeys();
  backfillFavoriteMonitorIds();
}

// Favorites saved before monitor_id existed only carry the sig, and the sig's
// resolution component changes whenever the mode changes. Once the live
// monitor list is available, match those legacy favorites to their panel by
// sig (or its physical-size+DPI portion) and attach the EDID device id so
// their identity becomes resolution-independent from here on. No-op for
// favorites that already have an id or whose monitor is currently absent
// (those stay resolvable via the sig fallback at apply time).
function stableSigPartForBackfill(sig) {
  const parts = String(sig || '').split('|');
  return parts.length >= 3 ? `${parts[1]}|${parts[2]}` : String(sig || '');
}
// The model identity shared by identical panels: the MONITOR\<EDID base> part
// of the device id (r"MONITOR\LGD05XX\5&...&UID1" -> r"MONITOR\LGD05XX").
function monitorIdBase(id) {
  const parts = String(id || '').split('\\');
  return parts.length >= 2 ? `${parts[0]}\\${parts[1]}` : String(id || '');
}
// EDID ids shared by more than one connected monitor: an @-suffixed old key
// embedding a SHARED id is ambiguous between the siblings, so it only
// migrates when the panel is no longer duplicated.
function sharedMonitorIds() {
  const counts = new Map();
  for (const mon of monitors) counts.set(mon.monitor_id, (counts.get(mon.monitor_id) || 0) + 1);
  const shared = new Set();
  for (const [id, count] of counts) if (count > 1 && id) shared.add(id);
  return shared;
}
// Whether a stored per-monitor profile key belongs to this monitor: its
// current profile identity, its bare EDID id, a serial-keyed identity
// ("<base>@serial=<serial>", matched precisely on the serial so a sibling's
// entry can't be stolen), a legacy sig (or its stable physical-size+DPI
// portion), or a stale disambiguated id ("<id>@connector" / "<id>@dev=...")
// from before a duplicate-count change. The stale composite only matches
// while this monitor is NOT itself disambiguated or its id is not shared, so
// a still-duplicated sibling cannot steal its entry.
function monitorMatchesKey(m, key) {
  if (m.profile_id && m.profile_id === key) return true;
  if (m.monitor_id && m.monitor_id === key) return true;
  if (m.monitor_id) {
    const serialPrefix = monitorIdBase(m.monitor_id) + '@serial=';
    if (m.edid_serial && key.startsWith(serialPrefix) && key.slice(serialPrefix.length) === m.edid_serial) return true;
    if (key.startsWith(m.monitor_id + '@')) {
      const shared = sharedMonitorIds().has(m.monitor_id);
      if (m.profile_id === m.monitor_id || !shared) return true;
    }
  }
  if (m.sig === key) return true;
  return stableSigPartForBackfill(m.sig) === stableSigPartForBackfill(key);
}
function backfillFavoriteMonitorIds() {
  let changed = false;
  for (const fav of displayFavorites) {
    if (fav.monitorId || !fav.monitorSig) continue;
    const matches = monitors.filter((m) =>
      m.sig === fav.monitorSig
      || stableSigPartForBackfill(m.sig) === stableSigPartForBackfill(fav.monitorSig));
    const mon = matches[0];
    if (mon && mon.monitor_id) {
      fav.monitorId = mon.monitor_id;
      // Attach the GDI device name only when the panel match is unambiguous
      // (exactly one monitor): with two identical panels the sig matches both,
      // and attaching the wrong sibling's device would mis-route applies.
      if (matches.length === 1 && mon.device_name) fav.monitorDevice = mon.device_name;
      changed = true;
    }
  }
  if (changed) {
    emitFavoritesChanged();
    scheduleFavoritesSave();
  }
}

// Re-key the Text tab's per-monitor profiles (modes, knobs, custom offsets)
// from legacy sig keys to the EDID device id. The sig embeds the current
// resolution, so an entry saved under it stops matching after a mode switch
// (e.g. a hotkey favorite); the EDID id never changes with resolution. Runs
// whenever the monitor list or config lands (idempotent: skips entries
// whose key already matches a current monitor id or that have no resolvable
// monitor).
function backfillTextMonitorKeys() {
  if (!monitors.length) return;
  let changed = false;
  for (const map of [textSettings.perMonitorModes, textSettings.perMonitorKnobs, textSettings.perMonitorCustom]) {
    for (const [key, value] of Object.entries(map)) {
      const mon = monitors.find((m) => monitorMatchesKey(m, key));
      if (!mon || !mon.monitor_id || monProfileId(mon) === key) continue;
      delete map[key];
      map[monProfileId(mon)] = value;
      changed = true;
    }
  }
  if (changed) saveConfig();
}

// Re-key the Taskbar Dimmer's per-monitor profiles and migrate the legacy
// flat taskbar_dimmer_* fields into the primary monitor's profile. Runs
// whenever the monitor list or config lands (idempotent): the flat fields
// seed the primary profile only while the profiles map is empty, and
// sig-keyed entries are moved to the EDID id exactly like the Text-tab
// backfill.
function backfillTaskbarDimmerProfiles() {
  if (!monitors.length) return;
  // Re-key entries to the profile identity (EDID id, disambiguated for
  // identical panels), so each monitor keeps its own profile.
  let changed = false;
  for (const [key, value] of Object.entries(taskbarDimmerProfiles)) {
    const mon = monitors.find((m) => monitorMatchesKey(m, key));
    if (!mon || !mon.monitor_id || monProfileId(mon) === key) continue;
    delete taskbarDimmerProfiles[key];
    taskbarDimmerProfiles[monProfileId(mon)] = value;
    changed = true;
  }
  // Legacy flat fields (written by configs predating per-monitor profiles):
  // seed the primary monitor's profile from them so the tray menu and the
  // primary taskbar keep the user's old values.
  const primary = monitors.find((m) => m.primary) || monitors[0];
  if (primary && !Object.keys(taskbarDimmerProfiles).length) {
    const key = monProfileId(primary);
    if (key) {
      taskbarDimmerProfiles[key] = {
        enabled: taskbarDimmer.enabled,
        opacity: taskbarDimmer.opacity,
        fadeInDurationMs: taskbarDimmer.fadeInDurationMs,
        fadeOutDurationMs: taskbarDimmer.fadeOutDurationMs,
      };
      changed = true;
    }
  }
  if (changed) saveConfig();
}

// Re-key the Screen Dimmer's per-monitor profiles and migrate the legacy
// flat screen_protector_* fields into the primary monitor's profile. Runs
// whenever the monitor list or config lands (idempotent): the flat fields
// seed the primary profile only while the profiles map is empty, and
// sig-keyed entries are moved to the EDID id exactly like the Taskbar
// Dimmer backfill.
function backfillScreenDimmerProfiles() {
  if (!monitors.length) return;
  // Re-key entries to the profile identity (EDID id, disambiguated for
  // identical panels), so each monitor keeps its own profile.
  let changed = false;
  for (const [key, value] of Object.entries(screenDimmerProfiles)) {
    const mon = monitors.find((m) => monitorMatchesKey(m, key));
    if (!mon || !mon.monitor_id || monProfileId(mon) === key) continue;
    delete screenDimmerProfiles[key];
    screenDimmerProfiles[monProfileId(mon)] = value;
    changed = true;
  }
  // Legacy flat fields (written by configs predating per-monitor profiles):
  // seed the primary monitor's profile from them so the primary keeps the
  // user's old values and the no-selector paths stay truthful.
  const primary = monitors.find((m) => m.primary) || monitors[0];
  if (primary && !Object.keys(screenDimmerProfiles).length) {
    const key = monProfileId(primary);
    if (key) {
      screenDimmerProfiles[key] = {
        seconds: screenDimmer.seconds,
        opacity: screenDimmer.opacity,
        fadeInDurationMs: screenDimmer.fadeInDurationMs,
        fadeOutDurationMs: screenDimmer.fadeOutDurationMs,
        fullscreenDetection: screenDimmer.fullscreenDetection,
      };
      changed = true;
    }
  }
  if (changed) saveConfig();
}

// ── Favorite hotkeys (bound directly to each favorite) ────────────────
// Each favorite can carry its own hotkey: the action name embeds the
// favorite's stable id ("favorite_<favId>", registered dynamically in
// hotkeys.rs), so a hotkey stays bound to a specific saved favorite even if
// the favorites list is reordered. There is no slot indirection: one row per
// favorite, each with its own capture pill.
export function favoriteHotkeyAction(favId) {
  return `favorite_${String(favId)}`;
}
export function favoriteForAction(action) {
  if (typeof action !== 'string' || !action.startsWith('favorite_')) return null;
  const favId = action.slice('favorite_'.length);
  if (!favId) return null;
  return displayFavorites.find((f) => f.favId === favId) || null;
}
// Drop hotkey bindings whose favorite no longer exists; a hotkey must never
// stay silently bound to a favId that resolves to nothing. Also unregisters
// the stale combo live so the OS stops firing it. Called from every
// favorites-removal path and after config load.
function reconcileFavoriteHotkeys() {
  const pruned = [];
  for (const action of Object.keys(hotkeys)) {
    if (action.startsWith('favorite_') && !favoriteForAction(action)) {
      delete hotkeys[action];
      pruned.push(action);
    }
  }
  if (pruned.length) {
    scheduleHotkeySave();
    for (const action of pruned) {
      invoke('set_hotkey', { action, combo: 'None' }).catch(() => {});
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
// Display identity the Black Crush profiles are keyed against: the selected
// monitor's EDID id (identical to app.displayInfo's for the primary, so
// existing primary profile keys stay compatible), falling back to the current
// display before the selector has a monitor.
function crushInfo() {
  if (activeMonitor.mon) {
    return {
      monitorId: monProfileId(activeMonitor.mon),
      deviceName: activeMonitor.mon.device_name || '',
      width: activeMonitor.mon.width,
      height: activeMonitor.mon.height,
      hz: Number(activeMonitor.mon.refresh_hz) || undefined,
    };
  }
  return app.displayInfo;
}
// The refresh rate that scopes the current black-lift profile: a SECONDARY
// monitor's own live rate (the monitor list is its only Hz source, and it can
// differ from the primary's), else the primary display's rate. The primary is
// deliberately sourced from app.displayInfo rather than the list: its value
// updates optimistically during a refresh-rate change, while the list only
// refreshes after the display-changed event lands.
function currentProfileHz() {
  const mon = activeMonitor.mon;
  if (mon && !mon.primary) {
    const monHz = Number(mon.refresh_hz);
    if (Number.isFinite(monHz) && monHz > 0) return monHz;
  }
  return app.displayInfo.refreshRateHz || null;
}
function hzKey(hz) {
  const value = Number(hz);
  return Number.isFinite(value) && value > 0 ? String(Math.round(value)) : null;
}
function displayIdentity(info = app.displayInfo) {
  // Prefer the profile identity (EDID id, disambiguated for identical
  // panels) so two same-model monitors key separate black-lift profiles.
  const identity = String(info.profileId || info.monitorId || info.deviceName || '').trim();
  return identity || 'unknown-display';
}
function profileKey(hz, info = crushInfo()) {
  const hzPart = hzKey(hz);
  if (!hzPart || !info.width || !info.height) return null;
  // Monitor identity + active mode makes profiles independent across both
  // physical/virtual displays and resolutions at the same refresh rate.
  return `${displayIdentity(info)}|${info.width}x${info.height}|${hzPart}`;
}
function clampAdjustment(value) { return Math.max(-150, Math.min(150, Math.round(Number(value) || 0))); }
function clampKnob(value) { return Math.max(0, Math.min(100, Math.round(Number(value) || 0))); }
// Boldness tops out at 30 (≈0.3px of stroke thickening): the slider max.
// Old configs with higher values are clamped on load.
export const BOLDNESS_MAX = 30;
function clampBoldness(value) { return Math.min(BOLDNESS_MAX, clampKnob(value)); }
function clampOffset(value) { const n = Number(value); return Number.isFinite(n) ? Math.max(-1, Math.min(1, n)) : 0; }
// Normalize a per-monitor custom offset entry from config. Accepts the 2D
// [[rx, ry], [gx, gy], [bx, by]] shape (current) and the legacy flat [r, g, b]
// shape (upgraded to y = 0). Returns null for malformed entries.
function customOffsetsFromConfig(offs) {
  if (!Array.isArray(offs) || offs.length !== 3) return null;
  const channels = [];
  for (let c = 0; c < 3; c++) {
    const channel = offs[c];
    if (Array.isArray(channel) && channel.length >= 2) {
      channels.push([channel[0], channel[1]]);
    } else if (Array.isArray(channel)) {
      return null; // malformed channel
    } else {
      channels.push([channel, 0]); // legacy flat value → y = 0
    }
  }
  const [r, g, b] = channels;
  return {
    rx: clampOffset(r[0]), ry: clampOffset(r[1]),
    gx: clampOffset(g[0]), gy: clampOffset(g[1]),
    bx: clampOffset(b[0]), by: clampOffset(b[1]),
  };
}
function profileValue(key) {
  return key && Object.prototype.hasOwnProperty.call(crush.blackProfiles, key)
    ? clampAdjustment(crush.blackProfiles[key])
    : null;
}
// The status bar and its refresh-rate dropdown are primary-scoped (they show
// the primary's rates and change the primary's mode), so their profile values
// are looked up under the primary display's identity, NOT the selected
// monitor's. The selected monitor's own profiles are keyed by its own hz via
// currentProfileHz in the persist/apply paths.
export function blackLevelForRefreshRate(hz) {
  const key = profileKey(hz, app.displayInfo);
  const value = profileValue(key);
  if (value !== null) return value;
  const legacyKey = hzKey(hz);
  return legacyKey && Object.prototype.hasOwnProperty.call(legacyBlackProfiles, legacyKey)
    ? clampAdjustment(legacyBlackProfiles[legacyKey])
    : 0;
}
export function blackLevelDisplayForRefreshRate(hz) {
  const key = profileKey(hz, app.displayInfo);
  const currentKey = profileKey(app.displayInfo.refreshRateHz, app.displayInfo);
  // The live slider value only corresponds to the status bar while the
  // primary is the selection; with a secondary selected, show the stored
  // primary profile (or nothing) instead of the secondary's live value.
  const primarySelected = !activeMonitor.mon || activeMonitor.mon.primary;
  const value = key && key === currentKey && primarySelected
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
  const key = profileKey(currentProfileHz());
  if (key) crush.blackProfiles[key] = clampAdjustment(crush.adjustment);
}
export function applyBlackLevelProfileForRefreshRate(hz) {
  if (!configLoaded.v) return;
  // A selected secondary monitor scopes the profile by its OWN refresh rate;
  // the hz parameter is the primary's (from the status-bar and apply paths)
  // and only applies when no monitor is selected or the rate is unknown.
  const effectiveHz = currentProfileHz() ?? hz;
  const key = profileKey(effectiveHz);
  if (!key) return;
  const existing = profileValue(key);
  if (existing !== null) {
      crush.adjustment = existing;
    applyGamma(blackLift()).catch((e) => console.warn('[profile] gamma apply failed', e));
    return;
  }
  const legacyKey = hzKey(effectiveHz);
  const fallback = legacyKey && Object.prototype.hasOwnProperty.call(legacyBlackProfiles, legacyKey)
    ? clampAdjustment(legacyBlackProfiles[legacyKey])
    : (legacyBlackLift !== null ? clampAdjustment(legacyBlackLift) : 0);
  crush.blackProfiles[key] = fallback;
  legacyBlackLift = null;
  crush.adjustment = fallback;
  // Profile application is owned here rather than by TabCrush, so a display
  // change while another tab is open still updates the gamma immediately.
  // applyGamma routes to the selected monitor or the primary display.
  applyGamma(blackLift()).catch((e) => console.warn('[profile] gamma apply failed', e));
}

export function currentBlackLevelProfileKey() {
  return profileKey(currentProfileHz());
}

export const configLoaded = $state({ v: false });
export async function loadConfig(prefetchedCfg = null) {
  // Old slot-based favorite hotkeys (favorite_slot_N) migrated to hotkeys
  // bound directly to the favorite (favorite_<favId>); filled during the
  // config load below, applied live after the config is committed.
  let migrateFavoriteHotkeys = [];
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
    if (cfg && typeof cfg.explorer_hook_all_apps === 'boolean') textSettings.hookAllApps = cfg.explorer_hook_all_apps;
    if (cfg && Array.isArray(cfg.explorer_hook_blocklist)) {
      textSettings.hookBlocklist = cfg.explorer_hook_blocklist.filter((n) => typeof n === 'string');
    }
    if (cfg && typeof cfg.per_monitor_modes === 'object' && cfg.per_monitor_modes) {
      // Drop mode ids that no longer exist. The fixed RGWB/RWBG presets were
      // removed (the Custom mode covers those layouts); keeping a stale id
      // would push it to the hook every sync for no effect.
      const VALID_MODES = new Set(['off', 'grayscale', 'rgb', 'bgr', 'custom']);
      textSettings.perMonitorModes = {};
      for (const [sig, mode] of Object.entries(cfg.per_monitor_modes)) {
        if (VALID_MODES.has(mode)) textSettings.perMonitorModes[sig] = mode;
      }
    }
    if (cfg && typeof cfg.per_monitor_knobs === 'object' && cfg.per_monitor_knobs) {
      textSettings.perMonitorKnobs = {};
      for (const [sig, k] of Object.entries(cfg.per_monitor_knobs)) {
        if (k && typeof k === 'object') {
          textSettings.perMonitorKnobs[sig] = {
            boldness: clampBoldness(k.boldness),
            damping: clampKnob(k.damping),
          };
        }
      }
    }
    if (cfg && typeof cfg.per_monitor_custom === 'object' && cfg.per_monitor_custom) {
      textSettings.perMonitorCustom = {};
      for (const [sig, offs] of Object.entries(cfg.per_monitor_custom)) {
        const normalized = customOffsetsFromConfig(offs);
        if (normalized) textSettings.perMonitorCustom[sig] = normalized;
      }
    }
    // Profiles saved before the EDID-id keying sit under sig keys that stop
    // matching after a resolution change; move them to the monitor id now
    // (no-op when the monitor list isn't loaded yet — loadMonitors re-runs
    // it).
    backfillTextMonitorKeys();
    // Restore the per-monitor selector after the monitor list loads.
    // Default the selector to the primary monitor; a saved selection wins
    // when it still exists.
    loadMonitors().then(() => {
      const saved = typeof cfg.active_monitor_sig === 'string' ? cfg.active_monitor_sig : '';
      const match = (saved && monitors.find((m) => m.sig === saved))
        || monitors.find((m) => m.primary)
        || monitors[0]
        || null;
      activeMonitor.mon = match;
      syncTextPerMonitor();
      syncTaskbarDimmerView();
      // main.js's startup Black Crush apply runs before the monitor list
      // resolves and can only resolve the profile key against the primary
      // display identity. Re-resolve now that the real selection is known so
      // the selected monitor's own profile wins (and its gamma is applied).
      if (configLoaded.v && app.displayInfo.refreshRateHz) {
        applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
      }
    }).catch(() => { syncTextPerMonitor(); });
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
    if (cfg && typeof cfg.screen_protector_fullscreen_detection === 'boolean') {
      screenDimmer.fullscreenDetection = cfg.screen_protector_fullscreen_detection;
    }
    if (cfg && typeof cfg.skip_favorite_confirmation === 'boolean') {
      appSettings.skipFavoriteConfirmation = cfg.skip_favorite_confirmation;
    }
    if (cfg && typeof cfg.taskbar_dimmer_opacity === 'number') taskbarDimmer.opacity = Math.max(0, Math.min(1, cfg.taskbar_dimmer_opacity));
    if (cfg && typeof cfg.taskbar_dimmer_fade_in_duration_ms === 'number') taskbarDimmer.fadeInDurationMs = Math.max(0, Math.min(1000, Math.round(cfg.taskbar_dimmer_fade_in_duration_ms)));
    if (cfg && typeof cfg.taskbar_dimmer_fade_out_duration_ms === 'number') taskbarDimmer.fadeOutDurationMs = Math.max(0, Math.min(1000, Math.round(cfg.taskbar_dimmer_fade_out_duration_ms)));
    if (cfg && typeof cfg.taskbar_dimmer_fade_linked === 'boolean') taskbarDimmer.fadeLinked = cfg.taskbar_dimmer_fade_linked;
    // Detection Method is global, not per monitor, so these are the only
    // place it lives in the config; the backend also seeds it at startup.
    if (cfg && typeof cfg.taskbar_dimmer_hover_detection === 'string') {
      taskbarDimmer.hoverDetection = cfg.taskbar_dimmer_hover_detection;
      Object.assign(dimmerDetections, detectionDefaultsForMode(cfg.taskbar_dimmer_hover_detection));
      invoke('set_taskbar_dimmer_hover_mode', { mode: cfg.taskbar_dimmer_hover_detection }).catch(() => {});
    }
    if (cfg && typeof cfg.taskbar_dimmer_poll_interval_ms === 'number' && Number.isFinite(cfg.taskbar_dimmer_poll_interval_ms)) {
      taskbarDimmer.pollIntervalMs = Math.max(16, Math.min(200, Math.round(cfg.taskbar_dimmer_poll_interval_ms)));
      invoke('set_taskbar_dimmer_poll_interval', { ms: taskbarDimmer.pollIntervalMs }).catch(() => {});
    }
    if (cfg && typeof cfg.taskbar_dimmer_profiles === 'object' && cfg.taskbar_dimmer_profiles) {
      // Per-monitor profiles win over the flat legacy fields above (which
      // stay as the pre-per-monitor format and the primary fallback).
      for (const key of Object.keys(taskbarDimmerProfiles)) delete taskbarDimmerProfiles[key];
      for (const [key, p] of Object.entries(cfg.taskbar_dimmer_profiles)) {
        if (!p || typeof p !== 'object') continue;
        taskbarDimmerProfiles[key] = {
          enabled: typeof p.enabled === 'boolean' ? p.enabled : TASKBAR_DIMMER_DEFAULTS.enabled,
          opacity: Math.max(0, Math.min(1, Number(p.opacity) || TASKBAR_DIMMER_DEFAULTS.opacity)),
          fadeInDurationMs: Math.max(0, Math.min(1000, Math.round(Number(p.fadeInDurationMs) || TASKBAR_DIMMER_DEFAULTS.fadeInDurationMs))),
          fadeOutDurationMs: Math.max(0, Math.min(1000, Math.round(Number(p.fadeOutDurationMs) || TASKBAR_DIMMER_DEFAULTS.fadeOutDurationMs))),
        };
      }
    }
    if (cfg && typeof cfg.screen_dimmer_profiles === 'object' && cfg.screen_dimmer_profiles) {
      // Per-monitor profiles win over the flat legacy fields above (which
      // stay as the pre-per-monitor format and the primary fallback).
      for (const key of Object.keys(screenDimmerProfiles)) delete screenDimmerProfiles[key];
      for (const [key, p] of Object.entries(cfg.screen_dimmer_profiles)) {
        if (!p || typeof p !== 'object') continue;
        screenDimmerProfiles[key] = {
          seconds: Math.max(5, Math.min(180, Math.round(Number(p.seconds) || SCREEN_DIMMER_DEFAULTS.seconds))),
          opacity: Math.max(0, Math.min(1, Number(p.opacity) || SCREEN_DIMMER_DEFAULTS.opacity)),
          fadeInDurationMs: Math.max(0, Math.min(5000, Math.round(Number(p.fadeInDurationMs) || SCREEN_DIMMER_DEFAULTS.fadeInDurationMs))),
          fadeOutDurationMs: Math.max(0, Math.min(5000, Math.round(Number(p.fadeOutDurationMs) || SCREEN_DIMMER_DEFAULTS.fadeOutDurationMs))),
          fullscreenDetection: typeof p.fullscreenDetection === 'boolean' ? p.fullscreenDetection : SCREEN_DIMMER_DEFAULTS.fullscreenDetection,
        };
      }
    }
    // Seed/re-key the dimmer profiles now that the config is in (no-op when
    // the monitor list isn't loaded yet — loadMonitors re-runs it), then
    // point the tabs' views at the selected monitor's profiles.
    backfillTaskbarDimmerProfiles();
    syncTaskbarDimmerView();
    backfillScreenDimmerProfiles();
    syncScreenDimmerView();
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
        const normalized = normalizeFavorite(fav);
        if (normalized) displayFavorites.push(normalized);
      }
    }
    backfillFavoriteMonitorIds();
    // Migrate the old slot-based favorite hotkeys (favorite_slot_N with a
    // favorite_hotkey_slots map) to hotkeys bound directly to the favorite
    // (favorite_<favId>). Runs before the prune below so mapped combos keep
    // their bindings; unmapped old favorite_slot_N entries (no slot map
    // entry) are treated as unknown favorites and pruned.
    const migrated = [];
    if (cfg && cfg.favorite_hotkey_slots && typeof cfg.favorite_hotkey_slots === 'object') {
      for (const [slot, favId] of Object.entries(cfg.favorite_hotkey_slots)) {
        if (typeof favId !== 'string' || !favId) continue;
        if (!displayFavorites.some((f) => f.favId === favId)) continue;
        const oldKey = `favorite_slot_${slot}`;
        const combo = hotkeys[oldKey];
        if (typeof combo !== 'string' || !combo) continue;
        const newKey = favoriteHotkeyAction(favId);
        if (!hotkeys[newKey]) {
          hotkeys[newKey] = combo;
          migrated.push({ action: newKey, combo });
        }
        delete hotkeys[oldKey];
      }
    }
    migrateFavoriteHotkeys = migrated;
  } catch (e) {
    console.warn('[config] load failed', e);
  } finally {
    configLoaded.v = true;
    applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
  }
  // One-time migration: make the migrated favorite hotkeys live in Rust
  // (set_hotkey registers them), prune any stale favorite_ actions left in
  // the config, then persist the migrated config.
  if (migrateFavoriteHotkeys.length) {
    for (const { action, combo } of migrateFavoriteHotkeys) {
      await invoke('set_hotkey', { action, combo }).catch(() => {});
    }
  }
  reconcileFavoriteHotkeys();
  if (migrateFavoriteHotkeys.length) saveConfig();
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
    const status = await invoke('get_taskbar_dimmer_status', { monitorKey: dimmerMonKey() });
    if (status) taskbarDimmer.available = status.available !== false;
  } catch { taskbarDimmer.available = false; }
  if (!taskbarDimmer.available) {
    // Transient false reading (tray restart, shell settling), re-check with
    // backoff instead of latching "unavailable" for the rest of the session.
    clearTimeout(dimmerAvailabilityRetryTimer);
    dimmerAvailabilityRetryTimer = null;
    dimmerAvailabilityRetryEpoch++;
    scheduleDimmerAvailabilityRecheck(5, dimmerAvailabilityRetryEpoch);
  }
  try {
    await invoke('set_taskbar_dimmer', { monitorKey: dimmerMonKey(), enabled: taskbarDimmer.enabled, opacity: taskbarDimmer.opacity, fadeInDurationMs: taskbarDimmer.fadeInDurationMs, fadeOutDurationMs: taskbarDimmer.fadeOutDurationMs, immediate: true });
  } catch (e) {
    // Transient failure (the shell can still be settling Shell_TrayWnd right
    // after a tray restart). Do NOT overwrite the loaded toggle state with
    // false, that silently erased the persisted ON state. The availability
    // recheck re-applies the settings once the taskbar is reachable again.
    console.warn('[taskbar-dimmer] initial apply failed, keeping loaded state', e);
  }
}

export function saveConfig() {
  if (!configLoaded.v) return Promise.resolve();
  persistCurrentBlackLevelProfile();
  const blackLiftProfiles = Object.fromEntries(Object.entries(crush.blackProfiles).map(([key, value]) => [key, clampAdjustment(value) / 10]));
  // The primary monitor's Taskbar Dimmer profile, used to keep the legacy
  // flat taskbar_dimmer_* fields in sync (the tray menu reads those as its
  // no-selector fallback). Falls back to the active monitor's view when the
  // primary is unknown.
  const primaryMon = monitors.find((m) => m.primary) || null;
  const primaryDimmerKey = primaryMon ? dimmerMonKey(primaryMon) : '';
  const primaryDimmerProfile = primaryDimmerKey ? taskbarDimmerProfiles[primaryDimmerKey] : null;
  const primaryScreenDimmerKey = primaryMon ? dimmerMonKey(primaryMon) : '';
  const primaryScreenDimmerProfile = primaryScreenDimmerKey ? screenDimmerProfiles[primaryScreenDimmerKey] : null;
  return invoke('save_config', { config: {
    black_lift: crush.adjustment / 10,
    black_lift_profiles: blackLiftProfiles,
    black_enabled: crush.blackEnabled,
    hide_calibration_interface: crush.hideCalibrationInterface,
    black_strip_gap_enabled: crush.blackStripGapEnabled,
    high_nits_min: crush.highNitsMin,
    high_nits_max: crush.highNitsMax,
    high_nits_step: crush.highNitsStep,
    taskbar_dimmer_enabled: primaryDimmerProfile?.enabled ?? taskbarDimmer.enabled,
    explorer_hook_enabled: textSettings.explorerHookEnabled,
    explorer_hook_all_apps: textSettings.hookAllApps,
    explorer_hook_blocklist: [...textSettings.hookBlocklist],
    per_monitor_modes: { ...textSettings.perMonitorModes },
    per_monitor_knobs: { ...textSettings.perMonitorKnobs },
    per_monitor_custom: Object.fromEntries(
      Object.entries(textSettings.perMonitorCustom).map(([sig, o]) => [sig, [[o.rx, o.ry], [o.gx, o.gy], [o.bx, o.by]]]),
    ),
    active_monitor_sig: activeMonitor.mon ? activeMonitor.mon.sig : '',
    screen_protector_enabled: screenDimmer.enabled,
    // The flat screen protector fields stay in sync with the PRIMARY
    // monitor's profile (the no-selector paths read these as fallback and
    // the backend seeds the primary from them at startup). The per-monitor
    // map below is the source of truth for every monitor.
    screen_protector_seconds: primaryScreenDimmerProfile?.seconds ?? screenDimmer.seconds,
    screen_protector_opacity: primaryScreenDimmerProfile?.opacity ?? screenDimmer.opacity,
    screen_protector_fade_in_duration_ms: primaryScreenDimmerProfile?.fadeInDurationMs ?? screenDimmer.fadeInDurationMs,
    screen_protector_fade_out_duration_ms: primaryScreenDimmerProfile?.fadeOutDurationMs ?? screenDimmer.fadeOutDurationMs,
    screen_protector_fade_linked: screenDimmer.fadeLinked,
    screen_protector_fullscreen_detection: primaryScreenDimmerProfile?.fullscreenDetection ?? screenDimmer.fullscreenDetection,
    screen_dimmer_profiles: { ...screenDimmerProfiles },
    skip_favorite_confirmation: appSettings.skipFavoriteConfirmation,
    // The flat legacy fields stay in sync with the PRIMARY monitor's profile
    // (the tray menu, which has no monitor selector, reads these as its
    // fallback and the backend seeds the primary from them at startup). The
    // per-monitor map below is the source of truth for every monitor.
    taskbar_dimmer_opacity: primaryDimmerProfile?.opacity ?? taskbarDimmer.opacity,
    taskbar_dimmer_fade_in_duration_ms: primaryDimmerProfile?.fadeInDurationMs ?? taskbarDimmer.fadeInDurationMs,
    taskbar_dimmer_fade_out_duration_ms: primaryDimmerProfile?.fadeOutDurationMs ?? taskbarDimmer.fadeOutDurationMs,
    taskbar_dimmer_fade_linked: taskbarDimmer.fadeLinked,
    // Detection Method is global, written straight from the view (the
    // backend stores it only in these flat fields, never in a profile).
    taskbar_dimmer_hover_detection: taskbarDimmer.hoverDetection || 'event',
    taskbar_dimmer_poll_interval_ms: taskbarDimmer.pollIntervalMs,
    taskbar_dimmer_profiles: { ...taskbarDimmerProfiles },
    display_menu_resolution_sort: displayMenuPrefs.resolutionSort,
    display_menu_resolution_group_by: displayMenuPrefs.resolutionGroupBy,
    display_menu_show_tags: displayMenuPrefs.showTags,
    display_menu_refresh_rate_sort: displayMenuPrefs.refreshRateSort,
    hotkeys: { ...hotkeys },
    favorite_display_modes: displayFavorites.map((f) => ({
      width: f.width,
      height: f.height,
      hz: f.hz,
      fav_id: f.favId,
      monitor_id: f.monitorId,
      monitor_sig: f.monitorSig,
      monitor_name: f.monitorName,
      confirmed_monitor_id: f.confirmedMonitorId || '',
    })),
  } }).catch((e) => {
    console.warn('[config] save failed', e);
    return undefined;
  });
}

// Persist the active monitor's Screen Dimmer profile from the current view
// fields, then push it (plus the global master toggle) to the backend.
// `persist` triggers a config save; slider drags pass false and the change
// handler commits once.
export function setScreenDimmer(
  enabled = screenDimmer.enabled,
  seconds = screenDimmer.seconds,
  opacity = screenDimmer.opacity,
  fadeInDurationMs = screenDimmer.fadeInDurationMs,
  fadeOutDurationMs = screenDimmer.fadeOutDurationMs,
  fullscreenDetection = screenDimmer.fullscreenDetection,
  persist = false
) {
  const profile = currentScreenDimmerProfile();
  if (profile) {
    profile.seconds = seconds;
    profile.opacity = opacity;
    profile.fadeInDurationMs = fadeInDurationMs;
    profile.fadeOutDurationMs = fadeOutDurationMs;
    profile.fullscreenDetection = fullscreenDetection;
  }
  const promise = invoke('power_set_screen_protector', {
    monitorKey: dimmerMonKey(),
    enabled,
    seconds,
    opacity,
    fadeInDurationMs,
    fadeOutDurationMs,
    fullscreenDetection,
  });
  if (persist) {
    promise.then(saveConfig).catch(() => {});
  }
  return promise;
}

export async function setScreenDimmerEnabled(enabled, tab = app.activeTab) {
  if (!enabled) {
    screenDimmer.enabled = false;
    await setScreenDimmer(false, screenDimmer.seconds, screenDimmer.opacity, screenDimmer.fadeInDurationMs, screenDimmer.fadeOutDurationMs, screenDimmer.fullscreenDetection, true);
    return;
  }

  // The dimmer runs in any session now: no UAC relaunch, the watcher starts
  // immediately and dims on idle alone. Only its media-playback detection
  // (pause while video plays) needs the elevated helper, so enabling also
  // registers the one-time Task Scheduler helper task in the background.
  // A declined or failed setup leaves the dimmer on (idle dimming works)
  // and only disables the media suppression; the next enable retries.
  screenDimmer.enabled = true;
  await setScreenDimmer(true, screenDimmer.seconds, screenDimmer.opacity, screenDimmer.fadeInDurationMs, screenDimmer.fadeOutDurationMs, screenDimmer.fullscreenDetection, true);
  try {
    await invoke('ensure_helper_task');
  } catch (error) {
    console.warn('[screen-dimmer] privileged helper setup unavailable; media detection disabled', error);
  }
}

let pendingTaskbarDimmerUpdate = null;
let taskbarDimmerUpdateInFlight = false;
let taskbarDimmerUpdateVersion = 0;
// A persist=true update that gets superseded by a newer (possibly
// persist=false) update would lose its save under the plain version check,
// e.g. toggling the dimmer right as the startup availability recovery
// re-applies settings, which silently dropped the toggle save and left the
// config stuck on the old value. Remember the save intent and flush it once
// the pipeline settles.
let taskbarDimmerSaveQueued = false;
function emitTaskbarDimmerChanged() {
  emit('taskbar-dimmer-changed', {
    // monitorKey lets the tray menu (which is primary-scoped) ignore edits
    // made to secondary monitors.
    monitorKey: dimmerMonKey(),
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
    // Pipeline drained, honor any save that a superseded update requested.
    if (taskbarDimmerSaveQueued) {
      taskbarDimmerSaveQueued = false;
      saveConfig();
    }
    return;
  }
  pendingTaskbarDimmerUpdate = null;
  taskbarDimmerUpdateInFlight = true;
  invoke('set_taskbar_dimmer', { monitorKey: update.monitorKey, enabled: update.enabled, opacity: update.opacity, fadeInDurationMs: update.fadeInDurationMs, fadeOutDurationMs: update.fadeOutDurationMs, immediate: update.immediate })
    .then(() => {
      if (update.version === taskbarDimmerUpdateVersion) {
        // This is the latest update; it owns the save decision. Also honor a
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
      // A failed persist=true update still represents the user's intent, queue
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
  // Persist the edit into the active monitor's profile. Nothing is global
  // here: the on/off switch and the opacity/fades all live per monitor, so
  // toggling from the Taskbar Dimmer tab turns on/off the selected monitor's
  // own overlay.
  const profile = currentDimmerProfile();
  if (profile) {
    profile.enabled = taskbarDimmer.enabled;
    profile.opacity = taskbarDimmer.opacity;
    profile.fadeInDurationMs = taskbarDimmer.fadeInDurationMs;
    profile.fadeOutDurationMs = taskbarDimmer.fadeOutDurationMs;
  }
  taskbarDimmerUpdateVersion++;
  pendingTaskbarDimmerUpdate = { monitorKey: dimmerMonKey(), enabled: taskbarDimmer.enabled, opacity: taskbarDimmer.opacity, fadeInDurationMs: taskbarDimmer.fadeInDurationMs, fadeOutDurationMs: taskbarDimmer.fadeOutDurationMs, persist, immediate, version: taskbarDimmerUpdateVersion };
  flushTaskbarDimmerUpdate();
}
// Global convenience: turn every monitor's taskbar dimmer on or off at once,
// while keeping each monitor's own opacity/fades. Used by the nav switch and
// the hotkey, so the app keeps a one-switch-all way to silence the dimmers
// even though the per-monitor on/off lives in each profile now.
export function setTaskbarDimmerEnabledForAll(enabled) {
  taskbarDimmer.enabled = enabled;
  // Update every monitor's profile (seeding a default profile for any
  // monitor without one) so each monitor carries the new on/off state. A
  // disconnected monitor's profile is covered by the second loop below.
  for (const mon of monitors) {
    const key = dimmerMonKey(mon);
    if (!key) continue;
    const p = (taskbarDimmerProfiles[key] ||= {
      enabled: TASKBAR_DIMMER_DEFAULTS.enabled,
      opacity: TASKBAR_DIMMER_DEFAULTS.opacity,
      fadeInDurationMs: TASKBAR_DIMMER_DEFAULTS.fadeInDurationMs,
      fadeOutDurationMs: TASKBAR_DIMMER_DEFAULTS.fadeOutDurationMs,
    });
    p.enabled = enabled;
  }
  for (const p of Object.values(taskbarDimmerProfiles)) p.enabled = enabled;
  saveConfig();
  invoke('set_taskbar_dimmer_enabled_for_all', { enabled, immediate: false })
    .then(() => emitTaskbarDimmerChanged())
    .catch((e) => console.warn('[taskbar-dimmer] for-all update failed', e));
}
export function restoreTaskbarDimmerDefaults() {
  taskbarDimmer.enabled = TASKBAR_DIMMER_DEFAULTS.enabled;
  taskbarDimmer.opacity = TASKBAR_DIMMER_DEFAULTS.opacity;
  taskbarDimmer.fadeInDurationMs = TASKBAR_DIMMER_DEFAULTS.fadeInDurationMs;
  taskbarDimmer.fadeOutDurationMs = TASKBAR_DIMMER_DEFAULTS.fadeOutDurationMs;
  taskbarDimmer.fadeLinked = TASKBAR_DIMMER_DEFAULTS.fadeLinked;
  // Reset the active monitor's profile entry (on/off included, so the
  // selected monitor's dimmer turns off too) to the canonical defaults.
  const profile = currentDimmerProfile();
  if (profile) {
    profile.enabled = TASKBAR_DIMMER_DEFAULTS.enabled;
    profile.opacity = TASKBAR_DIMMER_DEFAULTS.opacity;
    profile.fadeInDurationMs = TASKBAR_DIMMER_DEFAULTS.fadeInDurationMs;
    profile.fadeOutDurationMs = TASKBAR_DIMMER_DEFAULTS.fadeOutDurationMs;
  }
  // Detection Method is global and lives on the Settings tab, so it is not
  // reset here; the Settings tab's own restore handles it.
  setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, taskbarDimmer.fadeInDurationMs, taskbarDimmer.fadeOutDurationMs, true, true);
}

// Rust retries overlay creation in the background if it fails at startup
// (e.g. the taskbar/shell hadn't settled yet) and emits this event once that
// retry succeeds. get_taskbar_dimmer_status in loadConfig only runs once, at
// startup, before a background retry can possibly finish, without this the
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
// fired, without this a stale resolution could schedule a second chain.
let dimmerAvailabilityRetryEpoch = 0;
function scheduleDimmerAvailabilityRecheck(attemptsLeft, epoch) {
  if (attemptsLeft <= 0) return;
  // 1s, 2s, 3s, 4s, 5s after the initial false reading, bounded, so a
  // genuinely missing taskbar just gives up quietly.
  const delayMs = 1000 * (6 - attemptsLeft);
  dimmerAvailabilityRetryTimer = setTimeout(() => {
    dimmerAvailabilityRetryTimer = null;
    invoke('get_taskbar_dimmer_status', { monitorKey: dimmerMonKey() })
      .then((status) => {
        if (epoch !== dimmerAvailabilityRetryEpoch) return;
        if (status && status.available) {
          // Re-apply the user's settings, any set_taskbar_dimmer call made
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
  // Restore defaults resets gamma on every monitor, not just the selected
  // one. The device name disambiguates identical panels sharing a sig.
  for (const m of monitors) {
    invoke('apply_gamma_ramp_for', { sig: m.sig, deviceName: m.device_name || null, blackLift: 0 }).catch(() => {});
  }
  invoke('apply_gamma_ramp', { blackLift: 0 }).catch((e) => console.warn('[restore-all] gamma reset failed', e));
}
export function setDimmerSuspendedForFullscreen(suspended) { invoke('set_taskbar_dimmer_suspended', { suspended }).catch(() => {}); }
export function setHoverDetection(mode) {
  const previous = taskbarDimmer.hoverDetection;
  taskbarDimmer.hoverDetection = mode;
  // Detection Method is global, so there is no per-monitor profile to write.
  Object.assign(dimmerDetections, detectionDefaultsForMode(mode));
  invoke('set_taskbar_dimmer_hover_mode', { mode }).catch((e) => { console.warn('[hover-detection] toggle failed', e); taskbarDimmer.hoverDetection = previous; Object.assign(dimmerDetections, detectionDefaultsForMode(previous)); });
}
let pollIntervalSaveTimer = null;
let pollIntervalInvokeTimer = null;
export function setPollIntervalMs(ms) {
  const clamped = Math.max(16, Math.min(200, Math.round(ms)));
  taskbarDimmer.pollIntervalMs = clamped;
  // Detection Method is global, so there is no per-monitor profile to write.
  // Debounce the IPC call too: the slider fires on every input event during
  // a drag, and the polling cadence only matters once the user settles on
  // one.
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
        // frozen for the whole session, the WebView still scales the canvas
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
    // Keep the last-known model, a transient vendor refresh returning an
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

// NVIDIA "Override to reference mode" is a driver-control setting that can
// change while the app is open (NVIDIA Control Panel) with no Windows display
// notification. The Black Crush tab polls this cheap registry-backed flag so
// its warning clears and gamma resumes as soon as the driver setting is
// disabled; the one-shot refreshVendorDisplayInfo on tab entry goes stale,
// and polling that runs the full NVAPI chain on every tick. The backend caches
// the flag for 5 s, so this 3 s poll notices a change within a few seconds of
// the cache expiring.
let referenceModePollTimer = null;
export function startReferenceModePolling() {
  if (referenceModePollTimer !== null) return;
  const poll = () => {
    invoke('get_nvidia_reference_mode')
      .then((enabled) => { app.displayInfo.referenceMode = enabled === true; })
      .catch(() => {});
  };
  poll();
  referenceModePollTimer = setInterval(poll, 3000);
}
export function stopReferenceModePolling() {
  if (referenceModePollTimer !== null) {
    clearInterval(referenceModePollTimer);
    referenceModePollTimer = null;
  }
}

let displayPollSequence = 0;
let displayInfoRetryCount = 0;
let displayInfoRefreshTimer = null;
let monitorRefreshRetryTimer = null;
let monitorRefreshRetryCount = 0;

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
    app.displayInfo = { width: nextWidth, height: nextHeight, deviceName: nextDeviceName, monitorName: typeof info.monitor_name === 'string' ? info.monitor_name : '', monitorId: nextMonitorId, profileId: typeof info.profile_id === 'string' ? info.profile_id : '', nativeWidth: Number(info.native_width) || 0, nativeHeight: Number(info.native_height) || 0, nativeSource: typeof info.native_source === 'string' ? info.native_source : '', gpuModel: typeof info.gpu_model === 'string' && info.gpu_model ? info.gpu_model : prevGpuModel, vendor: prevVendor, connector: typeof info.connector === 'string' ? info.connector : '', colorFormat: typeof info.color_format === 'string' ? info.color_format : '', bitsPerChannel: Number(info.bits_per_channel) || 0, desktopBits: Number(info.desktop_bits) || 0, dynamicRange: typeof info.dynamic_range === 'string' ? info.dynamic_range : '', vrr: typeof info.vrr === 'string' ? info.vrr : '', referenceMode: info.reference_mode === true, refreshRateHz: nextRefreshRate, refreshRates: Array.isArray(info.refresh_rates) ? info.refresh_rates : [], resolutions: Array.isArray(info.resolutions) ? info.resolutions : [] };
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
