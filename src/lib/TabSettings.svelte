<script>
  // Inlined data URIs, see App.svelte for why icons never hit the network.
  import tabSystemIcon from './icons/tab_system.png?inline';
  import tabDebugIcon from './icons/tab_debug.png?inline';
  import { isEnabled, enable, disable } from '@tauri-apps/plugin-autostart';
  import { onMount } from 'svelte';
  import { app, appSettings, restoreAllDefaults, taskbarDimmer, setHoverDetection, setPollIntervalMs, toggleKeepTrayMenuWarm, TASKBAR_DIMMER_DEFAULTS } from './stores.svelte.js';
  import ConfirmModal from './ConfirmModal.svelte';

  let startOnBoot = $state(false);
  let ready = $state(false);

  // Confirmation for the destructive "reset everything" action.
  let resetAllConfirmOpen = $state(false);
  let resetAllConfirmEl = $state(null);
  let resetAllBtnEl = $state(null);
  let resetAllError = $state('');

  // Confirmation for the tab-level "Restore Tab Defaults" action (Start on
  // Boot + Detection Method), separate from the destructive "Reset all".
  let restoreConfirmOpen = $state(false);

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
    startOnBoot = next; // optimistic; reverted below on failure
    try {
      if (next) await enable();
      else await disable();
    } catch (e) {
      console.warn('[settings] toggle autostart failed', e);
      startOnBoot = !next;
    }
  }

  async function toggleSkipFavoriteConfirmation() {
    appSettings.skipFavoriteConfirmation = !appSettings.skipFavoriteConfirmation;
    try {
      await import('./stores.svelte.js').then(({ saveConfig }) => saveConfig());
    } catch (e) {
      appSettings.skipFavoriteConfirmation = !appSettings.skipFavoriteConfirmation;
      console.warn('[settings] favorite confirmation setting failed', e);
    }
  }

  async function toggleTrayMenuWarm() {
    await toggleKeepTrayMenuWarm();
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
    // Detection Method (Taskbar Dimmer) is global and lives in Settings, so
    // reset it to defaults here too. setPollIntervalMs persists the config
    // (debounced), so the reset survives a restart.
    setHoverDetection(TASKBAR_DIMMER_DEFAULTS.hoverDetection);
    setPollIntervalMs(TASKBAR_DIMMER_DEFAULTS.pollIntervalMs);
    await import('./stores.svelte.js').then(({ setKeepTrayMenuWarm }) => setKeepTrayMenuWarm(false));
  }

  function openResetAllConfirm() {
    resetAllConfirmOpen = true;
  }
  function closeResetAllConfirm() {
    resetAllConfirmOpen = false;
  }

  // Confirm modal: close on outside click, Escape, and trap Tab between
  // the two buttons so focus never escapes to elements behind the backdrop.
  function onResetAllKeydown(e) {
    if (e.key === 'Escape') {
      closeResetAllConfirm();
      resetAllBtnEl?.focus();
      return;
    }
    if (e.key === 'Tab') {
      const cancel = resetAllConfirmEl?.querySelector('.modal-btn-cancel');
      const confirm = resetAllConfirmEl?.querySelector('.modal-btn-danger');
      if (!cancel || !confirm) return;
      if (e.shiftKey && document.activeElement === cancel) {
        e.preventDefault();
        confirm.focus();
      } else if (!e.shiftKey && document.activeElement === confirm) {
        e.preventDefault();
        cancel.focus();
      }
    }
  }
  $effect(() => {
    if (!resetAllConfirmOpen) return;
    function onDocPointerDown(e) {
      if (resetAllConfirmEl?.contains(e.target) || resetAllBtnEl?.contains(e.target)) return;
      closeResetAllConfirm();
    }
    function onDocKey(e) {
      if (e.key === 'Escape') {
        closeResetAllConfirm();
        resetAllBtnEl?.focus();
      }
    }
    // Focus the Cancel button when the dialog opens so Enter/Escape behave
    // predictably and screen readers announce it as a dialog.
    const focusId = setTimeout(() => resetAllConfirmEl?.querySelector('.modal-btn-cancel')?.focus(), 0);
    document.addEventListener('pointerdown', onDocPointerDown);
    document.addEventListener('keydown', onDocKey);
    return () => {
      clearTimeout(focusId);
      document.removeEventListener('pointerdown', onDocPointerDown);
      document.removeEventListener('keydown', onDocKey);
    };
  });

  async function confirmResetAll() {
    closeResetAllConfirm();
    // Start on Boot is handled by the autostart plugin, not the config file,
    // so restoreAllDefaults() can't reset it, disable it here too.
    if (startOnBoot) {
      try {
        await disable();
        startOnBoot = false;
      } catch (e) {
        console.warn('[settings] reset all could not disable autostart', e);
      }
    }
    try {
      resetAllError = '';
      await restoreAllDefaults();
    } catch (e) {
      console.warn('[settings] reset all failed', e);
      resetAllError = 'Reset failed. Please try again.';
    }
    resetAllBtnEl?.focus();
  }
</script>

