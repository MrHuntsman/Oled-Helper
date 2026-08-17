<script>
  // Text rendering / subpixel tuning tab. The registry-based ClearType method
  // is disabled; text now flows through the swash rasterizer hook (see
  // src-tauri/src/explorer_hook.rs and text-raster/). The preview renders that
  // same swash output, so the fringing shown matches what Explorer produces.
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import TabTextIcon from './TabTextIcon.svelte';
  import ModeBars from './ModeBars.svelte';
  import { textSettings, saveConfig, activeMonitor, syncTextPerMonitor, syncTextScope, BOLDNESS_MAX } from './stores.svelte.js';

  let { modalOpen = false, menuOpen = false } = $props();

  const MODES = [
    { id: 'off', label: 'No filter' },
    { id: 'grayscale', label: 'Grayscale' },
    { id: 'rgb', label: 'RGB' },
    { id: 'bgr', label: 'BGR' },
    { id: 'custom', label: 'Custom' },
  ];
  // Custom subpixel center slider bounds (±1 pixel) and step. The preview
  // updates live so tuning the offsets against the fringe is immediate.
  const CUSTOM_OFFSET_MIN = -1;
  const CUSTOM_OFFSET_MAX = 1;
  const CUSTOM_OFFSET_STEP = 0.01;
  // Sensible starting point for a fresh custom profile: the WOLED quad
  // positions (R/G/B across a 4-subpixel pixel, white derived by the panel),
  // horizontal-only so every y = 0.
  const CUSTOM_OFFSET_DEFAULT = { rx: -0.375, ry: 0, gx: -0.125, gy: 0, bx: 0.375, by: 0 };
  // The six Subpixel center axes (R/G/B × x/y) in slider order.
  const CUSTOM_AXES = [
    { key: 'rx', label: 'Red X' },
    { key: 'ry', label: 'Red Y' },
    { key: 'gx', label: 'Green X' },
    { key: 'gy', label: 'Green Y' },
    { key: 'bx', label: 'Blue X' },
    { key: 'by', label: 'Blue Y' },
  ];
  let state = $state({
    effective_mode: 'rgb',
    gamma: 1400,
    cleartype_level: 100,
    smoothing_enabled: true,
  });
  let loaded = $state(false);
  let busy = $state(false);
  let error = $state('');

  // Explorer hook state from the backend's explorer_hook_status command
  // (polled while mounted): active = DLL verified in the running explorer.exe.
  let hookStatus = $state({ enabled: false, active: false, applied: false });
  async function refreshHookStatus() {
    try {
      hookStatus = await invoke('explorer_hook_status');
    } catch (e) {
      console.warn('[text] explorer hook status failed', e);
    }
  }

  // Canvas the preview renders into. It shows the actual swash output (the
  // same engine the Explorer hook uses), so the fringing is real.
  let previewCanvas = $state(null);
  // Zoom level (%) for the right preview column, driven by the mini slider
  // above the canvas. Session-only, like the rest of the preview state.
  let previewZoom = $state(800);

  // Sync the persisted master toggle into the backend hook module so it
  // gates the hook (Explorer untouched when off).
  function syncExplorerToggle() {
    invoke('explorer_hook_set_enabled', { enabled: textSettings.explorerHookEnabled }).catch(() => {});
  }

  // The knob the sliders show: the selected monitor's per-monitor value.
  // Knobs are part of each monitor's profile; before the selector has a
  // monitor (pre-load) there is nothing to edit, so the value is 0. An
  // absent entry means "the defaults": lcd = 100 (full light filter),
  // boldness/damping = 0.
  function knobValue(field) {
    if (!activeMonitor.mon) return 0;
    const k = textSettings.perMonitorKnobs[activeMonitor.mon.sig];
    if (k) return k[field];
    return field === 'lcd' ? 100 : 0;
  }

  // Push the per-monitor knob assignments (and the mode assignments + enabled
  // flag they belong to) to the backend so Explorer applies them live.
  // Debounced at 33ms to match the preview render, so Explorer tracks the
  // drag without repainting per event.
  let knobSyncTimer = null;
  function scheduleKnobSync() {
    clearTimeout(knobSyncTimer);
    knobSyncTimer = setTimeout(() => syncTextPerMonitor(), 33);
  }

  // Shared handler for the tuning sliders: update the selected monitor's
  // knob value, persist, push to the hook (debounced), and re-render the
  // preview (debounced — a drag fires dozens of input events, and each
  // rasterization batch is a few backend calls, so rendering on the final
  // value keeps the UI responsive).
  // The boldness knob is stored 0..=30 but IS real pixels of stroke
  // thickening (swash emboldens the outline by boldness/100 px), so the
  // slider reads in px: 0.00..0.30, one notch = 0.05px. No "px" suffix —
  // it would need a wider value slot and shorten the slider vs its peers.
  function formatBoldness(v) {
    return (v / 100).toFixed(2);
  }

  function onKnobInput(field, value) {
    if (!activeMonitor.mon) return;
    const v = Math.max(0, Math.min(100, Math.round(Number(value) || 0)));
    const sig = activeMonitor.mon.sig;
    const k = textSettings.perMonitorKnobs[sig]
      || { lcd: 100, boldness: 0, damping: 0 };
    k[field] = v;
    // An all-default triplet (lcd 100 = full light filter, no bold, no
    // damping) is "no adjustment" — drop the entry so it does not silently
    // keep per-monitor resolution active. Note lcd 0 (no filter) is a real
    // setting, not "off": only the defaults clear.
    if (k.lcd === 100 && k.boldness === 0 && k.damping === 0) {
      delete textSettings.perMonitorKnobs[sig];
      // Dropping the local entry means the debounced sync loop won't iterate
      // it, so push an explicit all-default triplet to clear the hook's copy
      // too. Without this, a fast drag to 0 would leave Explorer at the old
      // value while the preview (driven by the store) already shows 0.
      invoke('explorer_hook_set_monitor_knobs', {
        sig,
        lcdStrength: 100,
        boldness: 0,
        damping: 0,
      }).catch(() => {});
    } else {
      textSettings.perMonitorKnobs[sig] = k;
    }
    saveConfig();
    scheduleKnobSync();
    schedulePreviewRender();
  }

  async function refreshState() {
    try {
      const s = await invoke('text_rendering_get_state');
      state = { ...state, ...s };
      syncExplorerToggle();
      scheduleKnobSync();
      syncExplorerHook(state.effective_mode);
      syncTextPerMonitor();
    } catch (e) {
      error = String(e);
      console.warn('[text] get_state failed', e);
    } finally {
      loaded = true;
    }
  }

  function syncExplorerHook(mode) {
    invoke('explorer_hook_set_mode', { mode })
      .then(refreshHookStatus)
      .catch((e) => console.warn('[text] explorer hook failed', e));
  }

  // ── Per-monitor text modes (M3) ───────────────────────────────────────
  // Every monitor carries its own profile; the top-bar selector (stores
  // activeMonitor) picks which one the mode control edits. A monitor without
  // an explicit mode follows the system text setting (the hook's fallback
  // slot, synced from text_rendering_get_state) — the base state, which the
  // radios reflect directly. The hook's per-monitor flag is derived in
  // stores from whether any assignment exists.

  // The mode the radios should show as active: the selected monitor's assigned
  // mode, or the system text setting when it has no override (the base state —
  // unconfigured monitors follow the system, which is RGB for most setups).
  // Restore defaults (the tab's reset button) clears overrides back to this.
  // Custom is not representable through the registry, so while scope is
  // registry-only a stored custom assignment displays as the system mode
  // (the radio group keeps a valid selection). The assignment itself is kept
  // so widening coverage restores it.
  function applicableMode(mode) {
    if (isRegistryOnly() && mode === 'custom') return state.effective_mode;
    return mode;
  }

  function shownMode() {
    if (activeMonitor.mon) return applicableMode(textSettings.perMonitorModes[activeMonitor.mon.sig] || state.effective_mode);
    return applicableMode(state.effective_mode);
  }

  // LCD filter and fringe reduction only affect subpixel rendering (see
  // text-raster RenderOptions); grayscale and No filter ignore them, so
  // those sliders only show for the subpixel modes. Boldness emboldens the
  // outline and works on every mode.
  function isSubpixelMode() {
    const m = shownMode();
    return m === 'rgb' || m === 'bgr' || m === 'custom';
  }

  // Assign the selected monitor's mode override.
  function applyMonitorMode(mode) {
    const sig = activeMonitor.mon.sig;
    textSettings.perMonitorModes[sig] = mode;
    // A fresh custom profile starts at the WOLED quad positions so the
    // preview renders something sane before the user drags the sliders.
    if (mode === 'custom' && !textSettings.perMonitorCustom[sig]) {
      textSettings.perMonitorCustom[sig] = { ...CUSTOM_OFFSET_DEFAULT };
    }
    // The primary monitor drives the system-wide DirectWrite/GDI text
    // setting: write the registry so DirectWrite apps follow it (the hook
    // only covers GDI). DirectWrite reads these values at process start, so
    // this reaches new app instances rather than live ones.
    if (activeMonitor.mon.primary) {
      invoke('text_rendering_set_mode', { mode })
        .catch((e) => console.warn('[text] registry mode sync failed', e));
    }
    saveConfig();
    syncTextPerMonitor();
    schedulePreviewRender();
  }

  // The custom subpixel centers for the selected monitor (or the defaults
  // when it has none).
  function customOffsets() {
    if (!activeMonitor.mon) return { ...CUSTOM_OFFSET_DEFAULT };
    return textSettings.perMonitorCustom[activeMonitor.mon.sig] || { ...CUSTOM_OFFSET_DEFAULT };
  }

  // One axis of the custom subpixel centers.
  function customValue(axis) {
    return customOffsets()[axis];
  }

  // Restore the filter knobs to "no adjustment" (the defaults: full light
  // filter, no bold, no damping) for the selected monitor. The debounced
  // sync only iterates existing entries, so an explicit all-default triplet
  // is pushed to clear the hook's copy too (same as the fast-drag-to-0 fix
  // in onKnobInput).
  function resetKnobs() {
    if (!activeMonitor.mon) return;
    const sig = activeMonitor.mon.sig;
    delete textSettings.perMonitorKnobs[sig];
    invoke('explorer_hook_set_monitor_knobs', {
      sig,
      lcdStrength: 100,
      boldness: 0,
      damping: 0,
    }).catch(() => {});
    saveConfig();
    scheduleKnobSync();
    schedulePreviewRender();
  }

  // Restore the six subpixel centers to the seeded default (the WOLED quad
  // positions) for the selected monitor: persist, push to the hook
  // (debounced), and re-render the preview.
  function resetCustomOffsets() {
    if (!activeMonitor.mon) return;
    const sig = activeMonitor.mon.sig;
    textSettings.perMonitorCustom[sig] = { ...CUSTOM_OFFSET_DEFAULT };
    saveConfig();
    scheduleKnobSync();
    schedulePreviewRender();
  }

  // Update one custom subpixel center, persist, push to the hook (debounced),
  // and re-render the preview.
  function onCustomInput(axis, value) {
    if (!activeMonitor.mon) return;
    const sig = activeMonitor.mon.sig;
    const v = Math.max(CUSTOM_OFFSET_MIN, Math.min(CUSTOM_OFFSET_MAX, Number(value) || 0));
    const offs = textSettings.perMonitorCustom[sig] || { ...CUSTOM_OFFSET_DEFAULT };
    offs[axis] = v;
    textSettings.perMonitorCustom[sig] = offs;
    saveConfig();
    scheduleKnobSync();
    schedulePreviewRender();
  }

  // The mode the preview rasterizes with: the selected monitor's override,
  // or the system default. Custom falls back to the system mode under
  // registry-only scope (see applicableMode).
  function previewMode() {
    if (activeMonitor.mon) return applicableMode(textSettings.perMonitorModes[activeMonitor.mon.sig] || state.effective_mode);
    return applicableMode(state.effective_mode);
  }

  // Coverage levels: registry-only (no hook), shell hosts, or every GUI
  // process. The three stops map onto the two persisted booleans
  // (explorerHookEnabled + hookAllApps), so the storage and backend are
  // unchanged — this is one control over existing state.
  function currentScope() {
    if (!textSettings.explorerHookEnabled) return 'registry';
    return textSettings.hookAllApps ? 'all' : 'shell';
  }

  // Registry-only scope: the registry path (GDI FontSmoothing* + DirectWrite
  // Avalon params) can express grayscale/RGB/BGR/off but NOT the 2D subpixel
  // offsets (Custom) or the swash filter knobs, so those controls hide.
  function isRegistryOnly() {
    return currentScope() === 'registry';
  }

  // Pick a coverage level and push the matching backend state. Registry
  // disables the hook entirely (mode 0 → passthrough); shell/all enable it
  // with the all-apps flag off/on. Scope reaches the DLL live (per-draw
  // guard), so already-running non-shell apps stop being re-rendered the
  // moment "shell" is picked.
  function setScope(scope) {
    if (scope === 'registry') {
      textSettings.explorerHookEnabled = false;
    } else {
      textSettings.explorerHookEnabled = true;
      textSettings.hookAllApps = scope === 'all';
      syncTextScope();
    }
    saveConfig();
    invoke('explorer_hook_set_enabled', { enabled: textSettings.explorerHookEnabled })
      .then(refreshHookStatus)
      .catch((e) => console.warn('[text] explorer hook toggle failed', e));
  }

  // Blocklist editor: comma/semicolon/newline-separated exe names, applied
  // on change (Enter or blur). Bare names get .exe appended; duplicates and
  // empty entries are dropped.
  let blocklistText = $state(textSettings.hookBlocklist.join(', '));
  function applyBlocklist() {
    const names = [...new Set(
      blocklistText.split(/[,;\n]/)
        .map((s) => s.trim().toLowerCase())
        .filter((s) => s.length > 0)
        .map((s) => (s.includes('.') ? s : `${s}.exe`)),
    )];
    textSettings.hookBlocklist = names;
    blocklistText = names.join(', ');
    syncTextScope();
    saveConfig();
  }

  async function applyMode(mode) {
    if (busy) return;
    // A monitor selected in the top bar means the mode edit targets that
    // monitor's profile; before the monitor list loads, it falls through to
    // the system-level mode below.
    if (activeMonitor.mon) {
      applyMonitorMode(mode);
      return;
    }
    busy = true;
    error = '';
    try {
      await invoke('text_rendering_set_mode', { mode });
      state.effective_mode = mode;
      syncExplorerHook(mode);
      await renderPreview();
    } catch (e) {
      error = String(e);
      console.warn('[text] set_mode failed', e);
    } finally {
      busy = false;
    }
  }

  async function resetDefaults() {
    busy = true;
    error = '';
    try {
      await invoke('text_rendering_reset');
      if (textSettings.explorerHookEnabled) {
        textSettings.explorerHookEnabled = false;
        saveConfig();
        syncExplorerToggle();
      }
      // Restore defaults also clears any per-monitor mode, knob, and custom
      // offset overrides.
      textSettings.perMonitorModes = {};
      textSettings.perMonitorKnobs = {};
      textSettings.perMonitorCustom = {};
      saveConfig();
      syncTextPerMonitor();
      await refreshState();
      await renderPreview();
    } catch (e) {
      error = String(e);
      console.warn('[text] reset failed', e);
    } finally {
      busy = false;
    }
  }

  // ── Swash preview ─────────────────────────────────────────────────────
  // Rasterizes sample text with the real swash engine (text_preview_render)
  // and composites the returned coverage onto a canvas. Left column at 100%,
  // right column at 400% so the subpixel fringing is visible.
  const PREVIEW_TEXT = 'The quick brown fox jumps over the lazy dog';
  // The right (zoomed) column shows the fringing-test characters instead of
  // the pangram: at 800% zoom only a handful of glyphs fit, and these five
  // are the best-to-worst fringe revealers (single-pixel stem l, dense M,
  // paired stems H, stem+flag 1, tiny !). Kept unspaced so they fit.
  const PREVIEW_TEXT_FRINGE = 'lMH1!';
  const PREVIEW_FG = [255, 255, 255];
  const PREVIEW_BG = [20, 20, 24];

  // Last rasterized sample bitmaps, keyed by (mode + knobs + dpr). Zoom only
  // changes how the SAME bitmaps are drawn, so a zoom or resize never needs a
  // backend round-trip; only a mode/knob change invalidates the cache.
  let previewCache = null;
  // Generation counter so a slow rasterization batch from an older render
  // cannot paint over a newer one that started meanwhile.
  let previewGen = 0;

  // Debounced re-render for slider drags (see onKnobInput). 33ms keeps
  // dragging smooth (≈30 fps) while avoiding the queue pile-up of a render
  // per input event.
  let previewRenderTimer = null;
  function schedulePreviewRender() {
    clearTimeout(previewRenderTimer);
    previewRenderTimer = setTimeout(() => renderPreview(), 33);
  }

  async function renderPreview() {
    const canvas = previewCanvas;
    if (!canvas || !loaded) return;
    const dpr = window.devicePixelRatio || 1;
    // Size the backing store from the canvas's OWN CSS box (width: 100%),
    // not the parent panel's client size — the panel is taller than the
    // canvas (caption row + padding), so using it as the source gives the
    // backing store a different aspect ratio than the displayed box and the
    // browser stretches the drawing to fit.
    const cssWidth = canvas.clientWidth;
    if (cssWidth <= 0) return;

    const mode = previewMode();
    const zoom = previewZoom / 100;
    const offs = customOffsets();
    const key = `${mode}|${knobValue('lcd')}|${knobValue('boldness')}|${knobValue('damping')}|${mode === 'custom' ? `${offs.rx}|${offs.ry}|${offs.gx}|${offs.gy}|${offs.bx}|${offs.by}` : ''}|${dpr}`;
    const gen = ++previewGen;

    let left;
    let right;
    if (previewCache && previewCache.key === key) {
      left = previewCache.left;
      right = previewCache.right;
    } else {
      try {
        // Rasterize at device-pixel density (size × dpr) so each bitmap pixel
        // maps to exactly one physical pixel when drawn back at CSS size — the
        // 100% column is crisp on HiDPI instead of half-resolution. On a 1×
        // display dpr = 1 and this is identical to before.
        const renderArgs = {
          mode,
          text: PREVIEW_TEXT,
          lcdStrength: knobValue('lcd'),
          boldness: knobValue('boldness'),
          damping: knobValue('damping'),
          offsetRX: mode === 'custom' ? offs.rx : null,
          offsetRY: mode === 'custom' ? offs.ry : null,
          offsetGX: mode === 'custom' ? offs.gx : null,
          offsetGY: mode === 'custom' ? offs.gy : null,
          offsetBX: mode === 'custom' ? offs.bx : null,
          offsetBY: mode === 'custom' ? offs.by : null,
        };
        left = await Promise.all(
          [8, 10, 12, 15].map((size) => invoke('text_preview_render', { ...renderArgs, size: size * dpr })),
        );
        right = await Promise.all(
          [15].map((size) => invoke('text_preview_render', { ...renderArgs, text: PREVIEW_TEXT_FRINGE, size: size * dpr })),
        );
      } catch (e) {
        console.warn('[text] preview render failed', e);
        return;
      }
      // A newer render started while the batch was in flight: drop this one
      // rather than paint stale bitmaps over the fresh state.
      if (gen !== previewGen) return;
      previewCache = { key, left, right };
    }

    const gap = 6;
    const leftTotal = left.reduce((a, r) => a + r.height / dpr, 0) + gap * (left.length - 1);
    const rightTotal = right.reduce((a, r) => a + (r.height * zoom) / dpr, 0);
    // Grow the canvas so the zoomed right column always fits; the tab body
    // scrolls, so a taller preview is fine. Setting the same value is a no-op.
    const targetH = Math.max(126, Math.ceil(Math.max(leftTotal, rightTotal)) + 4);
    if (parseInt(canvas.style.height || '0', 10) !== targetH) {
      canvas.style.height = targetH + 'px';
    }
    const cssHeight = canvas.clientHeight;
    // Clear + redraw only AFTER the new bitmaps are in hand, so the canvas
    // never sits blank during the async rasterization gap (the old preview
    // stays visible until the new one replaces it).
    canvas.width = Math.round(cssWidth * dpr);
    canvas.height = Math.round(cssHeight * dpr);
    const ctx = canvas.getContext('2d');
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = `rgb(${PREVIEW_BG[0]},${PREVIEW_BG[1]},${PREVIEW_BG[2]})`;
    ctx.fillRect(0, 0, cssWidth, cssHeight);

    const splitX = Math.floor(cssWidth / 2);
    const leftX = 10;
    const rightX = splitX + 12;

    // Left column (100%): stacked rows, vertically centered.
    let leftY = Math.max(0, (cssHeight - leftTotal) / 2);
    for (const r of left) {
      drawRun(ctx, r, leftX, leftY, 1);
      leftY += r.height / dpr + gap;
    }

    // Right column (slider zoom): the largest size only.
    let rightY = Math.max(0, (cssHeight - rightTotal) / 2);
    for (const r of right) {
      drawRun(ctx, r, rightX, rightY, zoom);
      rightY += (r.height * zoom) / dpr;
    }
  }

  function drawRun(ctx, run, x, y, zoom) {
    const w = run.width;
    const h = run.height;
    const subpixel = run.content === 'subpixel';
    const bpp = subpixel ? 4 : 1;
    const data = run.data;
    const off = document.createElement('canvas');
    off.width = w;
    off.height = h;
    const octx = off.getContext('2d');
    const img = octx.createImageData(w, h);
    const out = img.data;
    for (let py = 0; py < h; py++) {
      for (let px = 0; px < w; px++) {
        const s = (py * w + px) * bpp;
        let cr;
        let cg;
        let cb;
        if (subpixel) {
          cr = data[s] / 255;
          cg = data[s + 1] / 255;
          cb = data[s + 2] / 255;
        } else {
          const c = data[s] / 255;
          cr = c;
          cg = c;
          cb = c;
        }
        const o = (py * w + px) * 4;
        out[o] = Math.round(PREVIEW_BG[0] + (PREVIEW_FG[0] - PREVIEW_BG[0]) * cr);
        out[o + 1] = Math.round(PREVIEW_BG[1] + (PREVIEW_FG[1] - PREVIEW_BG[1]) * cg);
        out[o + 2] = Math.round(PREVIEW_BG[2] + (PREVIEW_FG[2] - PREVIEW_BG[2]) * cb);
        out[o + 3] = 255;
      }
    }
    octx.putImageData(img, 0, 0);
    // Bitmaps are rasterized at device density (size × dpr), so dividing by
    // dpr maps each bitmap pixel to one physical pixel. At zoom=1 that's a
    // crisp 100%; at zoom=4 each bitmap pixel becomes 4 CSS px (the fringing
    // inspection view).
    const dpr = window.devicePixelRatio || 1;
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(off, x, y, (w * zoom) / dpr, (h * zoom) / dpr);
  }

  onMount(() => {
    let disposed = false;
    let resizeObserver = null;

    refreshState().then(async () => {
      if (disposed) return;
      await renderPreview();
    });

    // Ensure the hook's per-monitor table matches the top-bar selector
    // (the flag is derived from the selected monitor).
    syncTextPerMonitor();
    refreshHookStatus();
    const hookStatusPoll = setInterval(refreshHookStatus, 2000);

    resizeObserver = new ResizeObserver(() => renderPreview());
    if (previewCanvas?.parentElement) resizeObserver.observe(previewCanvas.parentElement);

    return () => {
      disposed = true;
      clearInterval(hookStatusPoll);
      resizeObserver?.disconnect();
    };
  });
