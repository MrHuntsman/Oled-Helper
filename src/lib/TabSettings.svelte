<script>
  // Inlined data URIs — see App.svelte for why icons never hit the network.
  import tabSystemIcon from './icons/tab_system.png?inline';
  import tabDebugIcon from './icons/tab_debug.png?inline';
  import { isEnabled, enable, disable } from '@tauri-apps/plugin-autostart';
  import { onMount } from 'svelte';
  import { traySettings, setTrayEnabled, configLoaded, app, restoreAllDefaults } from './stores.svelte.js';

  let startOnBoot = $state(false);
  let ready = $state(false);

  // Confirmation for the destructive "reset everything" action.
  let resetAllConfirmOpen = $state(false);
  let resetAllConfirmEl = $state(null);
  let resetAllBtnEl = $state(null);
  let resetAllError = $state('');

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
    // so restoreAllDefaults() can't reset it — disable it here too.
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
      resetAllError = 'Reset failed — please try again.';
    }
    resetAllBtnEl?.focus();
  }
</script>

<div class="tab-body">
  <div class="tab-heading">
    <img class="tab-icon" src={tabSystemIcon} alt="" />
    <h2>Settings</h2>
    <div class="tab-heading-spacer"></div>
    <button class="restore-defaults-btn" onclick={restoreDefaults} title="Resets only the Start on Boot and System Tray options">↻ Restore Tab Defaults</button>
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

  <button class="settings-row reset-all-row" bind:this={resetAllBtnEl} onclick={openResetAllConfirm}>
    <div class="settings-row-text">
      <span class="settings-row-label">Reset All Settings</span>
      <span class="settings-row-desc">Restore every setting to its default value.</span>
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

  {#if resetAllConfirmOpen}
    <div class="modal-backdrop" role="presentation">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="reset-all-title" aria-describedby="reset-all-desc" tabindex="-1" onkeydown={onResetAllKeydown} bind:this={resetAllConfirmEl}>
        <div class="modal-title" id="reset-all-title">Reset all settings?</div>
        <p class="modal-desc" id="reset-all-desc">
          This restores every setting to its default and cannot be undone.
        </p>
        <div class="modal-actions">
          <button class="modal-btn modal-btn-cancel" onclick={closeResetAllConfirm}>Cancel</button>
          <button class="modal-btn modal-btn-danger" onclick={confirmResetAll}>Reset all</button>
        </div>
      </div>
    </div>
  {/if}
</div>
