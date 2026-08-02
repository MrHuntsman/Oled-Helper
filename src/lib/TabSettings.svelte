<script>
  import { isEnabled, enable, disable } from '@tauri-apps/plugin-autostart';
  import { onMount } from 'svelte';
  import { traySettings, setTrayEnabled, configLoaded, app } from './stores.svelte.js';

  let startOnBoot = $state(false);
  let ready = $state(false);

  onMount(async () => {
    try {
      startOnBoot = await isEnabled();
    } catch (e) {
      console.warn('[settings] isEnabled failed', e);
    } finally {
      ready = true;
    }
  });

  async function toggleStartOnBoot() {
    const next = !startOnBoot;
    startOnBoot = next; // optimistic — reverted below on failure
    try {
      if (next) await enable();
      else await disable();
    } catch (e) {
      console.warn('[settings] toggle autostart failed', e);
      startOnBoot = !next;
    }
  }

  async function toggleTray() {
    await setTrayEnabled(!traySettings.enabled);
  }

  async function restoreDefaults() {
    if (startOnBoot) {
      try {
        await disable();
        startOnBoot = false;
      } catch (e) {
        console.warn('[settings] restore defaults could not disable autostart', e);
      }
    }
    await setTrayEnabled(true);
  }
</script>

<div class="tab-body">
  <div class="tab-heading">
    <img class="tab-icon" src="/icons/tab_system.png" alt="" />
    <h2>Settings</h2>
    <div class="tab-heading-spacer"></div>
    <button class="restore-defaults-btn" onclick={restoreDefaults}>↻ Restore defaults</button>
  </div>
  <div class="separator"></div>

  <button class="settings-row" disabled={!ready} onclick={toggleStartOnBoot}>
    <div class="settings-row-text">
      <span class="settings-row-label">Start on Boot</span>
      <span class="settings-row-desc">Launch OLED Helper automatically when Windows starts.</span>
    </div>
    <div class="pill-wrap">
      {#if ready}
        <span class={`pill${startOnBoot ? '' : ' off'}`} aria-hidden="true"></span>
      {/if}
    </div>
  </button>

  <button class="settings-row" onclick={toggleTray}>
    <div class="settings-row-text">
      <span class="settings-row-label">System Tray</span>
      <span class="settings-row-desc">Keep the app available in the system tray and close the window to the tray.</span>
    </div>
    <div class="pill-wrap">
      {#if configLoaded.v}
        <span class={`pill${traySettings.enabled ? '' : ' off'}`} aria-hidden="true"></span>
      {/if}
    </div>
  </button>

  <div class="settings-footer">
    <button class="restore-defaults-btn" onclick={() => app.activeTab = 'debug'}>
      <img class="top-btn-icon" src="/icons/tab_debug.png" alt="" /> Debug
    </button>
  </div>
</div>
