import { mount } from 'svelte'
import './app.css'
import './tray-menu.css'
import TrayMenu from './lib/TrayMenu.svelte'

// Entry point for the custom CSS tray popup window (tray-menu.html).
document.title = 'OLED Helper';

// The tray popup is a small menu surface with no text selection, links, or
// custom context menus, so the WebView2 native right-click menu would only
// get in the way. Block it before Svelte mounts (mirrors the main window's
// contextmenu suppression in main.js).
document.addEventListener('contextmenu', (event) => event.preventDefault());

const target = document.getElementById('tray-menu-app');
mount(TrayMenu, { target });