</script>

<div class="tab-body">
  <div class="tab-heading tab-heading-2line">
    <div class="tab-heading-toggle no-hover">
      <TabTextIcon class="tab-icon" />
      <div class="tab-heading-titles">
        <div class="tab-heading-title-row">
          <h2>Text rendering</h2>
        </div>
        <span class="subtitle">ClearType &amp; font smoothing</span>
      </div>
    </div>
    <div class="tab-heading-spacer"></div>
    <button class="restore-defaults-btn" onclick={resetDefaults} disabled={busy}>↻ Restore defaults</button>
  </div>
  <div class="separator"></div>    <div class="dimmer-control">
      <div class="dimmer-control-heading">
        <span class="hdr-control-title">
          <span class="settings-row-label">Text coverage</span>
          <span class="info-tip-wrap">
            <button class="info-tip-btn" aria-label="About text coverage" tabindex="0">Info</button>
            <span class="info-tip info-tip-right">
              <span>
                Where the selected mode is applied:<br>
                <br>
                <b>Registry only</b> — apps that follow Windows text settings (browsers, DirectWrite apps on their next start).<br>
                <b>Shell apps</b> — adds live re-rendering for Explorer, Control Panel and MMC.<br>
                <b>All apps</b> — hooks every GDI process (2D remap + filters); browsers and games aren't affected.
              </span>
            </span>
          </span>
        </span>
      </div>
      <p class="text-render-explorer-warning">⚠ May trigger antivirus false positives.</p>
      <div class="text-render-scope-options" role="radiogroup" aria-label="Text rendering coverage">
        <button type="button" role="radio" aria-checked={currentScope() === 'registry'} class="text-render-inline-mode" class:active={currentScope() === 'registry'} onclick={() => setScope('registry')}>Registry only</button>
        <button type="button" role="radio" aria-checked={currentScope() === 'shell'} class="text-render-inline-mode" class:active={currentScope() === 'shell'} onclick={() => setScope('shell')}>Shell apps</button>
        <button type="button" role="radio" aria-checked={currentScope() === 'all'} class="text-render-inline-mode" class:active={currentScope() === 'all'} onclick={() => setScope('all')}>All apps</button>
      </div>
    {#if currentScope() === 'all'}
      <label class="text-render-blocklist">
        <span class="text-render-explorer-toggle-label">Excluded apps</span>
        <input type="text" class="text-render-blocklist-input" placeholder="e.g. notepad.exe, winrar"
          bind:value={blocklistText} onchange={applyBlocklist} />
      </label>
    {/if}
    {#if hookStatus.active}
      <span class="restart-explorer-hint">Note: Current open windows will need a restart.</span>
    {/if}
  </div>

  <div class="dimmer-control">
    <div class="dimmer-control-heading">
      <span class="hdr-control-title">
        <span class="settings-row-label">Text rendering mode</span>
        <span class="info-tip-wrap">
          <button class="info-tip-btn" aria-label="About text rendering mode" tabindex="0">Info</button>
          <span class="info-tip info-tip-right">
            <span>
              Sets the subpixel mode for GDI and DirectWrite text pipelines.<br>
              <br>
              Apps that cache their fonts like Chromium need a restart.
            </span>
          </span>
        </span>
      </span>
    </div>
    <div class="text-render-modes-row">
      <div class="text-render-inline-modes" role="radiogroup" aria-label="Preview subpixel mode">
        {#each MODES as m (m.id)}
          {#if !(isRegistryOnly() && m.id === 'custom')}
            <button type="button" class="text-render-inline-mode" class:active={shownMode() === m.id} role="radio" aria-checked={shownMode() === m.id} onclick={() => applyMode(m.id)} disabled={busy}>
              {#if m.id !== 'off'}<ModeBars mode={m.id} class="text-render-mode-bars" />{/if}
              {m.label}
              {#if m.id === 'grayscale'}<span class="text-render-recommended-badge">Recommended</span>{/if}
            </button>
          {/if}
        {/each}
      </div>
    </div>
  </div>

  <div class="text-render-preview-panel">
    <div class="text-render-preview-caption-row">
      <span class="text-render-preview-caption text-render-preview-caption-left">100% zoom</span>
      <span class="text-render-preview-caption text-render-preview-caption-right">{previewZoom}% zoom</span>
    </div>
    <canvas class="text-render-preview-canvas" bind:this={previewCanvas} role="img" aria-label="Live text rendering preview"></canvas>
    {#if !isRegistryOnly() && shownMode() !== 'off'}
    <div class="text-render-knobs-columns" class:single={shownMode() !== 'custom'}>
      <div class="text-render-knobs">
        <div class="text-render-custom-heading-row">
          <div class="text-render-custom-heading">Filters</div>
          <button type="button" class="text-render-custom-reset" onclick={resetKnobs}>↻ Reset</button>
        </div>
      {#if isSubpixelMode()}
      <div class="text-render-knob-row">
        <span class="text-render-knob-label">LCD filter</span>
        <div class="slider-track-wrap slider-track-wrap-sm" role="presentation">
          <input type="range" class="slider-sm" min="0" max="100" step="5" value={knobValue('lcd')}
            style="--fill:{knobValue('lcd')}%"
            oninput={(e) => onKnobInput('lcd', e.currentTarget.value)} />
        </div>
        <span class="slider-val">{knobValue('lcd')}</span>
      </div>
      {/if}
      <div class="text-render-knob-row">
        <span class="text-render-knob-label">Boldness</span>
        <div class="slider-track-wrap slider-track-wrap-sm" role="presentation">
          <input type="range" class="slider-sm has-ticks-bold" min="0" max={BOLDNESS_MAX} step="5" value={knobValue('boldness')}
            style="--fill:{(knobValue('boldness') / BOLDNESS_MAX) * 100}%"
            oninput={(e) => onKnobInput('boldness', e.currentTarget.value)} />
        </div>
        <span class="slider-val">{formatBoldness(knobValue('boldness'))}</span>
      </div>
      {#if isSubpixelMode()}
      <div class="text-render-knob-row">
        <span class="text-render-knob-label">Fringe reduction</span>
        <div class="slider-track-wrap slider-track-wrap-sm" role="presentation">
          <input type="range" class="slider-sm" min="0" max="100" step="5" value={knobValue('damping')}
            style="--fill:{knobValue('damping')}%"
            oninput={(e) => onKnobInput('damping', e.currentTarget.value)} />
        </div>
        <span class="slider-val">{knobValue('damping')}</span>
      </div>
      {/if}
      </div>
    {#if shownMode() === 'custom'}
      <div class="text-render-custom-offsets">
        <div class="text-render-custom-heading-row">
          <div class="text-render-custom-heading">Subpixel centers</div>
          <button type="button" class="text-render-custom-reset" onclick={resetCustomOffsets}>↻ Reset</button>
        </div>
        {#each CUSTOM_AXES as axis (axis.key)}
          <div class="text-render-knob-row">
            <span class="text-render-knob-label">{axis.label}</span>
            <div class="slider-track-wrap slider-track-wrap-sm" role="presentation">
              <input type="range" class="slider-sm" min={CUSTOM_OFFSET_MIN} max={CUSTOM_OFFSET_MAX} step={CUSTOM_OFFSET_STEP} value={customValue(axis.key)}
                style="--fill:{((customValue(axis.key) - CUSTOM_OFFSET_MIN) / (CUSTOM_OFFSET_MAX - CUSTOM_OFFSET_MIN)) * 100}%"
                oninput={(e) => onCustomInput(axis.key, e.currentTarget.value)} />
            </div>
            <span class="slider-val">{customValue(axis.key).toFixed(3)}</span>
          </div>
        {/each}
      </div>
    {/if}
    </div>
    {/if}
  </div>

  <div class="separator"></div>

  {#if error}
    <p class="text-render-error" role="alert">⚠ {error}</p>
  {/if}

</div>
