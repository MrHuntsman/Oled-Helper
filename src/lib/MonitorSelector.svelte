<script>
  import { invoke } from '@tauri-apps/api/core';
  import { onDestroy } from 'svelte';
  import { app, monitors, activeMonitor, setActiveMonitor } from './stores.svelte.js';
  import { fmtNits } from './calibration.js';
  import { dropdownCloseEffect, createDropdownKeyboardHandlers } from './dropdown.js';
  import { createDropdownPositionController } from './dropdown-position.js';

  // Reusable monitor selector: a trigger that opens a list of every connected
  // monitor, with a per-monitor hover tooltip showing the same fields as the
  // status-bar tooltip. Lives in the status bar (variant 'status',
  // transparent until hover like the resolution and Hz selects). The variant
  // prop is kept generic so the filled control look can be reused elsewhere.
  let { variant = 'status' } = $props();

  let menuOpen = $state(false);
  let menuEl = $state(null);
  let wrapEl = $state(null);
  let triggerEl = $state(null);
  let menuPlacement = $state(variant === 'status' ? 'up-right' : 'down-right');
  let highlightIndex = $state(-1);
  let positionController = null;

  // Label of the active monitor, or '' for Global (no per-monitor selection).
  // Matches the dropdown rows, including the primary marker.
  const label = $derived(
    activeMonitor.mon
      ? `${activeMonitor.mon.friendly_name || `Monitor ${monitors.indexOf(activeMonitor.mon) + 1}`}${activeMonitor.mon.primary ? ' · Primary' : ''}`
      : ''
  );

  function currentMonitorOptionIndex() {
    return monitors.findIndex((monitor) => monitor.sig === activeMonitor.mon?.sig);
  }

  function toggleMenu() {
    menuOpen = !menuOpen;
    if (menuOpen) {
      highlightIndex = currentMonitorOptionIndex();
      positionController?.stop();
      positionController = createDropdownPositionController({
        getTrigger: () => wrapEl?.querySelector('.monitor-select-trigger'),
        getMenu: () => menuEl,
        onPlacement: (placement) => { menuPlacement = placement; },
        options: { gap: 6, viewportMargin: 14, maxHeight: 320 },
      });
      positionController.start();
    } else {
      positionController?.stop();
      positionController = null;
    }
  }
  function closeMenu() {
    menuOpen = false;
    highlightIndex = -1;
    positionController?.stop();
    positionController = null;
    hoverTip.visible = false;
  }
  function pickMonitor(sig) {
    closeMenu();
    if (sig) setActiveMonitor(monitors.find((m) => m.sig === sig) || null);
  }

  // Per-monitor hover tooltip: same fields as the status-bar monitor tooltip,
  // fetched on demand for the hovered monitor and shown below the cursor.
  // Info is cached per monitor so hovering the same monitor again is instant.
  let hoverTip = $state({ visible: false, x: 0, y: 0, info: null, sig: '' });
  const hoveredMonitor = $derived(monitors.find((m) => m.sig === hoverTip.sig) || null);
  const tipCache = new Map();
  let tipFetchSeq = 0;

  function showHoverTip(e, mon) {
    if (!mon?.device_name) return;
    hoverTip.sig = mon.sig;
    positionHoverTip(e);
    const cached = tipCache.get(mon.sig);
    if (cached !== undefined) {
      hoverTip.info = cached;
      hoverTip.visible = true;
      return;
    }
    hoverTip.visible = true;
    const seq = ++tipFetchSeq;
    invoke('get_display_info_for', { deviceName: mon.device_name })
      .then((info) => {
        if (seq !== tipFetchSeq) return;
        const tip = info
          ? {
              connector: info.connector || '-',
              colorFormat: info.color_format ? `${info.color_format}${info.bits_per_channel ? ` · ${info.bits_per_channel} bpc` : ''}` : '-',
              dynamicRange: info.dynamic_range || '-',
              vrr: info.vrr || '-',
              current: `${info.width}×${info.height}${info.refresh_hz ? ` @ ${info.refresh_hz}Hz` : ''}`,
            }
          : null;
        tipCache.set(mon.sig, tip);
        if (hoverTip.sig === mon.sig) hoverTip.info = tip;
      })
      .catch(() => {
        if (hoverTip.sig === mon.sig) hoverTip.info = null;
      });
  }
  function positionHoverTip(e) {
    // Offset above and to the right of the cursor so the dropdown rows below
    // stay visible, then clamp so the tip never runs off the top or right
    // edge (estimates track the actual panel size, like the context-menu
    // clamp).
    hoverTip.x = Math.max(8, Math.min(e.clientX + 14, window.innerWidth - 260 - 8));
    hoverTip.y = Math.max(8, Math.min(e.clientY - 170 - 14, window.innerHeight - 170 - 8));
  }
  function moveHoverTip(e) {
    if (!hoverTip.visible) return;
    positionHoverTip(e);
  }
  function hideHoverTip() {
    hoverTip.visible = false;
    hoverTip.info = null;
  }

  const keyboard = createDropdownKeyboardHandlers({
    getOptions: () => monitors,
    getSelectedIndex: currentMonitorOptionIndex,
    getHighlightIndex: () => highlightIndex,
    setHighlightIndex: (index) => { highlightIndex = index; },
    isOpen: () => menuOpen,
    toggle: toggleMenu,
    close: closeMenu,
    select: (index) => {
      const monitor = monitors[index];
      if (monitor) pickMonitor(monitor.sig);
    },
    getMenu: () => menuEl,
    getTrigger: () => triggerEl,
  });

  $effect(() => {
    if (!menuOpen) return;
    return dropdownCloseEffect(menuEl, triggerEl, null, closeMenu, triggerEl);
  });

  onDestroy(() => positionController?.stop());
