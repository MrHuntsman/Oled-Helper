<script>
  // Custom CSS tray popup — replacement for the native menu (tray_menu.rs).
  // Rendered in its own small always-on-top WebviewWindow (see
  // src-tauri/src/tray_menu_window.rs), anchored to the tray icon.
  //
  // Mirrors tray_menu.rs's native menu structure: Resolution, Refresh Rate,
  // and Favorites submenus, then Restart/Quit. This window is a separate
  // WebView from the main app window, so it cannot reach into
  // stores.svelte.js's in-memory state — favorites are read/written directly
  // through load_config/save_config, the same way tray_menu.rs's native
  // Favorites submenu does.
  import { onMount, tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { emit, listen } from '@tauri-apps/api/event';

  let display = $state(null); // { width, height, refresh_hz, refresh_rates, resolutions: [{width,height}], ... }
  let favorites = $state([]); // [{ width, height, hz }]
  // Keep the tray's resolution labels in sync with the main app's persisted
  // "Show tags (4K, QHD, UW)" preference.
  let showResolutionTags = $state(true);
  // Mirror the main app's persisted list directions. The tray keeps its own
  // copies because it runs in a separate WebView and cannot import the main
  // window's in-memory store.
  let resolutionSort = $state('desc');
  let refreshRateSort = $state('desc');
  // Which submenu (if any) is currently open: null | 'resolution' | 'refresh' | 'favorites'.
  let openSubmenu = $state(null);
  // The main app confirms destructive removals. Keep the same safeguard in
  // the tray without using a blocking browser confirm dialog.
  let pendingFavoriteRemove = $state(null);
  // Serialize favorite writes and hold the latest cross-window event until
  // the local write queue drains, so concurrent stars cannot overwrite one
  // another or lose a newer change from the main window.
  let favoritesSaveInFlight = 0;
  let favoritesSaveQueue = Promise.resolve();
  let pendingExternalFavorites = null;
  let closeSubmenuTimer = null;
  // Serialize native region updates so a quick enter/leave cannot apply an
  // older submenu shape after the current one. The OS region, not just CSS,
  // must match the visible cards because the rest of the WebView is
  // transparent but would otherwise still intercept clicks.
  let hitRegionSync = Promise.resolve();

  async function loadDisplayInfo() {
    try {
      display = await invoke('tray_menu_get_display_info');
      resetSubmenuScroll();
      syncHitRegion();
    } catch (e) {
      console.error('tray-menu: failed to load display info', e);
    }
  }

  async function loadFavorites() {
    try {
      const cfg = await invoke('load_config');
      showResolutionTags = cfg?.display_menu_show_tags !== false;
      resolutionSort = cfg?.display_menu_resolution_sort === 'asc' ? 'asc' : 'desc';
      refreshRateSort = cfg?.display_menu_refresh_rate_sort === 'asc' ? 'asc' : 'desc';
      favorites = Array.isArray(cfg?.favorite_display_modes)
        ? cfg.favorite_display_modes
            .filter((f) => f && f.width > 0 && f.height > 0 && f.hz > 0)
            .map((f) => ({ width: Math.round(f.width), height: Math.round(f.height), hz: Math.round(f.hz) }))
        : [];
      syncHitRegion();
    } catch (e) {
      console.error('tray-menu: failed to load favorites', e);
    }
  }

  function normalizeFavorites(list) {
    return Array.isArray(list)
      ? list
          .filter((f) => f && f.width > 0 && f.height > 0 && f.hz > 0)
          .map((f) => ({ width: Math.round(f.width), height: Math.round(f.height), hz: Math.round(f.hz) }))
      : [];
  }

  function applyFavoritesList(next) {
    favorites = next;
    pendingFavoriteRemove = null;
    syncHitRegion();
  }

  function setFavoritesFromEvent(list) {
    const next = normalizeFavorites(list);
    if (favoritesSaveInFlight > 0) {
      pendingExternalFavorites = next;
      return;
    }
    applyFavoritesList(next);
  }

  async function saveFavorites(next) {
    const savedFavorites = normalizeFavorites(next);
    favorites = savedFavorites;
    favoritesSaveInFlight++;
    // The inline/direct layout can change immediately at the 3→4 boundary;
    // update the native hit region without waiting for disk I/O.
    syncHitRegion();
    const operation = favoritesSaveQueue.then(async () => {
      const cfg = await invoke('load_config');
      await invoke('save_config', {
        config: { ...cfg, favorite_display_modes: savedFavorites },
      });
      await emit('favorites-changed', savedFavorites);
    });
    favoritesSaveQueue = operation.catch(() => {});
    try {
      await operation;
    } catch (e) {
      console.error('tray-menu: failed to save favorites', e);
    } finally {
      favoritesSaveInFlight--;
      if (favoritesSaveInFlight === 0 && pendingExternalFavorites) {
        const external = pendingExternalFavorites;
        pendingExternalFavorites = null;
        applyFavoritesList(external);
      }
    }
  }

  // The native window is built at a fixed full-size footprint (see
  // tray_menu_window.rs) that fits both the base menu and any submenu; only
  // the DOM here changes on hover, not the window itself.
  function cancelScheduledSubmenuClose() {
    clearTimeout(closeSubmenuTimer);
    closeSubmenuTimer = null;
  }

  function openSubmenuFor(name) {
    cancelScheduledSubmenuClose();
    openSubmenu = name;
    resetSubmenuScroll();
    syncHitRegion();
  }

  // The main app's ascending order puts the largest value at the bottom of
  // the list, while descending puts it at the top. Reapply that endpoint every
  // time a numeric submenu opens (and after its list is refreshed), so users
  // always start at the highest resolution/refresh rate rather than inheriting
  // the previous scroll position from the WebView.
  function resetSubmenuScroll() {
    const name = openSubmenu;
    if (name !== 'resolution' && name !== 'refresh') return;
    tick().then(() => requestAnimationFrame(() => {
      if (openSubmenu !== name) return;
      const submenu = document.querySelector('.tray-submenu');
      if (!submenu) return;
      const direction = name === 'resolution' ? resolutionSort : refreshRateSort;
      submenu.scrollTop = direction === 'asc'
        ? Math.max(0, submenu.scrollHeight - submenu.clientHeight)
        : 0;
    }));
  }

  function sortedResolutions() {
    const dir = resolutionSort === 'asc' ? 1 : -1;
    return [...(display?.resolutions || [])].sort((a, b) => (
      dir * ((a.width * a.height) - (b.width * b.height))
    ));
  }

  function sortedRefreshRates() {
    const dir = refreshRateSort === 'asc' ? 1 : -1;
    return [...(display?.refresh_rates || [])].sort((a, b) => dir * (a - b));
  }

  function closeSubmenu() {
    cancelScheduledSubmenuClose();
    if (openSubmenu === null) return;
    openSubmenu = null;
    syncHitRegion();
  }

  // Give the pointer a short bridge while the native region catches up after
  // the submenu is shown. A real exit still closes promptly, but a transient
  // mouseleave at the pane seam cannot tear down the submenu mid-transition.
  function scheduleSubmenuClose() {
    cancelScheduledSubmenuClose();
    closeSubmenuTimer = setTimeout(() => {
      closeSubmenuTimer = null;
      closeSubmenu();
    }, 100);
  }

  function menuRect(element) {
    if (!element) return null;
    // offset* reports the settled layout box relative to the positioned
    // wrapper and deliberately ignores the card's entrance transform. Using
    // getBoundingClientRect() here would capture the transient scale/offset
    // and leave the native region misaligned after the animation finishes.
    return {
      x: element.offsetLeft,
      y: element.offsetTop,
      width: element.offsetWidth,
      height: element.offsetHeight,
    };
  }

  // Resize neither the native window nor the layout. Instead, tell Rust the
  // exact visible card rectangles and let Windows hit-test only those rounded
  // regions. This keeps the fixed taskbar anchor stable and makes the
  // transparent canvas outside the cards click through to the window below.
  function syncHitRegion() {
    // Install a conservative bridge before waiting for Svelte's DOM update.
    // Without this first pass, the submenu is visible in the browser while
    // Windows still has only the collapsed base-menu region; a fast pointer
    // crossing can blur/hide the window before the exact measurement arrives.
    const preBase = openSubmenu !== null
      ? menuRect(document.querySelector('.tray-menu'))
      : null;
    const preinstall = preBase
      ? {
          // The CSS width excludes the submenu's 12px horizontal padding
          // and 1px borders on each side, so its rendered outer width is
          // 244px. Match that here to keep the preinstalled native region
          // continuous until the exact DOM measurement arrives.
          x: Math.max(0, preBase.x - 244),
          y: preBase.y,
          width: 244,
          height: preBase.height,
        }
      : null;

    hitRegionSync = hitRegionSync.then(async () => {
      if (preinstall && openSubmenu !== null) {
        try {
          await invoke('tray_menu_set_expanded', {
            expanded: true,
            base: preBase,
            submenu: preinstall,
          });
        } catch (e) {
          console.error('tray-menu: failed to preinstall hit region', e);
        }
      }
      await tick();
      const base = menuRect(document.querySelector('.tray-menu'));
      if (!base) return;
      const expanded = openSubmenu !== null;
      const submenu = expanded
        ? menuRect(document.querySelector('.tray-submenu'))
        : null;
      try {
        await invoke('tray_menu_set_expanded', {
          expanded,
          base,
          submenu,
        });
      } catch (e) {
        console.error('tray-menu: failed to update hit region', e);
      }
    });
  }

  function isFavoriteMode(width, height, hz) {
    return favorites.some((fav) => favoriteKey(fav) === `${width}x${height}@${hz}`);
  }

  function requestFavoriteRemove(fav) {
    pendingFavoriteRemove = { width: fav.width, height: fav.height, hz: fav.hz };
    closeSubmenu();
    syncHitRegion();
  }

  function cancelFavoriteRemove() {
    pendingFavoriteRemove = null;
    syncHitRegion();
  }

  async function confirmFavoriteRemove() {
    if (!pendingFavoriteRemove) return;
    const key = favoriteKey(pendingFavoriteRemove);
    pendingFavoriteRemove = null;
    syncHitRegion();
    await saveFavorites(favorites.filter((fav) => favoriteKey(fav) !== key));
  }

  async function toggleFavoriteMode(width, height, hz) {
    if (!width || !height || !hz) return;
    const fav = { width, height, hz };
    if (isFavoriteMode(width, height, hz)) {
      requestFavoriteRemove(fav);
      return;
    }
    await saveFavorites([...favorites, fav]);
  }

  async function pickResolution(res) {
    if (display && res.width === display.width && res.height === display.height) return;
    closeSubmenu();
    await invoke('tray_menu_set_resolution', { width: res.width, height: res.height });
    await invoke('tray_menu_hide');
  }

  async function pickRefreshRate(hz) {
    if (display && hz === display.refresh_hz) return;
    closeSubmenu();
    await invoke('tray_menu_set_refresh_rate', { hz });
    await invoke('tray_menu_hide');
  }

  async function pickFavorite(fav) {
    if (isCurrentFavorite(fav)) return;
    closeSubmenu();
    await invoke('tray_menu_set_resolution', { width: fav.width, height: fav.height, hz: fav.hz });
    await invoke('tray_menu_hide');
  }

  const resolutionNames = new Map([
    ['1280x720', 'HD'],
    ['1920x1080', 'FHD'],
    ['2560x1080', 'Ultrawide FHD'],
    ['2560x1440', 'QHD'],
    ['3440x1440', 'UWQHD'],
    ['3840x2160', '4K'],
    ['5120x2160', '5K Ultrawide'],
    ['5120x2880', '5K'],
    ['7680x4320', '8K'],
  ]);

  function resolutionName(resolution) {
    return resolutionNames.get(`${resolution.width}x${resolution.height}`) || '';
  }

  function isUltrawide(width, height) {
    return width / height > 1.9;
  }

  function isRecommendedResolution(resolution) {
    if (!display) return false;
    if (display.native_width && display.native_height) {
      return resolution.width === display.native_width && resolution.height === display.native_height;
    }
    const resolutions = Array.isArray(display.resolutions) ? display.resolutions : [];
    if (resolutions.length === 0) return false;
    const largest = resolutions.reduce((best, candidate) => (
      candidate.width * candidate.height > best.width * best.height ? candidate : best
    ));
    return resolution.width === largest.width && resolution.height === largest.height;
  }

  function favoriteKey(f) {
    return `${f.width}x${f.height}@${f.hz}`;
  }

  function favoriteLabel(fav) {
    return `${fav.width}×${fav.height} @ ${fav.hz} Hz`;
  }

  function isCurrentFavorite(fav) {
    return display
      && fav.width === display.width
      && fav.height === display.height
      && fav.hz === display.refresh_hz;
  }

  async function restart() {
    await invoke('tray_menu_restart');
  }

  async function quit() {
    await invoke('tray_menu_quit');
  }

  onMount(() => {
    syncHitRegion();
    loadDisplayInfo();
    loadFavorites();
    const unlisten = listen('tray-menu-show', () => {
      openSubmenu = null;
      pendingFavoriteRemove = null;
      syncHitRegion();
      loadDisplayInfo();
      loadFavorites();
    });
    const unlistenTags = listen('display-menu-tags-changed', (event) => {
      showResolutionTags = event.payload !== false;
    });
    const unlistenSort = listen('display-menu-sorting-changed', (event) => {
      const prefs = event.payload || {};
      if (prefs.resolutionSort === 'asc' || prefs.resolutionSort === 'desc') {
        resolutionSort = prefs.resolutionSort;
      }
      if (prefs.refreshRateSort === 'asc' || prefs.refreshRateSort === 'desc') {
        refreshRateSort = prefs.refreshRateSort;
      }
      resetSubmenuScroll();
    });
    const unlistenFavorites = listen('favorites-changed', (event) => {
      setFavoritesFromEvent(event.payload);
    });
    return () => {
      unlisten.then((stop) => stop());
      unlistenTags.then((stop) => stop());
      unlistenSort.then((stop) => stop());
      unlistenFavorites.then((stop) => stop());
    };
  });
</script>

<div
  class="tray-menu-wrap"
  class:tray-submenu-open={openSubmenu !== null}
  role="menu"
  tabindex="-1"
  onmouseenter={cancelScheduledSubmenuClose}
  onmouseleave={scheduleSubmenuClose}
>
  <div class="tray-menu">
    {#if pendingFavoriteRemove}
      <div class="tray-favorite-confirm" role="dialog" aria-modal="true" aria-labelledby="tray-favorite-confirm-title">
        <div id="tray-favorite-confirm-title" class="tray-favorite-confirm-title">Remove favorite?</div>
        <div class="tray-favorite-confirm-copy">
          Remove {pendingFavoriteRemove.width}×{pendingFavoriteRemove.height} @ {pendingFavoriteRemove.hz} Hz?
        </div>
        <div class="tray-favorite-confirm-actions">
          <button type="button" class="tray-favorite-confirm-btn" onclick={cancelFavoriteRemove}>Cancel</button>
          <button type="button" class="tray-favorite-confirm-btn tray-favorite-confirm-danger" onclick={confirmFavoriteRemove}>Remove</button>
        </div>
      </div>
    {:else}
    <div
      class="tray-item"
      class:open={openSubmenu === 'resolution'}
      role="menuitem"
      tabindex="0"
      onmouseenter={() => openSubmenuFor('resolution')}
    >
      <span class="tray-item-label">Resolution</span>
      <span class="tray-item-chevron">›</span>
    </div>

    <div
      class="tray-item"
      class:open={openSubmenu === 'refresh'}
      role="menuitem"
      tabindex="0"
      onmouseenter={() => openSubmenuFor('refresh')}
    >
      <span class="tray-item-label">Refresh Rate</span>
      <span class="tray-item-chevron">›</span>
    </div>

    {#if favorites.length > 3}
      <div
        class="tray-item"
        class:open={openSubmenu === 'favorites'}
        role="menuitem"
        tabindex="0"
        onmouseenter={() => openSubmenuFor('favorites')}
      >
        <span class="tray-item-label">Favorites</span>
        <span class="tray-item-chevron">›</span>
      </div>
    {:else}
      {#each favorites as fav (favoriteKey(fav))}
        {@const current = isCurrentFavorite(fav)}
        <div
          class="tray-item tray-favorite-item"
          class:tray-favorite-current={current}
          role="menuitem"
          tabindex={current ? -1 : 0}
          onmouseenter={closeSubmenu}
          onclick={() => !current && pickFavorite(fav)}
          onkeydown={(e) => !current && (e.key === 'Enter' || e.key === ' ') && pickFavorite(fav)}
        >
          <span class="tray-item-check">{current ? '✓' : ''}</span>
          <span class="tray-item-label">{favoriteLabel(fav)}</span>
          <button
            type="button"
            class="tray-favorite-star"
            class:filled={true}
            aria-label={`Remove ${favoriteLabel(fav)} from favorites`}
            title={`Remove ${favoriteLabel(fav)} from favorites`}
            onclick={(event) => { event.stopPropagation(); requestFavoriteRemove(fav); }}
            onkeydown={(event) => event.stopPropagation()}
          >
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
              <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill="currentColor" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
            </svg>
          </button>
        </div>
      {/each}
    {/if}

    <div class="tray-sep"></div>

    <div class="tray-item" role="menuitem" tabindex="0" onmouseenter={closeSubmenu} onclick={restart} onkeydown={(e) => e.key === 'Enter' && restart()}>
      <span class="tray-item-label">Restart</span>
    </div>
    <div class="tray-item" role="menuitem" tabindex="0" onmouseenter={closeSubmenu} onclick={quit} onkeydown={(e) => e.key === 'Enter' && quit()}>
      <span class="tray-item-label">Quit</span>
    </div>
    {/if}
  </div>

  {#if openSubmenu === 'resolution' && display}
    <div class="tray-submenu tray-resolution-submenu" role="menu" tabindex="-1">
      {#each sortedResolutions() as res (res.width + 'x' + res.height)}
        {@const checked = res.width === display.width && res.height === display.height}
        {@const name = resolutionName(res)}
        {@const ultrawide = isUltrawide(res.width, res.height)}
        {@const recommended = isRecommendedResolution(res)}
        <div
          class="tray-item"
          class:tray-current={checked}
          role="menuitemradio"
          aria-checked={checked}
          aria-disabled={checked}
          tabindex={checked ? -1 : 0}
          onclick={() => !checked && pickResolution(res)}
          onkeydown={(e) => !checked && (e.key === 'Enter' || e.key === ' ') && pickResolution(res)}
        >
          <span class="tray-item-check">{checked ? '✓' : ''}</span>
          <span class="tray-item-label tray-resolution-label">
            <span>{res.width}×{res.height}</span>
            {#if showResolutionTags && ultrawide}<span class="tray-resolution-tag">UW</span>{/if}
            {#if showResolutionTags && name}<span class="tray-resolution-tag">{name}</span>{/if}
            {#if recommended}<span class="tray-resolution-native">(Native)</span>{/if}
          </span>
          <button
            type="button"
            class="tray-favorite-star"
            class:filled={isFavoriteMode(res.width, res.height, display.refresh_hz)}
            aria-label={isFavoriteMode(res.width, res.height, display.refresh_hz) ? `Remove ${res.width}×${res.height} @ ${display.refresh_hz} Hz from favorites` : `Favorite ${res.width}×${res.height} @ ${display.refresh_hz} Hz`}
            title={isFavoriteMode(res.width, res.height, display.refresh_hz) ? 'Remove from favorites' : 'Add to favorites'}
            onclick={(event) => { event.stopPropagation(); toggleFavoriteMode(res.width, res.height, display.refresh_hz); }}
            onkeydown={(event) => event.stopPropagation()}
          >
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
              <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill={isFavoriteMode(res.width, res.height, display.refresh_hz) ? 'currentColor' : 'none'} stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
            </svg>
          </button>
        </div>
      {/each}
    </div>
  {/if}

  {#if openSubmenu === 'refresh' && display}
    <div class="tray-submenu" role="menu" tabindex="-1">
      {#each sortedRefreshRates() as hz (hz)}
        {@const checked = hz === display.refresh_hz}
        <div
          class="tray-item"
          class:tray-current={checked}
          role="menuitemradio"
          aria-checked={checked}
          aria-disabled={checked}
          tabindex={checked ? -1 : 0}
          onclick={() => !checked && pickRefreshRate(hz)}
          onkeydown={(e) => !checked && (e.key === 'Enter' || e.key === ' ') && pickRefreshRate(hz)}
        >
          <span class="tray-item-check">{checked ? '✓' : ''}</span>
          <span class="tray-item-label">{hz} Hz</span>
          <button
            type="button"
            class="tray-favorite-star"
            class:filled={isFavoriteMode(display.width, display.height, hz)}
            aria-label={isFavoriteMode(display.width, display.height, hz) ? `Remove ${display.width}×${display.height} @ ${hz} Hz from favorites` : `Favorite ${display.width}×${display.height} @ ${hz} Hz`}
            title={isFavoriteMode(display.width, display.height, hz) ? 'Remove from favorites' : 'Add to favorites'}
            onclick={(event) => { event.stopPropagation(); toggleFavoriteMode(display.width, display.height, hz); }}
            onkeydown={(event) => event.stopPropagation()}
          >
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
              <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill={isFavoriteMode(display.width, display.height, hz) ? 'currentColor' : 'none'} stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
            </svg>
          </button>
        </div>
      {/each}
    </div>
  {/if}

  {#if openSubmenu === 'favorites' && favorites.length > 3}
    <div class="tray-submenu" role="menu" tabindex="-1">
      {#each favorites as fav (favoriteKey(fav))}
        {@const current = isCurrentFavorite(fav)}
        <div
          class="tray-item tray-favorite-item"
          class:tray-favorite-current={current}
          role="menuitem"
          tabindex={current ? -1 : 0}
          onclick={() => !current && pickFavorite(fav)}
          onkeydown={(e) => !current && (e.key === 'Enter' || e.key === ' ') && pickFavorite(fav)}
        >
          <span class="tray-item-check">{current ? '✓' : ''}</span>
          <span class="tray-item-label">{favoriteLabel(fav)}</span>
          <button
            type="button"
            class="tray-favorite-star"
            class:filled={true}
            aria-label={`Remove ${favoriteLabel(fav)} from favorites`}
            title={`Remove ${favoriteLabel(fav)} from favorites`}
            onclick={(event) => { event.stopPropagation(); requestFavoriteRemove(fav); }}
            onkeydown={(event) => event.stopPropagation()}
          >
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
              <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill="currentColor" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
            </svg>
          </button>
        </div>
      {/each}
    </div>
  {/if}
</div>
