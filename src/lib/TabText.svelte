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
  import { app, textSettings, saveConfig } from './stores.svelte.js';

  let { modalOpen = false, menuOpen = false } = $props();

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
  // Coalescing for the system-wide contrast write: dragging fires an oninput
  // per frame, and each write is an SPI call + registry write + WM_SETTINGCHANGE
  // broadcast to every top-level window. The preview follows live via
  // text_preview_update (a cheap repaint); the real write is flushed at most
  // once per CONTRAST_APPLY_MS with the latest value.
  const CONTRAST_APPLY_MS = 80;
  let contrastApplyTimer = null;
  let contrastPending = null;

  // Explorer hook state from the backend's explorer_hook_status command
  // (polled while mounted): active = DLL verified in the running explorer.exe;
  // applied = active AND the current explorer.exe generation was injected and
  // verified, so every window in it renders through the hook. Fonts are
  // rasterized at window creation, so a mode change only reaches already-open
  // windows after they are closed or Explorer restarts.
  let hookStatus = $state({ enabled: false, active: false, applied: false });
  async function refreshHookStatus() {
    try {
      hookStatus = await invoke('explorer_hook_status');
    } catch (e) {
      console.warn('[text] explorer hook status failed', e);
    }
  }

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
  // True while a restore/maximize animation is in progress. Manual resizes
  // must keep the preview following live, so the hold is only extended while
  // this is set (the animation resizes the window through its path). It is
  // armed wherever the hold is armed by a restore indicator and cleared by
  // window-restore-settled (which always fires, via the pixel-watch or its
  // 1.2s timeout fallback).
  let restoreInProgress = false;
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
      restoreInProgress = true;
      previewHoldUntil = Math.max(previewHoldUntil, stamp + PREVIEW_HOLD_MS);
    } else {
      restoreInProgress = false;
      previewHoldUntil = 0;
    }
    scheduleNativePreviewBounds();
  });

  // Sync the persisted master toggle into the backend hook module so it
  // gates injection (Explorer untouched when off).
  function syncExplorerToggle() {
    invoke('explorer_hook_set_enabled', { enabled: textSettings.explorerHookEnabled }).catch(() => {});
  }

  async function refreshState() {
    try {
      const s = await invoke('text_rendering_get_state');
      state = { ...state, ...s };
      syncExplorerToggle();
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
  // hook DLL (see src-tauri/src/explorer_hook.rs): grayscale → force
  // grayscale, off → force non-antialiased (crisp), rgb/bgr → pass through.
  function syncExplorerHook(mode) {
    invoke('explorer_hook_set_mode', { mode })
      .then(refreshHookStatus)
      .catch((e) => console.warn('[text] explorer hook failed', e));
  }

  // Master toggle: on → inject the hook and follow the mode; off → stop
  // injecting and write passthrough so Explorer returns to system ClearType.
  function onToggleExplorerHook() {
    textSettings.explorerHookEnabled = !textSettings.explorerHookEnabled;
    saveConfig();
    invoke('explorer_hook_set_enabled', { enabled: textSettings.explorerHookEnabled })
      .then(refreshHookStatus)
      .catch((e) => console.warn('[text] explorer hook toggle failed', e));
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

  // Contrast is a ClearType-only setting: grayscale and off text has no color
  // fringing to intensify, so the control is disabled (and ignored) outside
  // RGB/BGR.
  function applyContrast(value) {
    if (state.effective_mode !== 'rgb' && state.effective_mode !== 'bgr') return;
    state.contrast = value;
    // Preview follows the thumb every tick — this is a cheap repaint of the
    // native window, not a system settings change.
    invoke('text_preview_update', {
      mode: state.effective_mode,
      gamma: state.gamma,
      contrast: value,
    }).catch((e) => console.warn('[text] native preview update failed', e));
    // Coalesce the real write: at most one in flight per CONTRAST_APPLY_MS,
    // always carrying the latest value so the final thumb position lands.
    contrastPending = value;
    if (contrastApplyTimer === null) {
      contrastApplyTimer = setTimeout(() => {
        contrastApplyTimer = null;
        const pending = contrastPending;
        contrastPending = null;
        if (pending !== null) {
          invoke('text_rendering_set_contrast', { contrast: pending })
            // The preview is a real GDI window: it renders with the system's
            // currently applied settings, so it only shows the new value
            // AFTER the write lands. Repaint it post-write or the preview
            // stays one step behind (drag and keyboard behave differently).
            .then(() => updateNativePreview())
            .catch((e) => {
              error = String(e);
              console.warn('[text] set_contrast failed', e);
            });
        }
      }, CONTRAST_APPLY_MS);
    }
  }

  async function resetDefaults() {
    busy = true;
    error = '';
    try {
      await invoke('text_rendering_reset');
      // Restore the Explorer hook toggle to its (off) default too.
      if (textSettings.explorerHookEnabled) {
        textSettings.explorerHookEnabled = false;
        saveConfig();
        syncExplorerToggle();
      }
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

  // Called on window resize. Only a restore/maximize animation needs the
  // preview held back; a manual resize must keep it following live, so never
  // start a hold here — just extend one already in progress (the animation
  // resizes the window through its path).
  function onWindowResized() {
    if (restoreInProgress) previewHoldUntil = Date.now() + PREVIEW_HOLD_MS;
    scheduleNativePreviewBounds();
  }

  async function syncNativePreviewBounds() {
    if (nativePreviewBoundsSyncing) {
      nativePreviewBoundsPending = true;
      return;
    }
    nativePreviewBoundsSyncing = true;
    try {
      // Hide while a modal or a bottom-panel dropdown/context menu is open:
      // the preview is a separate popup HWND floating above the WebView, so
      // HTML menus that open over it would be drawn underneath (cut off).
      // With the preview hidden the menu renders fully; it reappears when
      // the menu closes.
      if (modalOpen || menuOpen) {
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
    if (modalOpen || menuOpen) {
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
          if (!modalOpen && !menuOpen) scheduleNativePreviewBounds();
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
          restoreInProgress = true;
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
      restoreInProgress = false;
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
          restoreInProgress = true;
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

    // Poll the hook status while mounted so the Explorer status reflects
    // reality (toggle, mode changes, Explorer restarts, watchdog
    // re-injection) without wiring every event path.
    refreshHookStatus();
    const hookStatusPoll = setInterval(refreshHookStatus, 2000);

    return () => {
      disposed = true;
      if (contrastApplyTimer) clearTimeout(contrastApplyTimer);
      contrastApplyTimer = null;
      clearInterval(hookStatusPoll);
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
    <button class="restore-defaults-btn" onclick={resetDefaults} disabled={busy}>↻ Restore defaults</button>
  </div>
  <div class="separator"></div>

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
          <button type="button" class="text-render-inline-mode" class:active={state.effective_mode === m.id} role="radio" aria-checked={state.effective_mode === m.id} onclick={() => applyMode(m.id)} disabled={busy}>
            {#if m.id !== 'off'}<ModeBars mode={m.id} class="text-render-mode-bars" />{/if}
            {m.label}
            {#if m.id === 'grayscale'}<span class="text-render-recommended-badge">Recommended</span>{/if}
          </button>
        {/each}
      </div>
      {#if state.effective_mode === 'rgb' || state.effective_mode === 'bgr'}
        <div class="text-render-mini-contrast">
          <span class="text-render-mini-contrast-label">Contrast</span>
          <div class="slider-track-wrap slider-track-wrap-sm" role="presentation">
            <input type="range" class="slider-sm" min="1000" max="2200" step="100" value={state.contrast}
              style="--fill:{((state.contrast - 1000) / 1200) * 100}%"
              oninput={(e) => applyContrast(Math.round(Number(e.currentTarget.value) / 100) * 100)}
              onpointerdown={() => contrastDragging = true}
              onpointerup={() => contrastDragging = false}
              onpointercancel={() => contrastDragging = false}
              onpointerenter={() => contrastHovering = true}
              onpointerleave={() => contrastHovering = false} />
            <div class="slider-tooltip" class:visible={contrastDragging || contrastHovering} style="left:calc(9px + ({((state.contrast - 1000) / 1200)} * (100% - 18px)))">{state.contrast}</div>
          </div>
          <span class="slider-val">{state.contrast}</span>
        </div>
      {/if}
    </div>
  </div>

  <div class="text-render-preview-panel">
    <div class="text-render-preview-caption-row">
      <span class="text-render-preview-caption text-render-preview-caption-left">100% zoom</span>
      <span class="text-render-preview-caption text-render-preview-caption-right">400% zoom</span>
    </div>
    <div class="text-render-native-preview-host" bind:this={nativePreviewHost} role="img" aria-label="Live native Windows text rendering preview">
      <span class="text-render-native-preview-fallback">
        {menuOpen || modalOpen
          ? 'The live text preview is hidden while a menu is open.'
          : 'Loading live text preview…'}
      </span>
    </div>
  </div>

  <div class="dimmer-control">
    <div class="dimmer-control-heading">
      <span class="hdr-control-title">
        <span class="settings-row-label">Windows Explorer</span>
      </span>
    </div>
    <p class="text-render-explorer-desc">Windows Explorer ignores grayscale and always renders its text with ClearType RGB.<br>This fix injects a hook into explorer.exe and forces grayscale rendering on Windows Explorer.</p>
    <p class="text-render-explorer-warning">⚠ It could trigger a false positive in your antivirus.</p>
    <div class="text-render-explorer-actions">
      <label class="text-render-explorer-toggle">
        <input type="checkbox" checked={textSettings.explorerHookEnabled} onchange={onToggleExplorerHook} />
        <span class="text-render-explorer-toggle-label">Fix Windows Explorer</span>
      </label>
      {#if hookStatus.active && hookStatus.applied}
        <span class="restart-explorer-status">Fix applied <span class="restart-explorer-check">✓</span></span>
      {:else if hookStatus.active}
        <span class="restart-explorer-hint">Applies to new Explorer windows. Already opened windows will need a restart.</span>
      {/if}
    </div>
  </div>

  <div class="separator"></div>

  {#if error}
    <p class="text-render-error" role="alert">⚠ {error}</p>
  {/if}

</div>
