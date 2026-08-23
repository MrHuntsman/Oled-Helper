<script>
  // Reusable confirmation dialog, mounted by the parent with `{#if open}`.
  // Close on outside click, Escape, or the Cancel button; Tab is trapped
  // between the two actions so focus never escapes to the page behind the
  // backdrop. Mirrors the inline confirm modals in TabSettings and
  // TabWindowsFont, so every tab's Restore Defaults asks before resetting.
  let { title, desc, confirmLabel = 'Restore', onConfirm, onClose } = $props();
  let modalEl = $state(null);

  function close() {
    onClose?.();
  }

  // Confirm modal: close on outside click, Escape, and trap Tab between the
  // two buttons so focus never escapes to elements behind the backdrop.
  function onModalKeydown(e) {
    if (e.key === 'Escape') {
      close();
      return;
    }
    if (e.key === 'Tab') {
      const cancel = modalEl?.querySelector('.modal-btn-cancel');
      const confirm = modalEl?.querySelector('.modal-btn-danger');
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
    function onDocPointerDown(e) {
      if (modalEl?.contains(e.target)) return;
      close();
    }
    function onDocKey(e) {
      if (e.key === 'Escape') close();
    }
    // Focus the Cancel button when the dialog opens so Enter/Escape behave
    // predictably and screen readers announce it as a dialog.
    const focusId = setTimeout(() => modalEl?.querySelector('.modal-btn-cancel')?.focus(), 0);
    document.addEventListener('pointerdown', onDocPointerDown);
    document.addEventListener('keydown', onDocKey);
    return () => {
      clearTimeout(focusId);
      document.removeEventListener('pointerdown', onDocPointerDown);
      document.removeEventListener('keydown', onDocKey);
    };
  });
</script>

<div class="modal-backdrop" role="presentation">
  <div class="modal" role="dialog" aria-modal="true" aria-labelledby="confirm-modal-title" aria-describedby="confirm-modal-desc" tabindex="-1" onkeydown={onModalKeydown} bind:this={modalEl}>
    <div class="modal-title" id="confirm-modal-title">{title}</div>
    <p class="modal-desc" id="confirm-modal-desc">{desc}</p>
    <div class="modal-actions">
      <button class="modal-btn modal-btn-cancel" onclick={close}>Cancel</button>
      <button class="modal-btn modal-btn-danger" onclick={() => { close(); onConfirm?.(); }}>{confirmLabel}</button>
    </div>
  </div>
</div>
