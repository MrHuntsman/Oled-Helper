<script>
  // Inlined data URI, see App.svelte for why icons never hit the network.
  import tabAboutIcon from './icons/tab_about.png?inline';
  import TabHeading from './TabHeading.svelte';
  import { onMount } from 'svelte';
  import { getVersion } from '@tauri-apps/api/app';
  import { invoke } from '@tauri-apps/api/core';
  import { updateState, checkForUpdates } from './stores.svelte.js';

  let appVersion = $state('');
  // Open the link in the user's default browser instead of navigating the
  // WebView to it (the backend opens it via ShellExecuteW).
  function openGitHub(event) {
    event.preventDefault();
    invoke('open_url', { url: 'https://github.com/MrHuntsman/Oled-Helper-v2' }).catch(() => {});
  }
  onMount(() => {
    getVersion().then((v) => { if (typeof v === 'string' && v) appVersion = v; }).catch(() => {});
  });
</script>

<div class="tab-body">
  <TabHeading icon={tabAboutIcon} title="About" />
  <div class="separator"></div>
  <p style="color: var(--text-weak);">OLED Helper is a Windows desktop utility for OLED display calibration and system control.</p>
  <p style="color: var(--text-weak); margin-top: 8px;">v{appVersion || '0.1.1'} · <a href="https://github.com/MrHuntsman/Oled-Helper-v2" tabindex="0" rel="noopener" style="color: var(--text-weak);" onclick={openGitHub}>View on GitHub</a></p>

  <div class="update-check-row" class:has-update={updateState.status === 'available'} class:has-error={updateState.status === 'error'}>
    {#if updateState.status === 'available'}
      <button
        type="button"
        class="modal-btn update-modal-install"
        onclick={() => { updateState.modalOpen = true; }}>
        Update available: v{updateState.version}
      </button>
    {:else if updateState.status === 'checking'}
      <span>Checking for updates…</span>
    {:else if updateState.status === 'uptodate'}
      <span>You're up to date.</span>
    {:else if updateState.status === 'error'}
      <span class="update-check-error">Update check failed: {updateState.error || 'unknown error'}</span>
      <button
        type="button"
        class="modal-btn"
        onclick={() => checkForUpdates(true)}>
        Retry
      </button>
    {:else}
      <button
        type="button"
        class="modal-btn"
        onclick={() => checkForUpdates(true)}>
        Check for updates
      </button>
    {/if}
  </div>
</div>
