import { mount } from 'svelte'
import './app.css'
import App from './App.svelte'
import { invoke } from '@tauri-apps/api/core'

// ── Forward uncaught errors / console output to the Rust-side file log ─────
// This runs before App mounts, so it catches crashes that happen during
// mount/init too — not just ones the in-app Diagnostics tab can display
// (that tab only exists once Svelte has successfully mounted). Best-effort:
// swallow failures here, since logging must never itself break startup.
function logToFile(level, message) {
  try { invoke('log_frontend', { level, message }).catch(() => {}); } catch (_) {}
}

window.addEventListener('error', (e) => {
  logToFile('error', `Uncaught error: ${e.message} at ${e.filename}:${e.lineno}:${e.colno}\n${e.error?.stack || ''}`);
});
window.addEventListener('unhandledrejection', (e) => {
  logToFile('error', `Unhandled promise rejection: ${e.reason?.message || e.reason}\n${e.reason?.stack || ''}`);
});

// Mirror console.error/warn to the file log too. Deliberately NOT mirroring
// console.log (too noisy/high-frequency — e.g. render loops — and console.log
// call sites should route through dlog/pushGpuLog in stores.svelte.js if the
// message is worth persisting). This mirror is synchronous-looking but the
// actual invoke() is fire-and-forget async, so it never blocks the caller.
const _consoleError = console.error.bind(console);
console.error = (...args) => { _consoleError(...args); logToFile('error', args.map(String).join(' ')); };
const _consoleWarn = console.warn.bind(console);
console.warn = (...args) => { _consoleWarn(...args); logToFile('warn', args.map(String).join(' ')); };

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
} catch (e) {
  console.error('OLED Helper mount failed:', e);
  // Show the error visibly so we can debug without DevTools
  document.body.innerHTML = `<div style="color:#E05555;padding:20px;font:14px monospace;background:#0D0D0F;height:100vh;">
    <h2>Startup Error</h2>
    <pre>${e.message}\n${e.stack || ''}</pre>
  </div>`;
}

export default {}
