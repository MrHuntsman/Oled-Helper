import { mount } from 'svelte'
import './app.css'
import App from './App.svelte'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { pushGpuLog, debugSettings, app, crush, taskbarDimmer, handleTaskbarDimmerAvailable, syncFavoritesFromBackend, loadConfig, applyBlackLevelProfileForRefreshRate } from './lib/stores.svelte.js'

// The Rust tauri-plugin-log Webview target is the canonical fan-out path:
// every record is written to stdout/file and emitted as `log://log`. We do not
// write directly to the Debug store from console interception, because that
// would make the three destinations diverge or duplicate records.
const nativeConsole = {
  trace: console.trace.bind(console),
  debug: console.debug.bind(console),
  info: console.info.bind(console),
  log: console.log.bind(console),
  warn: console.warn.bind(console),
  error: console.error.bind(console),
};

function normalizeLevel(level) {
  if (typeof level === 'number') {
    return ['trace', 'debug', 'info', 'warn', 'error'][Math.max(1, Math.min(5, level)) - 1] || 'info';
  }
  return ['trace', 'debug', 'info', 'warn', 'error'].includes(level) ? level : 'info';
}

function formatLogArg(value) {
  if (value instanceof Error) return value.stack || `${value.name}: ${value.message}`;
  if (typeof value === 'string') return value;
  if (value === undefined) return 'undefined';
  if (value === null) return 'null';
  try {
    const json = JSON.stringify(value);
    return json === undefined ? String(value) : json;
  } catch (_) {
    return String(value);
  }
}

function formatConsoleArgs(args) {
  return args.map(formatLogArg).join(' ');
}

function showCanonicalRecord(level, message, ts) {
  const method = nativeConsole[level] || nativeConsole.info;
  // This is the only call that reaches DevTools. Since it uses the saved
  // native method, it cannot re-enter the console interception below.
  method(message);
  // Svelte state is updated asynchronously so console calls from an $effect
  // cannot trigger effect_update_depth_exceeded. `ts` is passed through for
  // replayed startup records so the Debug tab shows their true log time.
  setTimeout(() => pushGpuLog(level, message, ts), 0);
}

function sendFrontendRecord(level, message) {
  // Rust assigns this record to the same log facade used by native messages;
  // tauri-plugin-log then fans it out to stdout, file, Webview, and here.
  try {
    invoke('log_frontend', { level, message }).catch(() => {
      // If the WebView is shutting down, there may be no Rust listener left.
      // Keep the failure visible in DevTools without recursively logging it.
      (nativeConsole[level] || nativeConsole.error)(message);
    });
  } catch (_) {
    (nativeConsole[level] || nativeConsole.error)(message);
  }
}

// Receive every Rust/native/frontend record from the plugin's Webview target.
// Numeric levels are tauri-plugin-log's LogLevel repr: 1 trace, 2 debug,
// 3 info, 4 warn, 5 error.
let dedupeWindowActive = false;
// Map of (level, message) -> count of live deliveries during the handshake.
// Counts (not a Set) so two identical buffered records matching one live
// record both still get replayed once.
const liveDuringReplayWindow = new Map();