<div class="tab-body">
  <div class="tab-heading">
    <img class="tab-icon" src={tabSystemIcon} alt="" />
    <h2>Settings</h2>
    <div class="tab-heading-spacer"></div>
    <button class="restore-defaults-btn" onclick={() => restoreConfirmOpen = true} title="Resets the Start on Boot, Always open as administrator, trusted favorites, tray menu behavior and Detection Method options">↻ Restore Tab Defaults</button>
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

  <button class="settings-row" onclick={toggleTrayMenuWarm}>
    <div class="settings-row-text">
      <span class="settings-row-label">Keep tray menu warm</span>
      <span class="settings-row-desc">Keep the tray menu ready for a faster first open, at the cost of extra WebView2 GPU memory.</span>
    </div>
    <div class="pill-wrap">
      <span class={`pill${appSettings.keepTrayMenuWarm ? '' : ' off'}`} aria-hidden="true"></span>
    </div>
  </button>

  <button class="settings-row" onclick={toggleSkipFavoriteConfirmation}>
    <div class="settings-row-text">
      <span class="settings-row-label">Skip favorites confirmations</span>
      <span class="settings-row-desc">After the first confirmation it won't ask again on the same monitor.</span>
    </div>
    <div class="pill-wrap">
      <span class={`pill${appSettings.skipFavoriteConfirmation ? '' : ' off'}`} aria-hidden="true"></span>
    </div>
  </button>

  <div class="settings-row hover-detect-settings-row">
    <div class="hover-detect-left">
      <span class="settings-row-label hover-detect-label">Detection Method
        <span class="info-tip-wrap">
          <button type="button" class="info-tip-btn" aria-label="About the detection method" tabindex="0">Info</button>
          <span class="info-tip info-tip-right">
            <span>How the app detects the cursor, fullscreen mode, and taskbar changes for the Taskbar Dimmer.<br><br>Event-driven: instant response, falls back to polling for elevated (admin) windows.<br><br>Polling: checks on a timer, use it if Event-driven has problems.</span>
          </span>
        </span>
      </span>
    </div>
    <div class="hover-detect-controls">
      <div class="hover-detect-switch" class:poll={taskbarDimmer.hoverDetection === 'poll'} role="radiogroup" aria-label="Taskbar Dimmer detection method">
        <button type="button" class="hover-detect-opt" class:active={taskbarDimmer.hoverDetection === 'event'}
          role="radio" aria-checked={taskbarDimmer.hoverDetection === 'event'}
          disabled={!taskbarDimmer.available}
          title="Recommended. Instant response. Falls back to polling for elevated (admin) windows."
          onclick={() => setHoverDetection('event')}>Event-driven</button>
        <button type="button" class="hover-detect-opt" class:active={taskbarDimmer.hoverDetection === 'poll'}
          role="radio" aria-checked={taskbarDimmer.hoverDetection === 'poll'}
          disabled={!taskbarDimmer.available}
          title="Checks on a timer. Use it if Event-driven has problems, probably negligible CPU impact."
          onclick={() => setHoverDetection('poll')}>Polling</button>
      </div>
      {#if taskbarDimmer.hoverDetection === 'poll'}
        <div class="hover-detect-poll-rate">
          <span class="hover-detect-poll-rate-label">Poll interval</span>
          <input type="range" min="16" max="200" step="1"
            value={taskbarDimmer.pollIntervalMs}
            disabled={!taskbarDimmer.available}
            aria-label="Poll interval"
            oninput={(e) => setPollIntervalMs(Number(e.currentTarget.value))}
            style={`--fill: ${((taskbarDimmer.pollIntervalMs - 16) / (200 - 16)) * 100}%`} />
          <span class="hover-detect-poll-rate-val">{taskbarDimmer.pollIntervalMs} ms</span>
        </div>
      {/if}
    </div>
  </div>

  <button class="settings-row reset-all-row" bind:this={resetAllBtnEl} onclick={openResetAllConfirm}>
    <div class="settings-row-text">
      <span class="settings-row-label">Reset All Settings</span>
      <span class="settings-row-desc">Restore OLED Helper settings; Windows HDR and display brightness are not changed.</span>
    </div>
    <span class="reset-all-chevron">↻ Reset all</span>
  </button>
  {#if resetAllError}
    <p class="reset-all-error" role="status">{resetAllError}</p>
  {/if}

  <div class="settings-footer">
    <button class="restore-defaults-btn" onclick={() => app.activeTab = 'system'}>
      <img class="top-btn-icon" src={tabSystemIcon} alt="" /> System Info
    </button>
    <button class="restore-defaults-btn" onclick={() => app.activeTab = 'debug'}>
      <img class="top-btn-icon" src={tabDebugIcon} alt="" /> Debug
    </button>
  </div>

  {#if restoreConfirmOpen}
    <ConfirmModal
      title="Restore Settings defaults?"
      desc="This resets the Start on Boot, trusted favorites, tray menu behavior and Detection Method options to their defaults."
      confirmLabel="Restore"
      onConfirm={restoreDefaults}
      onClose={() => restoreConfirmOpen = false} />
  {/if}

  {#if resetAllConfirmOpen}
    <div class="modal-backdrop" role="presentation">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="reset-all-title" aria-describedby="reset-all-desc" tabindex="-1" onkeydown={onResetAllKeydown} bind:this={resetAllConfirmEl}>
        <div class="modal-title" id="reset-all-title">Reset all settings?</div>
        <p class="modal-desc" id="reset-all-desc">
          This restores OLED Helper settings to their defaults. Windows HDR, SDR brightness, Auto HDR, and RTX HDR settings are not changed.
        </p>
        <div class="modal-actions">
          <button class="modal-btn modal-btn-cancel" onclick={closeResetAllConfirm}>Cancel</button>
          <button class="modal-btn modal-btn-danger" onclick={confirmResetAll}>Reset all</button>
        </div>
      </div>
    </div>
  {/if}
</div>
