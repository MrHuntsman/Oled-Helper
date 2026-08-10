<script>
  import { updateState, installUpdate } from './stores.svelte.js';

  let { onclose = () => {} } = $props();
  let installError = $state('');

  async function onInstall() {
    if (updateState.installing) return;
    installError = '';
    const ok = await installUpdate();
    if (!ok) {
      // installUpdate already surfaced the failure in updateState.error.
      installError = updateState.error || 'Update failed. Please try again.';
    }
    // On success the backend hands off to the staged exe and shuts the app
    // down; the new version relaunches on its own.
  }

  function formatDate(iso) {
    if (!iso) return '';
    try { return new Date(iso).toLocaleDateString(); } catch { return ''; }
  }
</script>

<div class="modal-backdrop" onclick={(e) => { if (e.target === e.currentTarget && !updateState.installing) onclose(); }}>
  <div class="modal update-modal" role="dialog" aria-modal="true" aria-label="App update">
    <div class="update-modal-header">
      <div>
        <div class="modal-title">Update available</div>
        <div class="update-modal-version">
          v{updateState.version || '?'} <span class="update-modal-date">{formatDate(updateState.date)}</span>
        </div>
      </div>
      <button type="button" class="modal-btn modal-btn-cancel update-modal-close" aria-label="Close" disabled={updateState.installing} onclick={onclose}>&times;</button>
    </div>

    {#if updateState.notes}
      <div class="update-modal-notes">
        {#each updateState.notes.split(/\n{2,}/) as para}
          <p>{para}</p>
        {/each}
      </div>
    {:else}
      <p class="modal-desc">A new version of OLED Helper is ready to install.</p>
    {/if}

    {#if updateState.progress !== null && updateState.progress < 1}
      <div class="update-progress">
        <div class="update-progress-bar"><div class="update-progress-fill" style="width:{Math.round(updateState.progress * 100)}%"></div></div>
        <span class="update-progress-label">{Math.round(updateState.progress * 100)}%</span>
      </div>
    {/if}

    {#if installError}
      <p class="update-error">{installError}</p>
    {/if}

    <div class="modal-actions">
      <button type="button" class="modal-btn modal-btn-cancel" disabled={updateState.installing} onclick={onclose}>Later</button>
      <button type="button" class="modal-btn update-modal-install" disabled={updateState.installing} onclick={onInstall}>
        {updateState.installing ? (updateState.progress !== null && updateState.progress < 1 ? 'Downloading…' : 'Installing…') : 'Install & Restart'}
      </button>
    </div>
  </div>
</div>
