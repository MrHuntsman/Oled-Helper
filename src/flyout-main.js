import { mount } from 'svelte'
import './app.css'
import './flyout.css'
import Flyout from './lib/Flyout.svelte'
import { invoke } from '@tauri-apps/api/core'

// Entry point for the tray mode-confirmation flyout window (flyout.html).
// Deliberately minimal next to main.js: no console/log bridge, no autostart
// plugin usage, no config loading; this window only ever renders the single
// confirm/revert dialog driven by events from Rust (see Flyout.svelte).
document.title = 'OLED Helper';

// Diagnostic only, temporary: confirms in the on-disk/terminal log whether
// this webview ever actually finishes loading and mounting Svelte, since a
// silently-blank transparent window is otherwise indistinguishable from "the
// window never appeared" just from watching the screen.
invoke('log_frontend', { level: 'info', message: '[flyout] script loaded, mounting Svelte' }).catch(() => {});

try {
  const target = document.getElementById('flyout-app');
  if (!target) throw new Error('#flyout-app not found');
  mount(Flyout, { target });
  invoke('log_frontend', { level: 'info', message: '[flyout] Svelte mounted' }).catch(() => {});
} catch (e) {
  console.error('OLED Helper flyout mount failed:', e);
  invoke('log_frontend', { level: 'error', message: `[flyout] mount failed: ${e?.message || e}` }).catch(() => {});
}
