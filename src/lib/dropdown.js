// Shared helpers for custom dropdown and context-menu dismiss-on-outside-click
// behaviour. Both the resolution and refresh-rate dropdowns and their context
// menus use identical DOM listener patterns; only the element refs and close
// functions differ.

/**
 * Attaches document-level listeners to close a custom dropdown when the user
 * clicks outside it or presses Escape. Returns a cleanup function.
 *
 * Call from a Svelte $effect, passing the reactive element refs.
 */
export function dropdownCloseEffect(menuEl, buttonEl, ctxMenuEl, closeFn, focusEl) {
  function onPointerDown(e) {
    if (menuEl?.contains(e.target) || buttonEl?.contains(e.target) || ctxMenuEl?.contains(e.target)) return;
    closeFn();
  }
  function onKeydown(e) {
    if (e.key === 'Escape') { closeFn(); focusEl?.focus(); }
  }
  document.addEventListener('pointerdown', onPointerDown);
  document.addEventListener('keydown', onKeydown);
  // Centre on the selected option like a native <select> popup,
  // with behaviour:'instant' (no animation) to match the OS snap.
  menuEl?.querySelector('.hz-menu-item.active')?.scrollIntoView({ block: 'center', behavior: 'instant' });
  return () => {
    document.removeEventListener('pointerdown', onPointerDown);
    document.removeEventListener('keydown', onKeydown);
  };
}

/**
 * Attaches document-level listeners to close a context menu when the user
 * clicks outside it, right-clicks, or presses Escape. Returns a cleanup
 * function.
 *
 * Call from a Svelte $effect. The bounds correction and initial focus are
 * handled inline (they read/write $state variables with different names).
 */
export function ctxMenuCloseEffect(ctxMenuEl, closeFn) {
  function onDocClick() { closeFn(); }
  function onDocKey(e) { if (e.key === 'Escape') closeFn(); }
  // mousedown (not click) also fires for scrollbar interactions,
  // so clicking the dropdown list's scrollbar dismisses the menu.
  function onDocPointerDown(e) {
    if (ctxMenuEl?.contains(e.target)) return;
    closeFn();
  }
  const id = setTimeout(() => {
    document.addEventListener('click', onDocClick);
    document.addEventListener('contextmenu', onDocClick);
    document.addEventListener('mousedown', onDocPointerDown);
    document.addEventListener('keydown', onDocKey);
  }, 0);
  return () => {
    clearTimeout(id);
    document.removeEventListener('click', onDocClick);
    document.removeEventListener('contextmenu', onDocClick);
    document.removeEventListener('mousedown', onDocPointerDown);
    document.removeEventListener('keydown', onDocKey);
  };
}