listen('log://log', (event) => {
  const payload = event.payload;
  if (!payload || typeof payload.message !== 'string') return;
  const level = normalizeLevel(payload.level);
  const message = payload.message;
  // Records emitted while the startup replay handshake is in flight can be
  // BOTH buffered in Rust and delivered here live ,  remember them so the
  // replay below skips them instead of printing duplicates.
  if (dedupeWindowActive) {
    const key = `${level}\u0000${message}`;
    liveDuringReplayWindow.set(key, (liveDuringReplayWindow.get(key) || 0) + 1);
  }
  showCanonicalRecord(level, message);
}).then(() => {
  // The listener is live now. Drain Rust's pre-WebView history (setup, config
  // load, tray/dimmer init) and replay it so startup records also reach the
  // Debug tab and DevTools exactly once, keeping all three views 1:1.
  dedupeWindowActive = true;
  invoke('take_log_history').then((entries) => {
    // True chronological order, oldest first, with their original timestamps.
    const ordered = [...(entries || [])].sort((a, b) => (a.ts_ms || 0) - (b.ts_ms || 0));
    for (const entry of ordered) {
      if (!entry || typeof entry.message !== 'string') continue;
      const level = normalizeLevel(entry.level);
      const key = `${level}\u0000${entry.message}`;
      const count = liveDuringReplayWindow.get(key);
      if (count) { liveDuringReplayWindow.set(key, count - 1); continue; }
      showCanonicalRecord(level, entry.message, entry.ts_ms);
    }
    // Keep the window open briefly for events still in flight, then close it.
    setTimeout(() => {
      liveDuringReplayWindow.clear();
      dedupeWindowActive = false;
    }, 250);
  }).catch(() => { dedupeWindowActive = false; });
}).catch(() => {});

// Async config writes run on a Rust worker thread, so a background write
// failure can't surface through the save_config invoke promise. Rust emits
// this event instead; routing it through log_frontend keeps it in the same
// 1:1 pipeline as everything else.
listen('config-write-failed', (event) => {
  sendFrontendRecord('warn', `[config] disk save failed: ${event.payload}`);
}).catch(() => {});

// Rust's overlay-creation retry (see lib.rs setup()) emits this once the
// taskbar dimmer overlay comes online after an initial startup failure.
listen('taskbar-dimmer-available', () => {
  handleTaskbarDimmerAvailable();
}).catch(() => {});

// Favorites can be toggled from the tray menu (which has its own window). The
// main window only reads config at load time, so the backend emits this with
// the fresh list to keep the resolution dropdown stars and favorites menu in
// sync without a full config reload.
listen('favorites-changed', (event) => {
  syncFavoritesFromBackend(event.payload);
}).catch(() => {});

// The custom tray window uses the same backend dimmer command but has a
// separate Svelte store instance. Mirror its state here so the Taskbar Dimmer
// tab's switch and slider update immediately after a tray interaction.
listen('taskbar-dimmer-changed', (event) => {
  const status = event.payload;
  if (!status) return;
  if (typeof status.enabled === 'boolean') taskbarDimmer.enabled = status.enabled;
  if (typeof status.opacity === 'number') taskbarDimmer.opacity = Math.max(0, Math.min(1, status.opacity));
  if (typeof status.fadeInDurationMs === 'number') taskbarDimmer.fadeInDurationMs = Math.max(0, Math.min(1000, Math.round(status.fadeInDurationMs)));
  if (typeof status.fadeOutDurationMs === 'number') taskbarDimmer.fadeOutDurationMs = Math.max(0, Math.min(1000, Math.round(status.fadeOutDurationMs)));
}).catch(() => {});

listen('black-level-changed', (event) => {
  const status = event.payload;
  if (!status) return;
  if (typeof status.adjustment === 'number') crush.adjustment = Math.max(-150, Math.min(150, Math.round(status.adjustment)));
  if (typeof status.enabled === 'boolean') crush.blackEnabled = status.enabled;
}).catch(() => {});

listen('sdr-content-brightness-changed', (event) => {
  const nits = Number(event.payload?.nits);
  if (Number.isFinite(nits) && nits >= 80 && nits <= 480) app.sdrWhiteNits = nits;
}).catch(() => {});

// Capture uncaught errors before Svelte mounts. They are sent to Rust and then
// return through `log://log`, so each appears once in all three destinations.
window.addEventListener('error', (event) => {
  // Resource failures are handled by the capture-phase listener below.
  if (event.target && event.target !== window) return;
  const message = `Uncaught error: ${event.message} at ${event.filename}:${event.lineno}:${event.colno}\n${event.error?.stack || ''}`;
  sendFrontendRecord('error', message);
});

