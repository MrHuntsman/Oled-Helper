import { mount } from 'svelte'
import './app.css'
import './flyout.css'
import TrayMenu from './lib/TrayMenu.svelte'

const target = document.getElementById('tray-menu-app')
if (target) mount(TrayMenu, { target })
