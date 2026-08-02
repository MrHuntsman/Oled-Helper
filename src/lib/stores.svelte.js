// Svelte 5 runes stores — $state object pattern
import { invoke } from '@tauri-apps/api/core';

export const app = $state({
  activeTab: 'crush',
  hdrStatus: 'unknown',  // set by pollHDRStatus via matchMedia
  displayInfo: { width: 0, height: 0, refreshRateHz: null },
});

export const crush = $state({
  adjustment: 0,
  blackEnabled: true,
  squareCount: 12,
  fullscreen: 'normal',
  showHint: true,
});

export function adjustmentDisplay() {
  if (!crush.blackEnabled) return 'OFF';
  return `${crush.adjustment > 0 ? '+' : ''}${(crush.adjustment / 10).toFixed(1)}`;
}
export function blackLift() {
  return crush.blackEnabled ? crush.adjustment : 0;
}

export const configLoaded = $state({ v: false });

// Load persisted config (black_lift in tenths-display units, e.g. 9 means
// +0.9, matching what's shown in the UI/slider) once at startup and apply
// it to the crush store before the UI reacts to it.
export async function loadConfig() {
  try {
    const cfg = await invoke('load_config');
    if (cfg && typeof cfg.black_lift === 'number') {
      crush.adjustment = Math.round(cfg.black_lift * 10);
    }
    if (cfg && typeof cfg.black_enabled === 'boolean') {
      crush.blackEnabled = cfg.black_enabled;
    }
  } catch (e) {
    console.warn('[config] load failed', e);
  } finally {
    // Gate saveConfig() until after the initial load resolves, so TabCrush's
    // mount-time effect (which fires with the default adjustment=0 before
    // this async load completes) doesn't immediately overwrite the file
    // we just read from.
    configLoaded.v = true;
  }
}

// Fire-and-forget save, called whenever the black level or enabled state
// changes. black_lift is written in the same units shown in the UI (tenths
// of crush.adjustment), so the config file reads e.g. 9 instead of 90.
export function saveConfig() {
  if (!configLoaded.v) return;
  invoke('save_config', { config: { black_lift: crush.adjustment / 10, black_enabled: crush.blackEnabled } }).catch((e) => {
    console.warn('[config] save failed', e);
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

export function pollDisplayInfo() {
  const s = window.screen;
  if (!s) return;
  const dpr = window.devicePixelRatio || 1;
  // Fall back to browser-reported size immediately so the UI has something
  // to show while the async Tauri call resolves.
  app.displayInfo = {
    width: Math.round(s.width * dpr),
    height: Math.round(s.height * dpr),
    refreshRateHz: app.displayInfo.refreshRateHz ?? null,
  };
  // Browsers don't expose real refresh rate, so ask the Tauri backend
  // (Win32 EnumDisplaySettingsW) for accurate resolution + Hz.
  invoke('get_display_info').then((info) => {
    if (!info) return;
    app.displayInfo = {
      width: info.width,
      height: info.height,
      refreshRateHz: info.refresh_hz || null,
    };
  }).catch(() => {});
}

// Plain tick counter bumped alongside the poll timer in App.svelte, so any
// component can derive off it to re-read non-reactive globals (navigator.*,
// screen.*, devicePixelRatio) on a schedule without needing its own timer.
export const envTick = $state({ n: 0 });
export function bumpEnvTick() {
  envTick.n++;
}

// ── GPU init log — plain (non-reactive) ring buffer + a reactive version counter.
// Writes just push to a plain array and bump a $state number; the Debug tab reads
// the array but only re-renders when the counter changes. This avoids Svelte 5
// deep-reactivity overhead/cascades from many rapid mutations to a $state array,
// which caused the app to hang on startup when GPU init logged in a tight loop.
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
const GPU_LOG_MAX = 300;
export const gpuLogEntries = [];
export const gpuLogVersion = $state({ n: 0 });

export function pushGpuLog(level, message) {
  gpuLogEntries.push({ level, message, ts: Date.now() });
  if (gpuLogEntries.length > GPU_LOG_MAX) gpuLogEntries.shift();
  gpuLogVersion.n++;
  // Best-effort mirror to the on-disk log (see src-tauri log_frontend command)
  // so the GPU init sequence survives even if the app hangs/crashes before
  // the user can copy the in-app log. Fire-and-forget; never awaited, never
  // throws synchronously — safe to call from anywhere pushGpuLog is called.
  try { invoke('log_frontend', { level, message: `[gpu] ${message}` }).catch(() => {}); } catch (_) {}
}

export function clearGpuLog() {
  gpuLogEntries.length = 0;
  gpuLogVersion.n++;
}
