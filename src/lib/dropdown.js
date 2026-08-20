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
  menuEl?.querySelector('.dd-menu-item.active')?.scrollIntoView({ block: 'center', behavior: 'instant' });
  return () => {
    document.removeEventListener('pointerdown', onPointerDown);
    document.removeEventListener('keydown', onKeydown);
  };
}

/**
 * Creates reusable keyboard handlers for a custom listbox dropdown.
 * Resolution, refresh rate, and favorites intentionally do not use this helper.
 */
export function createDropdownKeyboardHandlers({
  getOptions,
  getSelectedIndex,
  getHighlightIndex,
  setHighlightIndex,
  isOpen,
  toggle,
  close,
  select,
  canSelect = () => true,
  getMenu,
  getTrigger,
}) {
  function scrollHighlighted() {
    requestAnimationFrame(() => {
      const index = getHighlightIndex();
      const option = getMenu()?.querySelector(`[data-dropdown-option-index="${index}"]`);
      option?.scrollIntoView({ block: 'nearest' });
    });
  }

  function moveHighlight(direction) {
    const options = getOptions();
    if (options.length === 0) return;
    const current = getHighlightIndex() >= 0
      ? getHighlightIndex()
      : getSelectedIndex();
    const next = current < 0
      ? (direction > 0 ? 0 : options.length - 1)
      : (current + direction + options.length) % options.length;
    setHighlightIndex(next);
    scrollHighlighted();
  }

  function onTriggerKeydown(event) {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const direction = event.key === 'ArrowDown' ? 1 : -1;
      if (isOpen()) moveHighlight(direction);
      else {
        const options = getOptions();
        if (options.length === 0 || !canSelect()) return;
        const current = getSelectedIndex();
        const next = current < 0
          ? (direction > 0 ? 0 : options.length - 1)
          : (current + direction + options.length) % options.length;
        select(next);
      }
      return;
    }

    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      if (isOpen()) select(getHighlightIndex());
      else {
        setHighlightIndex(getSelectedIndex());
        toggle();
        requestAnimationFrame(() => {
          if (isOpen()) getMenu()?.focus();
        });
      }
      return;
    }

    if (event.key === 'Escape' && isOpen()) {
      event.preventDefault();
      close();
      getTrigger()?.focus();
    }
  }

  function onMenuKeydown(event) {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      moveHighlight(event.key === 'ArrowDown' ? 1 : -1);
      return;
    }

    if (event.key === 'Home' || event.key === 'End') {
      event.preventDefault();
      const options = getOptions();
      if (options.length > 0) {
        setHighlightIndex(event.key === 'Home' ? 0 : options.length - 1);
        scrollHighlighted();
      }
      return;
    }

    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      select(getHighlightIndex());
      return;
    }

    if (event.key === 'Escape') {
      event.preventDefault();
      close();
      getTrigger()?.focus();
    }
  }

  function onItemFocus(index) {
    setHighlightIndex(index);
  }

  function onItemKeydown(event) {
    event.stopPropagation();
    onMenuKeydown(event);
  }

  return { onTriggerKeydown, onMenuKeydown, onItemFocus, onItemKeydown };
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
