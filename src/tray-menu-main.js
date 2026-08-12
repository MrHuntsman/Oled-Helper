import { mount } from 'svelte'
import './app.css'
import './tray-menu.css'
import TrayMenu from './lib/TrayMenu.svelte'

// Entry point for the custom CSS tray popup window (tray-menu.html).
document.title = 'OLED Helper';

const target = document.getElementById('tray-menu-app');
mount(TrayMenu, { target });
