<script>
  import { updateState, installUpdate } from './stores.svelte.js';

  let { onclose = () => {} } = $props();
  let installError = $state('');
  let modalEl = $state(null);
  // Element that had focus when the modal opened, restored on close so
  // keyboard and screen-reader users land back where they left off.
  let lastFocused = null;

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

  // Trap Tab inside the dialog and close on Escape, mirroring ConfirmModal's
  // behavior, so focus can never escape to the page behind the backdrop.
  function onModalKeydown(e) {
    if (e.key === 'Escape') {
      e.preventDefault();
      if (!updateState.installing) onclose();
      return;
    }
    if (e.key === 'Tab') {
      const buttons = modalEl ? [...modalEl.querySelectorAll('button:not(:disabled)')] : [];
      if (buttons.length === 0) return;
      const first = buttons[0];
      const last = buttons[buttons.length - 1];
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    }
  }

  // Move focus to the safe action (Later) when the dialog opens, and return
  // it to the element that opened the modal when it closes.
  $effect(() => {
    lastFocused = document.activeElement;
    const focusId = setTimeout(() => modalEl?.querySelector('.modal-actions .modal-btn-cancel')?.focus(), 0);
    return () => {
      clearTimeout(focusId);
      if (lastFocused && typeof lastFocused.focus === 'function') lastFocused.focus();
    };
  });
</script>

<div class="modal-backdrop" onclick={(e) => { if (e.target === e.currentTarget && !updateState.installing) onclose(); }}>
  <div class="modal update-modal" role="dialog" aria-modal="true" aria-label="App update" tabindex="-1" bind:this={modalEl} onkeydown={onModalKeydown}>
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
