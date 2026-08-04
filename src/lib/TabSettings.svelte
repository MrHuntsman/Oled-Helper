<script>
  import { isEnabled, enable, disable } from '@tauri-apps/plugin-autostart';
  import { onMount } from 'svelte';
  import { traySettings, setTrayEnabled, configLoaded, app, taskbarDimmer, setHoverDetection, setPollIntervalMs } from './stores.svelte.js';

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

  <div class="settings-row settings-row-2line">
    <div class="settings-row-text">
      <span class="settings-row-label">Hover Detection</span>
    </div>
    <div class="hover-detect-radios" role="radiogroup" aria-label="Hover detection method">
      <button type="button" class="hover-detect-radio-row" class:active={taskbarDimmer.hoverDetection === 'event'}
        role="radio" aria-checked={taskbarDimmer.hoverDetection === 'event'}
        onclick={() => setHoverDetection('event')}>
        <span class="hover-detect-radio-dot" aria-hidden="true"></span>
        <span class="hover-detect-radio-text">
          <span class="hover-detect-radio-title">Event-driven</span>
          <span class="hover-detect-radio-desc">Mouse hook — instant response, low CPU. Auto-falls back to polling when an elevated window is in the foreground.</span>
        </span>
      </button>
      <button type="button" class="hover-detect-radio-row" class:active={taskbarDimmer.hoverDetection === 'poll'}
        role="radio" aria-checked={taskbarDimmer.hoverDetection === 'poll'}
        onclick={() => setHoverDetection('poll')}>
        <span class="hover-detect-radio-dot" aria-hidden="true"></span>
        <span class="hover-detect-radio-text">
          <span class="hover-detect-radio-title">Polling</span>
          <span class="hover-detect-radio-desc">Checks cursor position on a timer — always works, slightly higher CPU usage.</span>
          <span class="hover-detect-poll-rate" role="presentation" onclick={(e) => e.stopPropagation()}>
            <span class="hover-detect-poll-rate-label">Poll interval</span>
            <input type="range" min="50" max="1000" step="50"
              value={taskbarDimmer.pollIntervalMs}
              oninput={(e) => setPollIntervalMs(Number(e.currentTarget.value))}
              style={`--fill: ${((taskbarDimmer.pollIntervalMs - 50) / (1000 - 50)) * 100}%`} />
            <span class="hover-detect-poll-rate-val">{taskbarDimmer.pollIntervalMs} ms</span>
          </span>
        </span>
      </button>
    </div>
  </div>

  <div class="settings-footer">
    <button class="restore-defaults-btn" onclick={() => app.activeTab = 'debug'}>
      <img class="top-btn-icon" src="/icons/tab_debug.png" alt="" /> Debug
    </button>
  </div>
</div>
