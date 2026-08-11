<script>
  import { onMount, tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import appIcon from './icons/app-icon.png?inline';

  let snapshot = $state({ open: false, data: null });
  let activeSubmenu = $state(null);
  let submenuCloseTimer = null;
  let closeTimer = null;
  let animating = $state(false);
  let closing = $state(false);
  let submenuRevealed = $state(true);
  // Keep the flyout directly adjacent to its parent row. A visible gap can
  // trigger pointerleave before the pointer reaches the submenu, especially
  // when the row and flyout are vertically offset.
  const submenuGap = 0;
  let receivedOpenEvent = false;
  // Actual rendered card height, measured once the card is mounted. The flip
  // math in cardStyle uses it so the card's far edge lands exactly at the
  // cursor instead of an arbitrary max-height estimate.
  let cardHeight = $state(0);
  let viewport = $state({ width: 0, height: 0, dpr: 1 });
  const data = $derived(snapshot.data);
  const currentMode = $derived(data ? `${data.width}×${data.height} @ ${data.refresh_hz} Hz` : 'No display detected');
  // Track the card + submenu layout while the menu is open so the backend can
  // keep the native hit-test transparent outside those bounds. Report right
  // away (the cushion also covers the entrance animation travel) and again
  // once the entrance animation and submenu positioning have settled.
  $effect(() => {
    if (!snapshot.open) return;
    void activeSubmenu;
    // The removal confirm panel adds height to the card; re-measure so the
    // native popup grows (and re-flushes against the taskbar) in the same
    // frame instead of clipping the new section.
    void pendingRemove;
    measureCard();
    reportHitRects();
    const timer = setTimeout(() => {
      measureCard();
      if (activeSubmenu) positionSubmenu(activeSubmenu);
      reportHitRects();
    }, 170);
    return () => clearTimeout(timer);
  });
  // Report the rendered card height so the backend can place the popup with
  // its taskbar-facing edge exactly flush (the backend's initial placement
  // assumes a taller default card, which leaves a gap for shorter menus such
  // as 0-1 favorites). Only send when the height actually changed, a submenu
  // opening does not alter the card height and must not re-trigger placement.
  let lastReportedCardHeight = 0;
  function measureCard() {
    const card = document.querySelector('.tray-menu-card');
    if (!card) return;
    cardHeight = card.offsetHeight;
    if (cardHeight > 0 && Math.abs(cardHeight - lastReportedCardHeight) >= 1) {
      lastReportedCardHeight = cardHeight;
      invoke('tray_menu_set_card_height', { heightCss: cardHeight }).catch(() => {});
    }
  }
  function updateViewport() {
    viewport = {
      width: window.innerWidth,
      height: window.innerHeight,
      dpr: window.devicePixelRatio || 1,
    };
  }
  function logGeometry(source) {
    if (!data) return;
    const card = document.querySelector('.tray-menu-card');
    const rect = card?.getBoundingClientRect();
    const cardText = rect
      ? `card_css=(${rect.left.toFixed(1)},${rect.top.toFixed(1)},${rect.width.toFixed(1)}x${rect.height.toFixed(1)})`
      : 'card_css=missing';
    invoke('log_frontend', {
      level: 'debug',
      message: `[tray] frontend geometry source=${source} viewport_css=${viewport.width}x${viewport.height} dpr=${viewport.dpr.toFixed(2)} target_frac=(${data.cursor_x.toFixed(3)},${data.cursor_y.toFixed(3)}) work_frac=(${data.work_left.toFixed(3)},${data.work_top.toFixed(3)},${data.work_right.toFixed(3)},${data.work_bottom.toFixed(3)}) ${cardText}`,
    }).catch(() => {});
  }
  // The native popup is positioned and sized around the card before it is
  // shown. Keep the card local to that compact viewport instead of creating a
  // transparent surface over the desktop. The native origin includes the
  // 24px hit cushion, so the visible card begins at this local inset.
  const cardGeometry = $derived.by(() => {
    if (!data) return null;
    return {
      left: 24,
      top: 24,
      vw: viewport.width || window.innerWidth,
      vh: viewport.height || window.innerHeight,
    };
  });
  const cardStyle = $derived.by(() => {
    const g = cardGeometry;
    if (!g) return null;
    // Svelte treats an object passed to the generic style attribute as a
    // string value, which becomes "[object Object]" and is ignored by the
    // browser. Return serialized CSS so the inline position actually
    // overrides the shell's default bottom-right flex alignment.
    return `position: absolute; left: ${g.left}px; right: auto; top: ${g.top}px; bottom: auto;`;
  });
  // This starts as the normal right-opening direction. It is updated after a
  // submenu is mounted, using its actual rendered width, instead of comparing
  // the left and right margins of the card. A large empty margin on the left
  // must not force a left flyout when the right side can fit the submenu.
  let submenuDirection = $state('right');
  function updateSubmenuDirection(name) {
    const card = document.querySelector('.tray-menu-card');
    const submenu = document.querySelector(`[data-tray-submenu="${name}"]`);
    if (!card || !submenu) return;

    const cardRect = card.getBoundingClientRect();
    const submenuWidth = submenu.getBoundingClientRect().width;
    const gap = submenuGap;
    const margin = 12;
    const rightSpace = window.innerWidth - cardRect.right - margin;
    const leftSpace = cardRect.left - margin;
    const required = submenuWidth + gap;

    if (rightSpace >= required) {
      submenuDirection = 'right';
    } else if (leftSpace >= required) {
      submenuDirection = 'left';
    } else {
      // If neither side fully fits, use the side with more room and let the
      // submenu's max-width/scrolling behavior handle the remaining pixels.
      submenuDirection = rightSpace >= leftSpace ? 'right' : 'left';
    }
  }

  // Keep this command for layout diagnostics and compatibility with older
  // native builds. The current native window is already content-sized, so it
  // does not apply a region to the WebView2 surface.
  function reportHitRects() {
    const scale = viewport.dpr || window.devicePixelRatio || 1;
    // Retain the cushion in the payload for compatibility with the native
    // command, even though the current backend no longer applies a region.
    const cushion = 24;
    const rects = [...document.querySelectorAll('.tray-menu-card, .tray-menu-submenu')]
      .map((el) => {
        const r = el.getBoundingClientRect();
        return {
          x: Math.round((r.left - cushion) * scale),
          y: Math.round((r.top - cushion) * scale),
          width: Math.round((r.width + cushion * 2) * scale),
          height: Math.round((r.height + cushion * 2) * scale),
        };
      });
    return invoke('tray_menu_set_hit_rects', { rects }).catch(() => {});
  }

  function isCurrentFavorite(favorite) { return data && favorite.width === data.width && favorite.height === data.height && favorite.hz === data.refresh_hz; }
  function isCurrentResolution(resolution) { return data && resolution.width === data.width && resolution.height === data.height; }
  function isCurrentRate(rate) { return data && rate === data.refresh_hz; }
  // ── Favorite stars ────────────────────────────────────────────────────────
  // Mirrors the main app's resolution dropdown: a resolution is favorited at
  // the CURRENT refresh rate (favorites are res@hz combos). Full stars are
  // always visible on saved combos; empty stars appear on hover for the rest.
  function isResolutionFavorited(resolution) {
    return data && data.refresh_hz > 0
      && data.favorites.some((f) => f.width === resolution.width && f.height === resolution.height && f.hz === data.refresh_hz);
  }
  function favKey(width, height, hz) { return `${width}x${height}@${hz}`; }
  // Removal confirmation, compact enough for a flyout: one click on a filled
  // star opens a small in-card confirm panel ("Remove?"), one click on
  // Remove deletes the favorite. Escape or Cancel closes it without removing.
  let pendingRemove = $state(null); // { width, height, hz } awaiting confirm
  function cancelPendingRemove() {
    pendingRemove = null;
  }
  function pendingRemoveKey() {
    return pendingRemove ? favKey(pendingRemove.width, pendingRemove.height, pendingRemove.hz) : null;
  }
  function confirmPendingRemove() {
    const fav = pendingRemove;
    cancelPendingRemove();
    if (!fav) return;
    invoke('tray_menu_remove_favorite', { width: fav.width, height: fav.height, hz: fav.hz }).catch(() => {});
  }
  function requestRemove(fav) {
    cancelPendingRemove();
    // A pending hover-close must not race the panel: once a removal is being
    // confirmed, the submenu it came from stays put until the panel resolves.
    cancelSubmenuClose();
    pendingRemove = fav;
  }
  function onResStar(event, resolution) {
    event.stopPropagation();
    const hz = data.refresh_hz || 0;
    if (hz <= 0) return;
    if (isResolutionFavorited(resolution)) {
      requestRemove({ width: resolution.width, height: resolution.height, hz });
    } else {
      invoke('tray_menu_add_favorite', { width: resolution.width, height: resolution.height, hz }).catch(() => {});
    }
  }
  function onFavStar(event, favorite) {
    event.stopPropagation();
    requestRemove({ width: favorite.width, height: favorite.height, hz: favorite.hz });
  }
  function beginClose() {
    if (!snapshot.open || closing) return;
    cancelSubmenuClose();
    cancelPendingRemove();
    // Keep the open submenu mounted for the exit animation. It hangs off the
    // card's side, and unmounting it in the same frame the close starts makes
    // the menu look like it collapses sideways (the submenu vanishes instantly
    // while the card fades). Letting it ride along with the card means the
    // whole composite fades and moves down as one unit, then unmounts together
    // when the animation finishes.
    closing = true;
    animating = false;
    const shell = document.querySelector('.tray-menu-shell');
    shell?.classList.remove('tray-menu-enter');
    void shell?.offsetWidth;
    clearTimeout(closeTimer);
    // 250 ms: the .12s fade finishes at 120 ms, then we give the WebView
    // compositor ~130 ms to fully settle before the native window is hidden.
    // Hiding while the compositor is still finishing the fade is what renders
    // the sideways artifact (worst while the shell is still processing a tray
    // right-click). Reduced-motion users get no animation, so hide immediately.
    const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    closeTimer = setTimeout(async () => {
      snapshot = { ...snapshot, open: false };
      closing = false;
      await invoke('tray_menu_hide_native').catch(() => {});
    }, reduceMotion ? 0 : 250);
  }
  async function close() {
    beginClose();
    await invoke('tray_menu_close').catch(() => {});
  }
  async function restart() { await invoke('tray_menu_restart').catch(() => {}); }
  async function quit() { await invoke('tray_menu_quit').catch(() => {}); }
  async function selectResolution(resolution) { await invoke('tray_menu_select_resolution', { width: resolution.width, height: resolution.height }).catch(() => {}); }
  async function selectRate(rate) { await invoke('tray_menu_select_refresh_rate', { hz: rate }).catch(() => {}); }
  async function selectFavorite(favorite) { await invoke('tray_menu_select_favorite', { width: favorite.width, height: favorite.height, hz: favorite.hz }).catch(() => {}); }
  function positionSubmenu(name) {
    if (!name) return;
    const submenu = document.querySelector(`[data-tray-submenu="${name}"]`);
    const row = submenu?.parentElement;
    if (!submenu || !row) return;

    const rowRect = row.getBoundingClientRect();
    const card = document.querySelector('.tray-menu-card');
    const cardRect = card?.getBoundingClientRect();
    const menuHeight = cardRect?.height || cardHeight || 430;
    // The submenu is nested inside one row, so its available space must be
    // measured from the card bounds, not from the row. Measuring from the row
    // made a resolution list stop halfway down the card even when the card had
    // plenty of empty space below it.
    const cardTop = cardRect?.top ?? rowRect.top;
    const cardBottom = cardRect?.bottom ?? (cardTop + menuHeight);
    const viewportPadding = 12;
    const fitsCardBounds = cardTop >= viewportPadding
      && cardBottom <= window.innerHeight - viewportPadding;
    const availableAbove = Math.max(1, cardBottom - viewportPadding);
    const availableBelow = Math.max(1, window.innerHeight - cardTop - viewportPadding);
    const growDown = fitsCardBounds || availableBelow >= availableAbove;
    const available = growDown ? availableBelow : availableAbove;
    const submenuHeight = fitsCardBounds ? menuHeight : Math.min(menuHeight, available);
    const topOffset = cardTop - rowRect.top;
    const bottomOffset = rowRect.bottom - cardBottom;

    submenu.style.height = `${submenuHeight}px`;
    submenu.style.maxHeight = `${submenuHeight}px`;
    submenu.style.top = growDown ? `${topOffset}px` : 'auto';
    submenu.style.bottom = growDown ? 'auto' : `${bottomOffset}px`;
  }
  function cancelSubmenuClose() {
    clearTimeout(submenuCloseTimer);
    submenuCloseTimer = null;
  }
  function scheduleSubmenuClose(event, name) {
    // The parent row and its flyout are one interactive region. Returning
    // from the flyout to the active parent row should cancel the close rather
    // than briefly unmounting the submenu under the cursor.
    const row = event?.currentTarget?.closest?.('.tray-menu-row');
    const next = event?.relatedTarget;
    if (next && row?.contains(next)) {
      cancelSubmenuClose();
      return;
    }
    cancelSubmenuClose();
    submenuCloseTimer = setTimeout(() => {
      if (activeSubmenu === name) activeSubmenu = null;
      submenuCloseTimer = null;
    }, 220);
  }
  async function openSubmenu(name) {
    cancelSubmenuClose();
    // Pointer-enter, focus, and click can all request the same submenu during
    // one interaction. Do not restart its reveal sequence when it is already
    // active, otherwise the hidden class and entrance animation briefly reset
    // and make a right-click look like the submenu closed and reopened.
    if (activeSubmenu === name) return;
    // Keep the submenu hidden until its layout has settled. This avoids a
    // clipped first frame while the small native popup is being shown.
    submenuRevealed = false;
    submenuDirection = 'right';
    activeSubmenu = name;
    await tick();
    positionSubmenu(name);
    updateSubmenuDirection(name);
    await tick();
    // Reapply the vertical placement after the direction class has rerendered
    // so the hit region matches the final flyout geometry.
    positionSubmenu(name);
    // Race the layout report against a short grace period so the submenu can
    // never stay hidden if IPC hangs during teardown.
    await Promise.race([
      reportHitRects(),
      new Promise((resolve) => setTimeout(resolve, 150)),
    ]);
    if (activeSubmenu === name) submenuRevealed = true;
  }
  function onMenuFocusOut(event, name) {
    // While the removal confirm panel is open, focus naturally leaves the row
    // (the panel auto-focuses its Cancel button). That must not schedule a
    // hover-close, or the submenu would vanish under the cursor mid-confirm.
    if (pendingRemove) return;
    const next = event.relatedTarget;
    const row = event.currentTarget;
    if (next && row.contains(next)) return;
    scheduleSubmenuClose(event, name);
  }
  function onParentKeydown(event, name) {
    if (event.key !== 'ArrowRight') return;
    event.preventDefault();
    openSubmenu(name);
    requestAnimationFrame(() => {
      document.querySelector(`[data-tray-submenu="${name}"] .tray-menu-subitem`)?.focus();
    });
  }
  function onPointerDown(event) {
    const card = document.querySelector('.tray-menu-card');
    if (!card) return;
    const path = event.composedPath();
    const insideCard = path.includes(card);
    const insideSubmenu = event.target?.closest?.('.tray-menu-submenu');

    // A right-button press on a submenu item should be inert. Letting the
    // browser focus the button during pointerdown can fire the parent row's
    // focusout/pointerenter handlers and replay openSubmenu once, which looks
    // like the submenu closes and immediately reopens. The contextmenu
    // handler suppresses the browser menu itself; this also suppresses the
    // focus transition that causes the one-frame flicker. Keep the guard
    // limited to the submenu so the rest of the card keeps its normal behavior.
    if (insideSubmenu && event.button === 2) {
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    if (insideCard) return;
    close();
  }
  function onContextMenu(event) {
    // The tray menu is an application menu, not a browser document. Suppress
    // WebView2's native page context menu everywhere inside the popup,
    // including submenu rows and its scrollbar. Keep the right-button event
    // from reaching any row-level focus or pointer handler as well.
    event.preventDefault();
    event.stopPropagation();
  }
  let pendingRemoveCancelEl = $state(null);
  // Focus the safe default when the removal panel opens so Enter/Escape
  // cancel instead of removing.
  $effect(() => {
    if (pendingRemove) pendingRemoveCancelEl?.focus();
  });
  function onKeydown(event) {
    if (event.key === 'Escape') {
      event.preventDefault();
      if (pendingRemove) { cancelPendingRemove(); return; }
      close();
    }
    if (event.key === 'ArrowLeft') {
      const submenu = document.activeElement?.closest('.tray-menu-submenu');
      if (submenu) {
        event.preventDefault();
        submenu.parentElement?.querySelector('.tray-menu-parent')?.focus();
        return;
      }
    }
    if (event.key === 'Tab') {
      event.preventDefault();
      const focusable = [...document.querySelectorAll('.tray-menu-item:not(:disabled), .tray-menu-subitem:not(:disabled), .tray-menu-confirm-btn:not(:disabled)')];
      const index = focusable.indexOf(document.activeElement);
      focusable[(index + 1) % focusable.length]?.focus();
    }
  }
  onMount(() => {
    let stopOpen; let stopClose;
    updateViewport();
    window.addEventListener('resize', updateViewport);
    document.addEventListener('pointerdown', onPointerDown, true);
    // The native tray window includes a transparent hit cushion around the
    // visible card. The shell element does not receive contextmenu events in
    // that blank document area, so WebView2 can otherwise show its browser
    // menu there. Capture at document level to suppress it across the entire
    // popup surface, including transparent padding and submenu overflow.
    document.addEventListener('contextmenu', onContextMenu, true);
    invoke('tray_menu_get_snapshot').then((value) => {
      if (receivedOpenEvent || !value.open || !value.data) return;
      snapshot = value;
      animating = true;
      requestAnimationFrame(playEntrance);
      tick().then(() => {
        measureCard();
        logGeometry('snapshot');
      });
    }).catch(() => {});
    function playEntrance() {
      const shell = document.querySelector('.tray-menu-shell');
      if (!shell) return;
      // Force the browser to flush the hidden state before the animation
      // class is added. Without this the first open can skip the initial
      // keyframe and render mid-animation, which appears as a different
      // (sometimes sideways) easing.
      shell.classList.remove('tray-menu-enter');
      void shell.offsetWidth;
      shell.classList.add('tray-menu-enter');
    }
    listen('tray-menu-open', (event) => {
      clearTimeout(closeTimer);
      receivedOpenEvent = true;
      snapshot = { open: true, data: event.payload };
      activeSubmenu = null;
      closing = false;
      cancelPendingRemove();
      animating = true;
      requestAnimationFrame(playEntrance);
      tick().then(() => {
        measureCard();
        logGeometry('open');
      });
      setTimeout(() => {
        measureCard();
        logGeometry('open-settled');
        document.querySelector('.tray-menu-close')?.focus();
      }, 170);
    }).then((stop) => { stopOpen = stop; });
    listen('tray-menu-close', () => {
      beginClose();
    }).then((stop) => { stopClose = stop; });
    // A favorite was added/removed from this menu: swap in the refreshed
    // snapshot (cursor geometry stays untouched in the payload) and re-measure
    // so hit rects and card height track the changed favorites list.
    let stopUpdate;
    listen('tray-menu-update', (event) => {
      if (!snapshot.open || !event.payload) return;
      snapshot = { ...snapshot, data: event.payload };
      cancelPendingRemove();
      // Removing a favorite can drop the list below 3, which swaps the
      // favorites submenu for inline rows; the parent row would otherwise
      // keep its open state with no flyout mounted.
      if (activeSubmenu === 'favorites' && (event.payload.favorites?.length ?? 0) < 3) {
        activeSubmenu = null;
      }
      measureCard();
      if (activeSubmenu) positionSubmenu(activeSubmenu);
      reportHitRects();
    }).then((stop) => { stopUpdate = stop; });
    return () => {
      clearTimeout(submenuCloseTimer);
      clearTimeout(closeTimer);
      cancelPendingRemove();
      window.removeEventListener('resize', updateViewport);
      document.removeEventListener('pointerdown', onPointerDown, true);
      document.removeEventListener('contextmenu', onContextMenu, true);
      stopOpen?.();
      stopClose?.();
      stopUpdate?.();
    };
  });
</script>

<svelte:window onkeydown={onKeydown} />

{#if data && snapshot.open}
  <div class="tray-menu-shell" class:tray-menu-enter={animating} class:tray-menu-closing={closing} class:tray-menu-edge-top={data.taskbar_edge === 'top'} class:tray-menu-edge-bottom={data.taskbar_edge === 'bottom'} class:tray-menu-edge-left={data.taskbar_edge === 'left'} class:tray-menu-edge-right={data.taskbar_edge === 'right'} class:tray-menu-submenus-left={submenuDirection === 'left'} class:tray-menu-submenus-hidden={!submenuRevealed} role="presentation" oncontextmenu={onContextMenu}>
    <div class="tray-menu-card" style={cardStyle} role="menu" aria-label="OLED Helper tray menu">
      <div class="tray-menu-header"><img src={appIcon} alt="" class="tray-menu-app-icon" /><div class="tray-menu-header-copy"><div class="tray-menu-title">{data.title}</div><div class="tray-menu-subtitle">{data.monitor_name}</div><div class="tray-menu-mode">{currentMode}</div></div><button class="tray-menu-close" type="button" aria-label="Close menu" onclick={close}>×</button></div>
      <div class="tray-menu-divider"></div>
      {#if pendingRemove}
        <div class="tray-menu-confirm-panel" role="alertdialog" aria-label="Remove favorite">
          <span class="tray-menu-confirm-copy">Remove <strong>{pendingRemove.width}×{pendingRemove.height} @ {pendingRemove.hz} Hz</strong> from favorites?</span>
          <button class="tray-menu-confirm-btn tray-menu-confirm-cancel" type="button" bind:this={pendingRemoveCancelEl} onclick={cancelPendingRemove}>Cancel</button>
          <button class="tray-menu-confirm-btn tray-menu-confirm-remove" type="button" onclick={confirmPendingRemove}>Remove</button>
        </div>
      {/if}
      <div class="tray-menu-content">
        <div class="tray-menu-row" class:tray-menu-row-open={activeSubmenu === 'resolution'} onpointerenter={() => openSubmenu('resolution')} onpointerleave={(event) => scheduleSubmenuClose(event, 'resolution')} onfocusout={(event) => onMenuFocusOut(event, 'resolution')}><button class="tray-menu-item tray-menu-parent" type="button" aria-haspopup="menu" aria-expanded={activeSubmenu === 'resolution'} onfocus={() => openSubmenu('resolution')} onkeydown={(event) => onParentKeydown(event, 'resolution')} onclick={() => openSubmenu('resolution')}><span class="tray-menu-item-icon">▦</span><span>Resolution</span><span class="tray-menu-chevron">›</span></button>{#if activeSubmenu === 'resolution'}<div class="tray-menu-submenu tray-menu-submenu-resolution" data-tray-submenu="resolution" role="menu" onpointerenter={cancelSubmenuClose} onpointerleave={(event) => scheduleSubmenuClose(event, 'resolution')}>{#each data.resolutions as resolution}{@const favKey2 = favKey(resolution.width, resolution.height, data.refresh_hz || 0)}{@const resFav = isResolutionFavorited(resolution)}                    <div class="tray-menu-subitem-row"><button class="tray-menu-subitem" class:tray-menu-selected={isCurrentResolution(resolution)} type="button" role="menuitem" onclick={() => selectResolution(resolution)}><span class="tray-menu-check">{isCurrentResolution(resolution) ? '✓' : ''}</span><span class="tray-menu-resolution-value">{resolution.width}×{resolution.height}</span>{#if resolution.ultrawide}<span class="tray-menu-resolution-tag">UW</span>{/if}{#if resolution.name}<span class="tray-menu-resolution-tag">{resolution.name}</span>{/if}{#if resolution.native}<span class="tray-menu-resolution-native">Native</span>{/if}</button><button class="tray-menu-star" class:favorited={resFav} class:armed={pendingRemoveKey() === favKey2} type="button" title={resFav ? `Remove ${resolution.width}×${resolution.height} @ ${data.refresh_hz} Hz from favorites` : `Favorite ${resolution.width}×${resolution.height} @ ${data.refresh_hz} Hz`} aria-label={resFav ? `Remove ${resolution.width}×${resolution.height} @ ${data.refresh_hz} Hz from favorites` : `Favorite ${resolution.width}×${resolution.height} @ ${data.refresh_hz} Hz`} onclick={(event) => onResStar(event, resolution)}><svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill={resFav ? 'currentColor' : 'none'} stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/></svg></button></div>{:else}<div class="tray-menu-empty">No resolutions available</div>{/each}</div>{/if}</div>
        <div class="tray-menu-row" class:tray-menu-row-open={activeSubmenu === 'refresh'} onpointerenter={() => openSubmenu('refresh')} onpointerleave={(event) => scheduleSubmenuClose(event, 'refresh')} onfocusout={(event) => onMenuFocusOut(event, 'refresh')}><button class="tray-menu-item tray-menu-parent" type="button" aria-haspopup="menu" aria-expanded={activeSubmenu === 'refresh'} onfocus={() => openSubmenu('refresh')} onkeydown={(event) => onParentKeydown(event, 'refresh')} onclick={() => openSubmenu('refresh')}><span class="tray-menu-item-icon">◷</span><span>Refresh Rate</span><span class="tray-menu-chevron">›</span></button>{#if activeSubmenu === 'refresh'}<div class="tray-menu-submenu tray-menu-submenu-refresh" data-tray-submenu="refresh" role="menu" onpointerenter={cancelSubmenuClose} onpointerleave={(event) => scheduleSubmenuClose(event, 'refresh')}>{#each data.refresh_rates as rate}<button class="tray-menu-subitem" class:tray-menu-selected={isCurrentRate(rate)} type="button" role="menuitem" onclick={() => selectRate(rate)}><span class="tray-menu-check">{isCurrentRate(rate) ? '✓' : ''}</span>{rate} Hz</button>{:else}<div class="tray-menu-empty">No refresh rates available</div>{/each}</div>{/if}</div>
        <div class="tray-menu-divider tray-menu-section-divider"></div>
        {#if data.favorites.length >= 3}
          <div class="tray-menu-row" class:tray-menu-row-open={activeSubmenu === 'favorites'} onpointerenter={() => openSubmenu('favorites')} onpointerleave={(event) => scheduleSubmenuClose(event, 'favorites')} onfocusout={(event) => onMenuFocusOut(event, 'favorites')}><button class="tray-menu-item tray-menu-parent" type="button" aria-haspopup="menu" aria-expanded={activeSubmenu === 'favorites'} onfocus={() => openSubmenu('favorites')} onkeydown={(event) => onParentKeydown(event, 'favorites')} onclick={() => openSubmenu('favorites')}><span class="tray-menu-item-icon">★</span><span>Favorites</span><span class="tray-menu-chevron">›</span></button>{#if activeSubmenu === 'favorites'}<div class="tray-menu-submenu tray-menu-submenu-favorites" data-tray-submenu="favorites" role="menu" onpointerenter={cancelSubmenuClose} onpointerleave={(event) => scheduleSubmenuClose(event, 'favorites')}>{#each data.favorites as favorite}{@const favKey2 = favKey(favorite.width, favorite.height, favorite.hz)}<div class="tray-menu-subitem-row"><button class="tray-menu-subitem" class:tray-menu-selected={isCurrentFavorite(favorite)} type="button" role="menuitem" onclick={() => selectFavorite(favorite)}>{favorite.width}×{favorite.height}<span class="tray-menu-at">@</span>{favorite.hz} Hz</button><button class="tray-menu-star" class:favorited={true} class:armed={pendingRemoveKey() === favKey2} type="button" title={`Remove ${favorite.width}×${favorite.height} @ ${favorite.hz} Hz from favorites`} aria-label={`Remove ${favorite.width}×${favorite.height} @ ${favorite.hz} Hz from favorites`} onclick={(event) => onFavStar(event, favorite)}><svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill="currentColor" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/></svg></button></div>{/each}</div>{/if}</div>
        {:else if data.favorites.length > 0}
          {#each data.favorites as favorite}{@const favKey2 = favKey(favorite.width, favorite.height, favorite.hz)}
            <div class="tray-menu-favorite-row"><button class="tray-menu-item tray-menu-favorite-item" class:tray-menu-selected={isCurrentFavorite(favorite)} type="button" onclick={() => selectFavorite(favorite)}><span class="tray-menu-item-icon">★</span><span>{favorite.width}×{favorite.height}<span class="tray-menu-at">@</span>{favorite.hz} Hz</span></button><button class="tray-menu-star" class:favorited={true} class:armed={pendingRemoveKey() === favKey2} type="button" title={`Remove ${favorite.width}×${favorite.height} @ ${favorite.hz} Hz from favorites`} aria-label={`Remove ${favorite.width}×${favorite.height} @ ${favorite.hz} Hz from favorites`} onclick={(event) => onFavStar(event, favorite)}><svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill="currentColor" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/></svg></button></div>
          {/each}
        {:else}
          <div class="tray-menu-empty tray-menu-favorites-empty"><span class="tray-menu-item-icon">★</span><span>No favorites yet</span></div>
        {/if}
        <div class="tray-menu-divider tray-menu-section-divider"></div>
        <button class="tray-menu-item" type="button" onclick={restart}><span class="tray-menu-item-icon">↻</span><span>Restart</span></button><button class="tray-menu-item tray-menu-danger" type="button" onclick={quit}><span class="tray-menu-item-icon">×</span><span>Quit</span></button>
      </div>
    </div>
  </div>
{/if}
