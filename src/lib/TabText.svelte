<script>
  // Text rendering / subpixel tuning tab. Reads and writes the real Windows
  // ClearType settings (GDI + DirectWrite/WPF) through the text_rendering
  // commands in the Rust backend. Settings live in HKCU (per-user, no admin)
  // and persist in the Windows registry itself, so no app config is involved.
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import TabTextIcon from './TabTextIcon.svelte';
  import ModeBars from './ModeBars.svelte';

  let { modalOpen = false } = $props();

  const MODES = [
    { id: 'off', label: 'Off' },
    { id: 'grayscale', label: 'Grayscale' },
    { id: 'rgb', label: 'RGB' },
    { id: 'bgr', label: 'BGR' },
  ];
  let state = $state({
    effective_mode: 'rgb',
    gamma: 1400,
    contrast: 1400,
    cleartype_level: 100,
    smoothing_enabled: true,
  });
  let loaded = $state(false);
  let busy = $state(false);
  let error = $state('');

  let contrastDragging = $state(false);
  let contrastHovering = $state(false);

  // Legacy refs retained while the native preview is being migrated; the
  // visible comparison samples are stable HTML text rather than canvases.
  let grayscaleCanvas = $state(null);
  let rgbCanvas = $state(null);
  let bgrCanvas = $state(null);
  let nativePreviewHost = $state(null);
  let nativePreviewResizeObserver = null;
  let nativePreviewVisible = false;
  let nativePreviewSyncRaf = 0;
  let nativePreviewBoundsSyncing = false;
  let nativePreviewBoundsPending = false;

  async function refreshState() {
    try {
      const s = await invoke('text_rendering_get_state');
      state = { ...state, ...s };
    } catch (e) {
      error = String(e);
      console.warn('[text] get_state failed', e);
    } finally {
      loaded = true;
    }
  }

  async function applyMode(mode) {
    if (busy) return;
    busy = true;
    error = '';
    try {
      await      invoke('text_rendering_set_mode', { mode });
      state.effective_mode = mode;
      await updateNativePreview();
    } catch (e) {
      error = String(e);
      console.warn('[text] set_mode failed', e);
    } finally {
      busy = false;
    }
  }

  // Applied immediately on every drag tick: the backend now only does a fast
  // SPI write + registry write and posts the desktop notification
  // asynchronously, so there is no need to debounce — the native preview
  // follows the thumb live.
  function applyContrast(value) {
    if (state.effective_mode === 'off') return;
    state.contrast = value;
    invoke('text_rendering_set_contrast', { contrast: value })
      .then(updateNativePreview)
      .catch((e) => {
        error = String(e);
        console.warn('[text] set_contrast failed', e);
      });
  }

  async function resetDefaults() {
    busy = true;
    error = '';
    try {
      await invoke('text_rendering_reset');
      await refreshState();
      await updateNativePreview();
    } catch (e) {
      error = String(e);
      console.warn('[text] reset failed', e);
    } finally {
      busy = false;
    }
  }

  function scheduleNativePreviewBounds() {
    if (nativePreviewSyncRaf) cancelAnimationFrame(nativePreviewSyncRaf);
    nativePreviewSyncRaf = requestAnimationFrame(() => {
      nativePreviewSyncRaf = 0;
      syncNativePreviewBounds();
    });
  }

  async function syncNativePreviewBounds() {
    if (nativePreviewBoundsSyncing) {
      nativePreviewBoundsPending = true;
      return;
    }
    nativePreviewBoundsSyncing = true;
    try {
      if (modalOpen) {
        if (nativePreviewVisible) {
          nativePreviewVisible = false;
          await invoke('text_preview_hide');
        }
        return;
      }
      if (!nativePreviewHost) return;
      const rect = nativePreviewHost.getBoundingClientRect();
      const viewport = nativePreviewHost.closest('.content')?.getBoundingClientRect();
      const left = viewport ? Math.max(rect.left, viewport.left) : rect.left;
      const top = viewport ? Math.max(rect.top, viewport.top) : rect.top;
      const right = viewport ? Math.min(rect.right, viewport.right) : rect.right;
      const bottom = viewport ? Math.min(rect.bottom, viewport.bottom) : rect.bottom;
      const width = right - left;
      const height = bottom - top;

      // The native preview is an owned popup rather than a WebView DOM node, so
      // explicitly clip it to the visible content viewport. This keeps it from
      // floating over the header, other tabs, or other applications after scroll.
      if (width <= 0 || height <= 0) {
        if (nativePreviewVisible) {
          nativePreviewVisible = false;
          await invoke('text_preview_hide');
        }
        return;
      }

      const scale = window.devicePixelRatio || 1;
      await invoke('text_preview_set_bounds', {
        x: Math.round(left * scale),
        y: Math.round(top * scale),
        width: Math.round(width * scale),
        height: Math.round(height * scale),
      });
      nativePreviewVisible = true;
    } catch (e) {
      console.warn('[text] native preview positioning failed', e);
    } finally {
      nativePreviewBoundsSyncing = false;
      if (nativePreviewBoundsPending) {
        nativePreviewBoundsPending = false;
        scheduleNativePreviewBounds();
      }
    }
  }

  $effect(() => {
    if (modalOpen) {
      nativePreviewVisible = false;
      invoke('text_preview_hide').catch(() => {});
    } else if (loaded) {
      scheduleNativePreviewBounds();
    }
  });

  async function updateNativePreview() {
    try {
      await invoke('text_preview_update', {
        mode: state.effective_mode,
        gamma: state.gamma,
        contrast: state.contrast,
      });
    } catch (e) {
      console.warn('[text] native preview update failed', e);
    }
  }

  // ── Subpixel preview simulation ───────────────────────────────────────
  // Renders the same text through an RGB / BGR / grayscale subpixel filter so
  // the user can compare color fringing without touching the OS. White text is
  // rasterized onto a transparent offscreen canvas at 3x horizontal scale and
  // its alpha channel is used as per-subpixel coverage.
  function renderPreview() {
    if (!grayscaleCanvas || !rgbCanvas || !bgrCanvas) return;
    const text = 'The quick brown fox';
    const fontSize = 18;
    const font = `${fontSize}px "Segoe UI", Inter, system-ui, sans-serif`;
    const measure = document.createElement('canvas').getContext('2d');
    measure.font = font;
    const tw = Math.ceil(measure.measureText(text).width);
    const th = Math.ceil(fontSize * 1.4);

    const scale = 3;
    const off = document.createElement('canvas');
    off.width = tw * scale;
    off.height = th;
    const octx = off.getContext('2d', { willReadFrequently: true });
    octx.scale(scale, 1);
    octx.font = font;
    octx.textBaseline = 'top';
    octx.fillStyle = '#fff';
    octx.fillText(text, 0, 3);
    const src = octx.getImageData(0, 0, tw * scale, th).data;

    const fg = [232, 232, 236];
    const bg = [17, 17, 21];

    function draw(mode, canvas) {
      canvas.width = tw;
      canvas.height = th;
      const ctx = canvas.getContext('2d');
      const img = ctx.createImageData(tw, th);
      const out = img.data;
      for (let y = 0; y < th; y++) {
        const row = y * tw * scale;
        for (let x = 0; x < tw; x++) {
          const a0 = src[(row + x * scale) * 4 + 3] / 255;
          const a1 = src[(row + x * scale + 1) * 4 + 3] / 255;
          const a2 = src[(row + x * scale + 2) * 4 + 3] / 255;
          let cr;
          let cg;
          let cb;
          if (mode === 'rgb') {
            cr = a0; cg = a1; cb = a2;
          } else if (mode === 'bgr') {
            cr = a2; cg = a1; cb = a0;
          } else {
            const l = (a0 + a1 + a2) / 3;
            cr = l; cg = l; cb = l;
          }
          const o = (y * tw + x) * 4;
          out[o] = Math.round(bg[0] + (fg[0] - bg[0]) * cr);
          out[o + 1] = Math.round(bg[1] + (fg[1] - bg[1]) * cg);
          out[o + 2] = Math.round(bg[2] + (fg[2] - bg[2]) * cb);
          out[o + 3] = 255;
        }
      }
      ctx.putImageData(img, 0, 0);
    }

    draw('grayscale', grayscaleCanvas);
    draw('rgb', rgbCanvas);
    draw('bgr', bgrCanvas);
  }

  onMount(() => {
    let disposed = false;
    let unlistenWindowMoved = null;
    const scrollParents = [
      nativePreviewHost?.closest('.content'),
      nativePreviewHost?.closest('.tab-body'),
    ].filter((element, index, elements) => element && elements.indexOf(element) === index);
    const onLayoutChange = () => scheduleNativePreviewBounds();

    getCurrentWindow().onMoved(onLayoutChange).then((unlisten) => {
      if (disposed) unlisten();
      else unlistenWindowMoved = unlisten;
    }).catch(() => {});

    refreshState()
      .then(async () => {
        if (disposed) return;
        try {
          await invoke('text_preview_show', {
            mode: state.effective_mode,
            gamma: state.gamma,
            contrast: state.contrast,
          });
          if (!modalOpen) scheduleNativePreviewBounds();
        } catch (e) {
          console.warn('[text] native preview unavailable', e);
        }
      });
    nativePreviewResizeObserver = new ResizeObserver(onLayoutChange);
    if (nativePreviewHost) nativePreviewResizeObserver.observe(nativePreviewHost);
    window.addEventListener('resize', onLayoutChange);
    scrollParents.forEach((parent) => parent.addEventListener('scroll', onLayoutChange, { passive: true }));

    return () => {
      disposed = true;
      if (nativePreviewSyncRaf) cancelAnimationFrame(nativePreviewSyncRaf);
      nativePreviewResizeObserver?.disconnect();
      nativePreviewResizeObserver = null;
      window.removeEventListener('resize', onLayoutChange);
      scrollParents.forEach((parent) => parent.removeEventListener('scroll', onLayoutChange));
      unlistenWindowMoved?.();
      invoke('text_preview_hide').catch(() => {});
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
    <button class="restore-defaults-btn" onclick={resetDefaults} disabled={busy}>↻ Restore Windows defaults</button>
  </div>
  <div class="separator"></div>

  <div class="dimmer-control">
    <div class="dimmer-control-heading">
      <span class="hdr-control-title">
        <span class="settings-row-label">Text rendering mode</span>
        <span class="info-tip-wrap">
          <button class="info-tip-btn" aria-label="About text rendering mode" tabindex="0">Info</button>
          <span class="info-tip info-tip-right">
            <span>Fix color fringing by changing Windows subpixel rendering.</span>
          </span>
        </span>
      </span>
    </div>
    <div class="text-render-inline-modes" role="radiogroup" aria-label="Preview subpixel mode">
      {#each MODES as m (m.id)}
        <button type="button" class="text-render-inline-mode" class:active={state.effective_mode === m.id} role="radio" aria-checked={state.effective_mode === m.id} onclick={() => applyMode(m.id)} disabled={busy}>
          {#if m.id !== 'off'}<ModeBars mode={m.id} class="text-render-mode-bars" />{/if}
          {m.label}
        </button>
      {/each}
    </div>
    <div class="text-render-preview-caption-row">
      <span class="text-render-preview-caption text-render-preview-caption-left">100% zoom</span>
      <span class="text-render-preview-caption text-render-preview-caption-right">400% zoom</span>
    </div>
    <div class="text-render-native-preview-host" bind:this={nativePreviewHost} role="img" aria-label="Live native Windows text rendering preview">
      <span class="text-render-native-preview-fallback">The quick brown fox jumps over the lazy dog</span>
    </div>
  </div>

  <div class="dimmer-control">
    <div class="dimmer-control-heading text-render-slider-heading">
      <span class="settings-row-label">Text contrast</span>
      <span class="slider-val">{state.contrast}</span>
    </div>
    <div class="slider-track-wrap dimmer-slider-track" role="presentation">
      <input type="range" min="1000" max="2200" step="100" value={state.contrast}
        style="--fill:{((state.contrast - 1000) / 1200) * 100}%"
        oninput={(e) => applyContrast(Math.round(Number(e.currentTarget.value) / 100) * 100)}
        onpointerdown={() => contrastDragging = true}
        onpointerup={() => contrastDragging = false}
        onpointercancel={() => contrastDragging = false}
        onpointerenter={() => contrastHovering = true}
        onpointerleave={() => contrastHovering = false}
        disabled={busy || state.effective_mode === 'off'} />
      <div class="slider-tooltip" class:visible={contrastDragging || contrastHovering} style="left:calc(9px + ({((state.contrast - 1000) / 1200)} * (100% - 18px)))">{state.contrast}</div>
    </div>
    <div class="dimmer-scale"><span>Darker</span><span>Lighter</span></div>
  </div>

  <div class="separator"></div>

  {#if error}
    <p class="text-render-error" role="alert">⚠ {error}</p>
  {/if}

</div>
