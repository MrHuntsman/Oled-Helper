<script>
  // Custom CSS tray popup — test replacement for the native menu
  // (tray_menu.rs). Rendered in its own small always-on-top WebviewWindow
  // (see src-tauri/src/tray_menu_window.rs), anchored to the tray icon via
  // tauri-plugin-positioner.
  //
  // Scope for this test pass: a single "Resolution" item that expands into
  // a submenu listing every resolution the current display supports (same
  // data source as the native menu), plus Restart/Quit for parity.
  import { onMount, tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';

  let display = $state(null); // { width, height, resolutions: [{width,height}], ... }
  let resolutionOpen = $state(false);
  // Serialize native region updates so a quick enter/leave cannot apply an
  // older submenu shape after the current one. The OS region, not just CSS,
  // must match the visible cards because the rest of the WebView is
  // transparent but would otherwise still intercept clicks.
  let hitRegionSync = Promise.resolve();

  async function loadDisplayInfo() {
    try {
      display = await invoke('tray_menu_get_display_info');
      syncHitRegion();
    } catch (e) {
      console.error('tray-menu: failed to load display info', e);
    }
  }

  // The native window is built at a fixed full-size footprint (see
  // tray_menu_window.rs) that fits both the base menu and the resolution
  // submenu; only the DOM here changes on hover, not the window itself.
  function openResolution() {
    resolutionOpen = true;
    syncHitRegion();
  }

  function closeResolution() {
    if (!resolutionOpen) return;
    resolutionOpen = false;
    syncHitRegion();
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
    hitRegionSync = hitRegionSync.then(async () => {
      await tick();
      const base = menuRect(document.querySelector('.tray-menu'));
      if (!base) return;
      const expanded = resolutionOpen;
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

  async function pickResolution(res) {
    closeResolution();
    await invoke('tray_menu_set_resolution', { width: res.width, height: res.height });
    await invoke('tray_menu_hide');
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
    const unlisten = listen('tray-menu-show', () => {
      resolutionOpen = false;
      syncHitRegion();
      loadDisplayInfo();
    });
    return () => { unlisten.then((stop) => stop()); };
  });
</script>

<div class="tray-menu-wrap" role="menu" tabindex="-1" onmouseleave={closeResolution}>
  <div class="tray-menu">
    <div
      class="tray-item"
      class:open={resolutionOpen}
      role="menuitem"
      tabindex="0"
      onmouseenter={openResolution}
    >
      <span class="tray-item-label">Resolution</span>
      <span class="tray-item-chevron">›</span>
    </div>

    <div class="tray-sep"></div>

    <div class="tray-item" role="menuitem" tabindex="0" onmouseenter={closeResolution} onclick={restart} onkeydown={(e) => e.key === 'Enter' && restart()}>
      <span class="tray-item-label">Restart</span>
    </div>
    <div class="tray-item" role="menuitem" tabindex="0" onmouseenter={closeResolution} onclick={quit} onkeydown={(e) => e.key === 'Enter' && quit()}>
      <span class="tray-item-label">Quit</span>
    </div>
  </div>

  {#if resolutionOpen && display}
    <div class="tray-submenu" role="menu" tabindex="-1">
      {#each display.resolutions as res (res.width + 'x' + res.height)}
        {@const checked = res.width === display.width && res.height === display.height}
        <div
          class="tray-item"
          role="menuitemradio"
          aria-checked={checked}
          tabindex="0"
          onclick={() => pickResolution(res)}
          onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && pickResolution(res)}
        >
          <span class="tray-item-check">{checked ? '✓' : ''}</span>
          <span class="tray-item-label">{res.width}×{res.height}</span>
        </div>
      {/each}
    </div>
  {/if}
</div>