window.addEventListener('unhandledrejection', (event) => {
  sendFrontendRecord('error', `Unhandled promise rejection: ${formatLogArg(event.reason)}`);
});

// Browser-generated resource errors do not reach window.onerror or console
// interception. Convert them into normal canonical records instead. NOTE:
// Chromium/WebView2 also prints its own native "Failed to load resource"
// line to DevTools that JS cannot intercept, so resource failures appear
// twice in DevTools (one native line + one canonical record) ,  the terminal,
// file, and Debug tab see exactly one copy.
window.addEventListener('error', (event) => {
  const target = event.target;
  if (!target || !target.tagName) return;
  const tag = String(target.tagName).toLowerCase();
  if (!['img', 'script', 'link', 'audio', 'video'].includes(tag)) return;
  const src = target.currentSrc || target.src || target.href || '';
  const detail = (target.error && target.error.message) || event.message || '';
  sendFrontendRecord('error', `Resource load failed: <${tag}> ${src}${detail ? ` ,  ${detail}` : ''}`);
}, true);


// Intercept every standard console level. The original call is deliberately
// not made here; Rust is the single source of truth and the event above makes
// the one corresponding DevTools call after the record is accepted by Rust.
for (const level of ['trace', 'debug', 'info', 'log', 'warn', 'error']) {
  console[level] = (...args) => sendFrontendRecord(level === 'log' ? 'info' : level, formatConsoleArgs(args));
}

document.title = 'OLED Helper';
try { const pb = document.getElementById('preboot'); if (pb) pb.remove(); } catch (_) {}

try {
  const target = document.getElementById('app');
  if (!target) throw new Error('#app not found');

  // Load persisted config (black level, high-nits range, dimmer, etc.) AND
  // seed the current display's monitor/mode identity BEFORE the first
  // mount/render. Previously both ran from effects inside App.svelte that
  // fire after the initial paint, so the Black Level slider (and other
  // saved-per-display values) visibly rendered at their neutral default for
  // a frame and then jumped to the saved value once the async invokes
  // resolved. Awaiting both here means crush.adjustment already reflects the
  // saved profile for the CURRENT monitor/resolution/refresh-rate the very
  // first time Svelte renders, so there is nothing to jump from.
  //
  // get_display_info alone (not the full pollDisplayInfo helper) is enough:
  // it only needs to seed app.displayInfo's identity fields so
  // applyBlackLevelProfileForRefreshRate can resolve the right profile key.
  // App.svelte's own onMount still calls pollDisplayInfo() afterward for the
  // resolutions/refresh-rates lists and to keep polling wired up; that call
  // is cheap and harmless once this identity is already seeded.
  Promise.allSettled([
    loadConfig(),
    invoke('get_display_info').then((info) => {
      if (!info) return;
      app.displayInfo.width = info.width || 0;
      app.displayInfo.height = info.height || 0;
      app.displayInfo.deviceName = typeof info.device_name === 'string' ? info.device_name : '';
      app.displayInfo.monitorId = typeof info.monitor_id === 'string' ? info.monitor_id : '';
      app.displayInfo.refreshRateHz = info.refresh_hz || null;
    }).catch(() => {}),
  ]).then(() => {
    // Both the config and the display identity are in, resolve the black
    // level for the actual current mode before the first paint.
    applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
  }).catch((e) => console.error('[startup] preload failed', e)).finally(() => {
    mount(App, { target });
    document.addEventListener('contextmenu', e => { if (!debugSettings.contextMenuEnabled) e.preventDefault(); });
  });
} catch (e) {
  console.error('OLED Helper mount failed:', e);
  document.body.innerHTML = `<div style="color:#E05555;padding:20px;font:14px monospace;background:#0D0D0F;height:100vh;">
    <h2>Startup Error</h2>
    <pre>${e.message}\n${e.stack || ''}</pre>
  </div>`;
}

export default {}
