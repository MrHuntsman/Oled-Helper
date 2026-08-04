import { mount } from 'svelte'
import './app.css'
import App from './App.svelte'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { pushGpuLog } from './lib/stores.svelte.js'

// ── Forward uncaught errors / console output to the Rust-side file log ─────
// This runs before App mounts, so it catches crashes that happen during
// mount/init too — not just ones the in-app Diagnostics tab can display
// (that tab only exists once Svelte has successfully mounted). Best-effort:
// swallow failures here, since logging must never itself break startup.
function logToFile(level, message) {
  try { invoke('log_frontend', { level, message }).catch(() => {}); } catch (_) {}
}

// Rust-native diagnostics (such as the taskbar overlay timer) use the same
// runtime buffer as browser logs. They are already written to the Rust file
// logger, so do not send them back through logToFile or they would duplicate.
listen('runtime-log', (event) => {
  const entry = event.payload;
  if (!entry || typeof entry.message !== 'string') return;
  const level = entry.level === 'error' || entry.level === 'warn' ? entry.level : 'info';
  setTimeout(() => pushGpuLog(level, entry.message, { persist: false }), 0);
}).catch(() => {});

function formatLogArg(value) {
  if (value instanceof Error) return value.stack || `${value.name}: ${value.message}`;
  if (typeof value === 'string') return value;
  try { return JSON.stringify(value); } catch (_) { return String(value); }
}

function mirrorRuntimeLog(level, args) {
  const message = args.map(formatLogArg).join(' ');
  // Defer the reactive-buffer write. Console calls can happen while a Svelte
  // effect is executing; mutating gpuLogVersion synchronously there causes
  // effect_update_depth_exceeded. The Rust file log remains immediate.
  setTimeout(() => pushGpuLog(level, message, { persist: false }), 0);
  logToFile(level, message);
}

window.addEventListener('error', (e) => {
  const message = `Uncaught error: ${e.message} at ${e.filename}:${e.lineno}:${e.colno}\n${e.error?.stack || ''}`;
  setTimeout(() => pushGpuLog('error', message, { persist: false }), 0);
  logToFile('error', message);
});
window.addEventListener('unhandledrejection', (e) => {
  const message = `Unhandled promise rejection: ${formatLogArg(e.reason)}`;
  setTimeout(() => pushGpuLog('error', message, { persist: false }), 0);
  logToFile('error', message);
});

// Capture-phase listener for failed subresource loads (broken icon after a
// tray restart). Browser-generated resource errors do NOT reach window.onerror
// or the console bridge above, so log them here explicitly with the failing
// URL — that pinpoints whether an image failed to fetch (net::ERR_*), was
// served a corrupt body, or failed to decode.
window.addEventListener('error', (e) => {
  const target = e.target;
  if (!target || !target.tagName) return;
  const tag = String(target.tagName).toLowerCase();
  if (tag !== 'img' && tag !== 'script' && tag !== 'link' && tag !== 'audio' && tag !== 'video') return;
  const src = target.currentSrc || target.src || target.href || '';
  const detail = (target.error && target.error.message) || e.message || '';
  const message = `Resource load failed: <${tag}> ${src}${detail ? ` — ${detail}` : ''}`;
  setTimeout(() => pushGpuLog('error', message, { persist: false }), 0);
  logToFile('error', message);
}, true);

// Mirror all browser console levels to both the in-app Debug log and the
// Rust-side file/terminal log. GPU call sites use these same console methods,
// so they are included exactly once alongside Taskbar Dimmer and Settings
// warnings/errors.
const _consoleLog = console.log.bind(console);
console.log = (...args) => { _consoleLog(...args); mirrorRuntimeLog('info', args); };
const _consoleError = console.error.bind(console);
console.error = (...args) => { _consoleError(...args); mirrorRuntimeLog('error', args); };
const _consoleWarn = console.warn.bind(console);
console.warn = (...args) => { _consoleWarn(...args); mirrorRuntimeLog('warn', args); };

// Visible canary — proves the script even runs
document.title = 'OLED Helper';
// Remove the loading fallback
try { const pb = document.getElementById('preboot'); if (pb) pb.remove(); } catch (_) {}

try {
  const target = document.getElementById('app');
  if (!target) throw new Error('#app not found');

  const app = mount(App, { target });

  // Block right-click context menu — this is an app, not a website
  document.addEventListener('contextmenu', e => e.preventDefault());

  console.log('OLED Helper mounted OK');
  logToFile('info', 'OLED Helper mounted OK');

  // Fallback layer for the Rust-side pre-build WebView2 cache purge: clears
  // through the WebView2 API itself, so it works regardless of the purge's
  // hardcoded path. Tray restarts on older builds hard-killed the process
  // mid-write, corrupting cache entries for some embedded icons (served back
  // as broken images). Delayed a couple of seconds so the already-healthy
  // initial page load is not disturbed.
  setTimeout(() => {
    invoke('clear_webview_cache').catch((err) => {
      logToFile('warn', `cache clear failed: ${String(err)}`);
    });
  }, 2000);
} catch (e) {
  console.error('OLED Helper mount failed:', e);
  // Show the error visibly so we can debug without DevTools
  document.body.innerHTML = `<div style="color:#E05555;padding:20px;font:14px monospace;background:#0D0D0F;height:100vh;">
    <h2>Startup Error</h2>
    <pre>${e.message}\n${e.stack || ''}</pre>
  </div>`;
}

export default {}
