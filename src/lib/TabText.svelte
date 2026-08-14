<script>
  // Text rendering / subpixel tuning tab. Reads and writes the real Windows
  // ClearType settings (GDI + DirectWrite/WPF) through the text_rendering
  // commands in the Rust backend. Settings live in HKCU (per-user, no admin)
  // and persist in the Windows registry itself, so no app config is involved.
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import TabTextIcon from './TabTextIcon.svelte';
  import ModeBars from './ModeBars.svelte';
  import { app } from './stores.svelte.js';

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

  // The main window minimizes to the tray, so restoring replays the Windows
  // restore (maximize) animation. Hold the native preview hidden until that
  // animation has had time to finish so the text sample doesn't appear over
  // the transition. The hold is armed from the backend's window-visibility
  // events (animated restores) and extended by window resize events while it
  // is active.
  const PREVIEW_HOLD_MS = 350;
  let previewHoldUntil = 0;
  let previewHoldRetryTimer = null;
  // Timestamp of the last `window-restore-settled` event (0 = none yet). The
  // backend's visual settle detector emits it the moment the restore
  // animation finishes, which lets the preview appear exactly when the
  // animation ends instead of after a fixed delay. The fixed hold below stays
  // as a fallback in case the event is lost.
  let restoreSettledAt = 0;

  // Backend-driven show events (tray icon / tray menu, or the WM_SIZE
  // SIZE_RESTORED emit in lib.rs) stamp app.windowRestoredAt together with
  // whether the restore animates. Arm or clear the preview hold to match: an
  // animated restore (the window was minimized) must wait out the animation,
  // an instant show (close-to-tray) must not be delayed. Only fresh stamps
  // are honored, so switching back to this tab later never replays a hold
  // from an old restore.
  $effect(() => {
    const stamp = app.windowRestoredAt;
    if (!stamp || Date.now() - stamp > 2000) return;
    // A new restore is starting — forget a settle from a previous one so the
    // hold can arm again if needed.
    restoreSettledAt = 0;
    if (app.windowRestoreAnimated) {
      previewHoldUntil = Math.max(previewHoldUntil, stamp + PREVIEW_HOLD_MS);
    } else {
      previewHoldUntil = 0;
    }
    scheduleNativePreviewBounds();
  });

  async function refreshState() {
    try {
      const s = await invoke('text_rendering_get_state');
      state = { ...state, ...s };
      syncExplorerHook(state.effective_mode);
    } catch (e) {
      error = String(e);
      console.warn('[text] get_state failed', e);
    } finally {
      loaded = true;
    }
  }

  // Mirror the mode into Explorer. The shell hard-codes ClearType and ignores
  // the registry, so Grayscale/Off only reach Explorer through the injected
  // hook DLL (see src-tauri/src/explorer_hook.rs).
  function syncExplorerHook(mode) {
    invoke('explorer_hook_set_force_grayscale', {
      enabled: mode === 'grayscale' || mode === 'off',
    }).catch((e) => console.warn('[text] explorer hook failed', e));
  }

  async function applyMode(mode) {
    if (busy) return;
    busy = true;
    error = '';
    try {
      await      invoke('text_rendering_set_mode', { mode });
      state.effective_mode = mode;
      syncExplorerHook(mode);
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

  // Called on window move. Never starts a hold here: dragging the window must
  // keep the preview following live. Only extend a hold already in progress
  // (a restore animation moves the window through its path).
  function onWindowMoved() {
    if (Date.now() < previewHoldUntil) previewHoldUntil = Date.now() + PREVIEW_HOLD_MS;
    scheduleNativePreviewBounds();
  }

  // Called on window resize. Restore/maximize animations and manual resizes
  // all resize the window; hold the preview until the resize settles so it
  // never appears over (or jumps through) the transition.
  function onWindowResized() {
    previewHoldUntil = Date.now() + PREVIEW_HOLD_MS;
    scheduleNativePreviewBounds();
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
      // Wait out the window's restore/maximize animation before showing the
      // preview. Re-check once the hold expires so the sample appears right
      // after the transition instead of waiting for the next layout event.
      if (Date.now() < previewHoldUntil) {
        clearTimeout(previewHoldRetryTimer);
        previewHoldRetryTimer = setTimeout(() => {
          previewHoldRetryTimer = null;
          scheduleNativePreviewBounds();
        }, previewHoldUntil - Date.now());
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

    // Destroy-and-recreate lifecycle for the native preview. Hiding it (via
    // ShowWindow) is not enough: the preview is an owned popup and Windows
    // re-shows owned windows that were visible when their owner restores from
    // minimized — mid restore-animation, bypassing set_bounds. So on any
    // visibility loss the preview is destroyed entirely (there is then no
    // HWND Windows could auto-show), and recreated once the window is visible
    // again, after the restore animation has settled.
    const recreateNativePreview = () => {
      invoke('text_preview_show', {
        mode: state.effective_mode,
        gamma: state.gamma,
        contrast: state.contrast,
      })
        .then(() => {
          if (!modalOpen) scheduleNativePreviewBounds();
        })
        .catch((e) => console.warn('[text] native preview recreate failed', e));
    };

    // Backend-driven show/hide events (tray icon, tray menu, or the WM_SIZE
    // SIZE_RESTORED emit in lib.rs for taskbar restores). This is the primary
    // recreate trigger for a taskbar restore: document.visibilitychange does
    // NOT fire when a WebView2 host window is minimized/restored via the
    // taskbar (only for close-to-tray hides), so without this event the
    // mounted tab would never learn the window came back — the preview stays
    // destroyed until the user switches tabs (remount).
    let unlistenWindowVisibility = null;
    listen('window-visibility', (event) => {
      const payload = event.payload;
      const object = payload !== null && typeof payload === 'object';
      const visible = object ? payload.visible !== false : payload !== false;
      const animated = object ? payload.animated === true : false;
      if (!visible) {
        // Backend-driven hide (tray toggle minimize / close-to-tray). The tab
        // unmounts right after, but destroy immediately so no owned popup
        // survives even if the unmount is delayed.
        nativePreviewVisible = false;
        invoke('text_preview_destroy').catch(() => {});
      } else {
        // A show while this tab is still mounted means the window was
        // restored without unmounting us — the taskbar-restore-after-minimize
        // case. The restore animates (the backend only emits animated=true
        // for a genuine minimize→restore), so arm the hold and recreate. A
        // fresh window-restore-settled event clears the hold shortly after.
        if (animated) {
          restoreSettledAt = 0;
          previewHoldUntil = Math.max(previewHoldUntil, Date.now() + PREVIEW_HOLD_MS);
        }
        recreateNativePreview();
      }
    }).then((stop) => { unlistenWindowVisibility = stop; }).catch(() => {});

    // The backend's visual settle detector: the restore animation has
    // finished (or the timeout fallback fired). Stop waiting — show the
    // preview now, at the exact moment the animation ended.
    let unlistenRestoreSettled = null;
    listen('window-restore-settled', () => {
      restoreSettledAt = Date.now();
      previewHoldUntil = 0;
      scheduleNativePreviewBounds();
    }).then((stop) => { unlistenRestoreSettled = stop; }).catch(() => {});

    // Fallback destroy/recreate via document visibility. Unreliable for
    // taskbar minimize/restore in WebView2 (see above), but it does fire for
    // close-to-tray hides and costs nothing to keep.
    const onVisibilityChange = () => {
      if (document.visibilityState === 'hidden') {
        nativePreviewVisible = false;
        invoke('text_preview_destroy').catch(() => {});
      } else if (document.visibilityState === 'visible') {
        // A taskbar restore never fires window-visibility, so reaching this
        // handler with the tab still mounted means the window was minimized
        // and the restore animates — arm the hold here. (For backend-driven
        // shows the windowRestoredAt effect above overrides the hold to match
        // the real animated-vs-instant state.)
        if (Date.now() - (app.windowShownAt || 0) > 100) {
          restoreSettledAt = 0;
          previewHoldUntil = Math.max(previewHoldUntil, Date.now() + PREVIEW_HOLD_MS);
        }
        recreateNativePreview();
      }
    };
    document.addEventListener('visibilitychange', onVisibilityChange);

    getCurrentWindow().onMoved(onWindowMoved).then((unlisten) => {
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
    window.addEventListener('resize', onWindowResized);
    scrollParents.forEach((parent) => parent.addEventListener('scroll', onLayoutChange, { passive: true }));

    return () => {
      disposed = true;
      if (nativePreviewSyncRaf) cancelAnimationFrame(nativePreviewSyncRaf);
      if (previewHoldRetryTimer) clearTimeout(previewHoldRetryTimer);
      previewHoldRetryTimer = null;
      document.removeEventListener('visibilitychange', onVisibilityChange);
      nativePreviewResizeObserver?.disconnect();
      nativePreviewResizeObserver = null;
      window.removeEventListener('resize', onWindowResized);
      scrollParents.forEach((parent) => parent.removeEventListener('scroll', onLayoutChange));
      unlistenWindowMoved?.();
      unlistenWindowVisibility?.();
      unlistenRestoreSettled?.();
      // The tab unmounts when the window hides (close-to-tray, tray minimize)
      // or when the user switches tabs. Destroy the preview so no owned popup
      // survives to be auto-shown over the next restore animation; it is
      // recreated on the next mount.
      invoke('text_preview_destroy').catch(() => {});
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
