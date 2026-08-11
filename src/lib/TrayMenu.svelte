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
  let receivedOpenEvent = false;
  const data = $derived(snapshot.data);
  const currentMode = $derived(data ? `${data.width}×${data.height} @ ${data.refresh_hz} Hz` : 'No display detected');
  // Track the card + submenu layout while the menu is open so the backend can
  // keep the native hit-test transparent outside those bounds. Report right
  // away (the cushion also covers the entrance animation travel) and again
  // once the entrance animation and submenu positioning have settled.
  $effect(() => {
    if (!snapshot.open) return;
    void activeSubmenu;
    reportHitRects();
    const timer = setTimeout(reportHitRects, 170);
    return () => clearTimeout(timer);
  });
  const submenuDirection = $derived(data?.taskbar_edge === 'bottom' || data?.taskbar_edge === 'right' ? 'left' : 'right');

  // Report the clickable bounds (card + open submenu) to the backend. The
  // native window is a full-work-area transparent WebView; without this, the OS
  // hit-tests the whole window and swallows mouse input for the desktop and
  // other apps beneath it. Rects are client-space physical px (getBoundingClientRect
  // is already relative to the undecorated window's client area, scaled by dpr).
  // The cushion keeps a hover margin around each element so moving the cursor
  // just outside the card still delivers events to the WebView and pointerleave
  // can close an open submenu.
  function reportHitRects() {
    const scale = window.devicePixelRatio || 1;
    // Hover margin around each element; must be >= the entrance animation's
    // translateY offset (18px in tray-menu-card-in) so the region never clips
    // the card while it is rising into place.
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
  function beginClose() {
    if (!snapshot.open || closing) return;
    cancelSubmenuClose();
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
    const viewportPadding = 12;
    const availableAbove = Math.max(120, rowRect.bottom - viewportPadding);
    const availableBelow = Math.max(120, window.innerHeight - rowRect.top - viewportPadding);
    const growDown = availableBelow >= availableAbove || availableBelow >= 430;
    const available = growDown ? availableBelow : availableAbove;

    submenu.style.maxHeight = `${Math.min(430, available)}px`;
    submenu.style.top = growDown ? '0' : 'auto';
    submenu.style.bottom = growDown ? 'auto' : '0';
  }
  function cancelSubmenuClose() {
    clearTimeout(submenuCloseTimer);
    submenuCloseTimer = null;
  }
  function scheduleSubmenuClose(name) {
    cancelSubmenuClose();
    submenuCloseTimer = setTimeout(() => {
      if (activeSubmenu === name) activeSubmenu = null;
      submenuCloseTimer = null;
    }, 120);
  }
  async function openSubmenu(name) {
    cancelSubmenuClose();
    // Keep the submenu hidden until the native hit region has been extended to
    // cover it. SetWindowRgn clips rendering too, and the region update goes
    // through an async IPC round-trip, so revealing the submenu before the
    // region is applied shows it partially clipped ("opens in two stages").
    submenuRevealed = false;
    activeSubmenu = name;
    await tick();
    positionSubmenu(name);
    // Race the region update against a short grace period so the submenu can
    // never stay hidden if the IPC round-trip hangs (e.g. during teardown).
    await Promise.race([
      reportHitRects(),
      new Promise((resolve) => setTimeout(resolve, 150)),
    ]);
    if (activeSubmenu === name) submenuRevealed = true;
  }
  function onMenuFocusOut(event, name) {
    const next = event.relatedTarget;
    const row = event.currentTarget;
    if (next && row.contains(next)) return;
    scheduleSubmenuClose(name);
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
    if (!card || event.composedPath().includes(card)) return;
    close();
  }
  function onKeydown(event) {
    if (event.key === 'Escape') { event.preventDefault(); close(); }
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
      const focusable = [...document.querySelectorAll('.tray-menu-item:not(:disabled), .tray-menu-subitem:not(:disabled)')];
      const index = focusable.indexOf(document.activeElement);
      focusable[(index + 1) % focusable.length]?.focus();
    }
  }
  onMount(() => {
    let stopOpen; let stopClose;
    document.addEventListener('pointerdown', onPointerDown, true);
    invoke('tray_menu_get_snapshot').then((value) => {
      if (receivedOpenEvent || !value.open || !value.data) return;
      snapshot = value;
      animating = true;
      requestAnimationFrame(playEntrance);
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
      animating = true;
      requestAnimationFrame(playEntrance);
      setTimeout(() => document.querySelector('.tray-menu-close')?.focus(), 0);
    }).then((stop) => { stopOpen = stop; });
    listen('tray-menu-close', () => {
      beginClose();
    }).then((stop) => { stopClose = stop; });
    return () => {
      clearTimeout(submenuCloseTimer);
      clearTimeout(closeTimer);
      document.removeEventListener('pointerdown', onPointerDown, true);
      stopOpen?.();
      stopClose?.();
    };
  });