</script>

<span class="monitor-select-wrap monitor-select-{variant}" bind:this={wrapEl}>
  <button
    type="button"
    class="monitor-select-trigger"
    class:active={menuOpen}
    bind:this={triggerEl}
    aria-haspopup="listbox"
    aria-expanded={menuOpen}
    aria-label="Active monitor for per-monitor settings"
    onclick={toggleMenu}
    onkeydown={keyboard.onTriggerKeydown}
    onmouseenter={(e) => activeMonitor.mon && showHoverTip(e, activeMonitor.mon)}
    onmousemove={moveHoverTip}
    onmouseleave={hideHoverTip}>
    <span class="monitor-select-label">{label}</span>
    <span class="monitor-select-caret" aria-hidden="true"></span>
  </button>
  {#if menuOpen}
    <div
      class="dd-menu monitor-select-menu dropdown-menu-position-{menuPlacement}"
      bind:this={menuEl}
      role="listbox"
      aria-label="Active monitor for per-monitor settings"
      tabindex="-1"
      onkeydown={keyboard.onMenuKeydown}>
      {#each monitors as mon, index (mon.sig)}
        <button
          type="button"
          class="dd-menu-item"
          class:active={activeMonitor.mon?.sig === mon.sig}
          class:keyboard-highlighted={highlightIndex === index}
          data-dropdown-option-index={index}
          role="option"
          aria-selected={activeMonitor.mon?.sig === mon.sig}
          onfocus={() => keyboard.onItemFocus(index)}
          onmouseenter={(e) => showHoverTip(e, mon)}
          onmousemove={moveHoverTip}
          onmouseleave={hideHoverTip}
          onkeydown={keyboard.onItemKeydown}
          onclick={() => pickMonitor(mon.sig)}>
          {mon.friendly_name || `Monitor ${monitors.indexOf(mon) + 1}`}{mon.primary ? ' · Primary' : ''} · {mon.width}×{mon.height}
        </button>
      {/each}
    </div>
  {/if}
  {#if hoverTip.visible}
    <div
      class="monitor-hover-tip"
      role="tooltip"
      style="left:{hoverTip.x}px;top:{hoverTip.y}px;"
      aria-hidden="true">
      <span class="monitor-tip-title">
        {hoveredMonitor?.friendly_name || ''}
      </span>
      {#if hoveredMonitor?.primary && app.hdrStatus === 'hdr' && app.hdrLuminance?.maxNits}
        <span class="monitor-tip-row"><span>HDR max</span><strong>{fmtNits(app.hdrLuminance.maxNits)} nits</strong></span>
      {/if}
      {#if hoverTip.info}
        <span class="monitor-tip-row"><span>Connector</span><strong>{hoverTip.info.connector}</strong></span>
        <span class="monitor-tip-row"><span>Color format</span><strong>{hoverTip.info.colorFormat}</strong></span>
        <span class="monitor-tip-row"><span>Dynamic range</span><strong>{hoverTip.info.dynamicRange}</strong></span>
        <span class="monitor-tip-row"><span>VRR</span><strong>{hoverTip.info.vrr}</strong></span>
        <span class="monitor-tip-row"><span>Current</span><strong>{hoverTip.info.current}</strong></span>
      {:else}
        <span class="monitor-tip-row"><span>Loading</span><strong>…</strong></span>
      {/if}
    </div>
  {/if}
</span>