</script>

<svelte:window onkeydown={onKeydown} />

{#if data && snapshot.open}
  <div class="tray-menu-shell" class:tray-menu-enter={animating} class:tray-menu-closing={closing} class:tray-menu-edge-top={data.taskbar_edge === 'top'} class:tray-menu-edge-bottom={data.taskbar_edge === 'bottom'} class:tray-menu-edge-left={data.taskbar_edge === 'left'} class:tray-menu-edge-right={data.taskbar_edge === 'right'} class:tray-menu-submenus-left={submenuDirection === 'left'} class:tray-menu-submenus-hidden={!submenuRevealed} role="presentation">
    <div class="tray-menu-card" role="menu" aria-label="OLED Helper tray menu">
      <div class="tray-menu-header"><img src={appIcon} alt="" class="tray-menu-app-icon" /><div class="tray-menu-header-copy"><div class="tray-menu-title">{data.title}</div><div class="tray-menu-subtitle">{data.monitor_name}</div><div class="tray-menu-mode">{currentMode}</div></div><button class="tray-menu-close" type="button" aria-label="Close menu" onclick={close}>×</button></div>
      <div class="tray-menu-divider"></div>
      <div class="tray-menu-content">
        <div class="tray-menu-row" class:tray-menu-row-open={activeSubmenu === 'resolution'} onpointerenter={() => openSubmenu('resolution')} onpointerleave={() => scheduleSubmenuClose('resolution')} onfocusout={(event) => onMenuFocusOut(event, 'resolution')}><button class="tray-menu-item tray-menu-parent" type="button" aria-haspopup="menu" aria-expanded={activeSubmenu === 'resolution'} onfocus={() => openSubmenu('resolution')} onkeydown={(event) => onParentKeydown(event, 'resolution')} onclick={() => openSubmenu('resolution')}><span class="tray-menu-item-icon">▦</span><span>Resolution</span><span class="tray-menu-chevron">›</span></button>{#if activeSubmenu === 'resolution'}<div class="tray-menu-submenu tray-menu-submenu-resolution" data-tray-submenu="resolution" role="menu" onpointerenter={cancelSubmenuClose} onpointerleave={() => scheduleSubmenuClose('resolution')}>{#each data.resolutions as resolution}<button class="tray-menu-subitem" class:tray-menu-selected={isCurrentResolution(resolution)} type="button" role="menuitem" onclick={() => selectResolution(resolution)}><span class="tray-menu-check">{isCurrentResolution(resolution) ? '✓' : ''}</span>{resolution.width}×{resolution.height}</button>{:else}<div class="tray-menu-empty">No resolutions available</div>{/each}</div>{/if}</div>
        <div class="tray-menu-row" class:tray-menu-row-open={activeSubmenu === 'refresh'} onpointerenter={() => openSubmenu('refresh')} onpointerleave={() => scheduleSubmenuClose('refresh')} onfocusout={(event) => onMenuFocusOut(event, 'refresh')}><button class="tray-menu-item tray-menu-parent" type="button" aria-haspopup="menu" aria-expanded={activeSubmenu === 'refresh'} onfocus={() => openSubmenu('refresh')} onkeydown={(event) => onParentKeydown(event, 'refresh')} onclick={() => openSubmenu('refresh')}><span class="tray-menu-item-icon">◷</span><span>Refresh Rate</span><span class="tray-menu-chevron">›</span></button>{#if activeSubmenu === 'refresh'}<div class="tray-menu-submenu tray-menu-submenu-refresh" data-tray-submenu="refresh" role="menu" onpointerenter={cancelSubmenuClose} onpointerleave={() => scheduleSubmenuClose('refresh')}>{#each data.refresh_rates as rate}<button class="tray-menu-subitem" class:tray-menu-selected={isCurrentRate(rate)} type="button" role="menuitem" onclick={() => selectRate(rate)}><span class="tray-menu-check">{isCurrentRate(rate) ? '✓' : ''}</span>{rate} Hz</button>{:else}<div class="tray-menu-empty">No refresh rates available</div>{/each}</div>{/if}</div>
        <div class="tray-menu-divider tray-menu-section-divider"></div>
        {#if data.favorites.length >= 3}
          <div class="tray-menu-row" class:tray-menu-row-open={activeSubmenu === 'favorites'} onpointerenter={() => openSubmenu('favorites')} onpointerleave={() => scheduleSubmenuClose('favorites')} onfocusout={(event) => onMenuFocusOut(event, 'favorites')}><button class="tray-menu-item tray-menu-parent" type="button" aria-haspopup="menu" aria-expanded={activeSubmenu === 'favorites'} onfocus={() => openSubmenu('favorites')} onkeydown={(event) => onParentKeydown(event, 'favorites')} onclick={() => openSubmenu('favorites')}><span class="tray-menu-item-icon">★</span><span>Favorites</span><span class="tray-menu-chevron">›</span></button>{#if activeSubmenu === 'favorites'}<div class="tray-menu-submenu tray-menu-submenu-favorites" data-tray-submenu="favorites" role="menu" onpointerenter={cancelSubmenuClose} onpointerleave={() => scheduleSubmenuClose('favorites')}>{#each data.favorites as favorite}<button class="tray-menu-subitem" class:tray-menu-selected={isCurrentFavorite(favorite)} type="button" role="menuitem" onclick={() => selectFavorite(favorite)}><span class="tray-menu-check">{isCurrentFavorite(favorite) ? '✓' : ''}</span>{favorite.width}×{favorite.height}<span class="tray-menu-at">@</span>{favorite.hz} Hz</button>{/each}</div>{/if}</div>
        {:else if data.favorites.length > 0}
          {#each data.favorites as favorite}
            <button class="tray-menu-item tray-menu-favorite-item" class:tray-menu-selected={isCurrentFavorite(favorite)} type="button" onclick={() => selectFavorite(favorite)}><span class="tray-menu-item-icon">★</span><span>{favorite.width}×{favorite.height}<span class="tray-menu-at">@</span>{favorite.hz} Hz</span><span class="tray-menu-favorite-check">{isCurrentFavorite(favorite) ? '✓' : ''}</span></button>
          {/each}
        {:else}
          <div class="tray-menu-empty tray-menu-favorites-empty"><span class="tray-menu-item-icon">★</span><span>No favorites yet</span></div>
        {/if}
        <button class="tray-menu-item" type="button" onclick={restart}><span class="tray-menu-item-icon">↻</span><span>Restart</span></button><button class="tray-menu-item tray-menu-danger" type="button" onclick={quit}><span class="tray-menu-item-icon">×</span><span>Quit</span></button>
      </div>
    </div>
  </div>
{/if}
