<script>
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { onMount, onDestroy } from 'svelte';
  import TabWindowsFontIcon from './TabWindowsFontIcon.svelte';
  import { createDropdownPositionController } from './dropdown-position.js';

  // System font replacement (milestone M5). The backend builds impersonated
  // Segoe UI files and schedules a boot-time swap, so this tab is deliberately
  // minimal: pick a family, apply, reboot, or restore. Recommended families
  // (Inter, Roboto, …) can be downloaded on demand from Google Fonts instead
  // of requiring the user to install the static files.
  let fontReady = $state(false);
  let fontBusy = $state(false);
  let fontList = $state([]); // [{ family, path }], de-duplicated by family
  let selectedFontPath = $state('');
  let fontStatus = $state({ elevated: false, applied: null });
  let fontMessage = $state('');
  let fontError = $state('');
  let fontProgress = $state(''); // live phase text while download/apply runs
  let downloadCatalog = $state([]); // [{ key, family, license, filter }]
  let selectedDownloadKey = $state('');
  let unlistenFontProgress = null;

  // Module-level cache: the installed list, download catalog, and preview
  // sources are expensive to rebuild (font enumeration + Google Fonts
  // manifest fetch) but identical across tab visits, so they're fetched once
  // and reused on every re-open. Only the cheap status read (elevation check
  // + state file) runs each time, which keeps the "Currently applied" line
  // and the dropdowns near-instant on subsequent visits.
  let cachedFontData = null;

  onMount(async () => {
    // Fast path first: status is a local read, so it renders immediately
    // instead of waiting for the slow calls below.
    try {
      fontStatus = await invoke('font_get_status');
    } catch (e) {
      console.warn('[font] status failed', e);
    }
    if (!cachedFontData) {
      try {
        const [fonts, catalog, previews] = await Promise.all([
          invoke('font_list_installed'),
          invoke('font_download_catalog'),
          invoke('font_preview_sources'),
        ]);
        cachedFontData = {
          fonts: dedupeFonts(fonts),
          catalog: [...catalog].sort((a, b) => a.family.localeCompare(b.family)),
          previews,
        };
      } catch (e) {
        console.warn('[font] list/catalog failed', e);
      }
    }
    if (cachedFontData) {
      fontList = cachedFontData.fonts;
      downloadCatalog = cachedFontData.catalog;
      registerFontPreviews(cachedFontData.previews);
    }
    fontReady = true;
    // Live phase text from the backend while a font download/apply/restore
    // runs (that pipeline takes several seconds end to end).
    try {
      unlistenFontProgress = await listen('font-progress', (e) => {
        const p = e.payload;
        fontProgress = p?.detail ?? '';
      });
    } catch (e) {
      console.warn('[font] progress listen failed', e);
    }
  });

  onDestroy(() => {
    unlistenFontProgress?.();
  });

  // One entry per family; prefer the Regular (weight 400, non-italic) file as
  // the primary source for the weight-axis fallback.
  function dedupeFonts(fonts) {
    const map = new Map();
    for (const f of fonts) {
      if (!map.has(f.family)) {
        map.set(f.family, f.path);
      } else if (f.weight === 400 && !f.italic) {
        map.set(f.family, f.path);
      }
    }
    return [...map.entries()]
      .map(([family, path]) => ({ family, path }))
      .sort((a, b) => a.family.localeCompare(b.family));
  }

  // Quote a family name for a CSS font-family value, escaping any quotes the
  // family name itself contains (e.g. 'Comic Sans "Chalkboard" MS').
  function cssFontFamily(name) {
    return `"${String(name).replace(/["\\]/g, '\\$&')}"`;
  }

  // Register an @font-face for every catalog family (pointing at the Regular
  // static file's CDN URL) so the dropdown options and the sample line can
  // render real glyphs without downloading anything. The font is registered
  // under a unique `GF-<key>` name rather than the family's real name: a
  // catalog family that is also installed (e.g. Roboto) must never override
  // the system Roboto app-wide, the preview name only applies where we use
  // it. Offline/preview failures just leave those options in the default font.
  function registerFontPreviews(previews) {
    for (const p of previews ?? []) {
      try {
        const face = new FontFace(`GF-${p.key}`, `url(${p.regular_url})`);
        face.load().then(
          (loaded) => document.fonts.add(loaded),
          () => {}
        );
      } catch (e) {
        console.warn('[font] preview register failed', p.family, e);
      }
    }
  }

  // CSS font-family value for a catalog entry's preview: the unique GF-<key>
  // name registered above, so it never collides with an installed font.
  function catalogPreviewFont(key) {
    return cssFontFamily(`GF-${key}`);
  }

  // Trigger a UAC relaunch when the user interacts with a font dropdown while
  // not elevated, so they don't hit a disabled Apply button.
  async function requestFontElevation() {
    if (fontStatus.elevated || fontBusy) return;
    try {
      await invoke('relaunch_elevated', { tab: 'windows-font' });
    } catch (e) {
      fontError = String(e);
      console.warn('[font] elevation failed', e);
    }
  }

  // ── Font dropdowns ────────────────────────────────────────────────
  // The two font picks are custom .dd-menu lists instead of native <select>s:
  // the OS popup can't animate and gets clipped at the tab's scrollable
  // bottom edge. Each row renders the family in its own font (installed
  // families directly, catalog families via the registered GF-<key> face),
  // and the elevation prompt + mutual-exclusion behavior from the native
  // selects is preserved on the trigger buttons.
  let fontMenu = $state({
    installed: { open: false, el: null, wrap: null, placement: 'down-right' },
    download: { open: false, el: null, wrap: null, placement: 'down-right' },
  });
  let fontPositionControllers = { installed: null, download: null };

  // Trigger labels: the selected family name, or the placeholder text.
  const selectedInstalledFamily = $derived(
    fontList.find((f) => f.path === selectedFontPath)?.family ?? ''
  );
  const selectedDownloadFamily = $derived(
    downloadCatalog.find((c) => c.key === selectedDownloadKey)?.family ?? ''
  );

  function closeFontMenus() {
    for (const which of ['installed', 'download']) {
      fontMenu[which].open = false;
      fontPositionControllers[which]?.stop();
      fontPositionControllers[which] = null;
    }
  }

  function toggleFontMenu(which) {
    if (fontBusy || !fontReady) return;
    // Not elevated: prompt for a UAC relaunch instead of opening the list
    // (the Apply button would be disabled anyway).
    if (!fontStatus.elevated) {
      requestFontElevation();
      return;
    }
    const menu = fontMenu[which];
    menu.open = !menu.open;
    if (menu.open) {
      fontPositionControllers[which]?.stop();
      fontPositionControllers[which] = createDropdownPositionController({
        getTrigger: () => fontMenu[which].wrap?.querySelector('.font-select-trigger'),
        getMenu: () => fontMenu[which].el,
        onPlacement: (placement) => { fontMenu[which].placement = placement; },
        // The download dropdown is the row's right-side control, so its menu
        // stays anchored to the trigger's right edge even when the free space
        // to the left is narrower than the list (otherwise it would left-align
        // and hang off the right of its control).
        options: { gap: 6, viewportMargin: 14, maxHeight: 320, align: which === 'download' ? 'right' : undefined },
      });
      fontPositionControllers[which].start();
    } else {
      fontPositionControllers[which]?.stop();
      fontPositionControllers[which] = null;
    }
  }

  function selectInstalledFont(path) {
    closeFontMenus();
    selectedFontPath = path;
    onSelectInstalledFont();
  }

  function selectDownloadFont(key) {
    closeFontMenus();
    selectedDownloadKey = key;
    onSelectDownloadFont();
  }

  // Close either font menu on outside click, Escape, or window blur.
  $effect(() => {
    if (!fontMenu.installed.open && !fontMenu.download.open) return;
    function onDown(event) {
      for (const which of ['installed', 'download']) {
        const menu = fontMenu[which];
        if (menu.open && menu.wrap && !menu.wrap.contains(event.target)) {
          menu.open = false;
          fontPositionControllers[which]?.stop();
          fontPositionControllers[which] = null;
        }
      }
    }
    function onKey(event) {
      if (event.key === 'Escape') closeFontMenus();
    }
    function onBlur() {
      closeFontMenus();
    }
    window.addEventListener('pointerdown', onDown);
    window.addEventListener('keydown', onKey);
    window.addEventListener('blur', onBlur);
    return () => {
      window.removeEventListener('pointerdown', onDown);
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('blur', onBlur);
    };
  });

  // The family currently selected in either dropdown, for the sample line.
  // The two dropdowns are mutually exclusive, picking an installed font
  // clears the download selection and vice versa, so exactly one can drive
  // the preview (and an installed font never silently overrides a downloaded
  // one, since both can't be selected at once). `fontPreviewCss` is the CSS
  // font-family value for the preview line: the real family name for installed
  // fonts, the unique GF-<key> name for catalog fonts.
  let fontPreviewFamily = $state('');
  let fontPreviewCss = $state('');
  function onSelectInstalledFont() {
    selectedDownloadKey = '';
    updateFontPreviewFamily();
  }
  function onSelectDownloadFont() {
    selectedFontPath = '';
    updateFontPreviewFamily();
  }
  function updateFontPreviewFamily() {
    if (selectedFontPath) {
      const found = fontList.find((f) => f.path === selectedFontPath);
      fontPreviewFamily = found?.family ?? '';
      fontPreviewCss = found ? cssFontFamily(found.family) : '';
    } else if (selectedDownloadKey) {
      const found = downloadCatalog.find((c) => c.key === selectedDownloadKey);
      fontPreviewFamily = found?.family ?? '';
      fontPreviewCss = found ? catalogPreviewFont(found.key) : '';
    } else {
      fontPreviewFamily = '';
      fontPreviewCss = '';
    }
  }

  // The two dropdowns are mutually exclusive, so one button applies whichever
  // selection is active: an installed font or a downloaded catalog family.
  async function applySelected() {
    if (fontBusy || (!selectedFontPath && !selectedDownloadKey)) return;
    if (selectedDownloadKey) await applyDownloadedFont();
    else await applyFont();
  }

  async function applyFont() {
    if (fontBusy || !selectedFontPath) return;
    fontBusy = true;
    fontError = '';
    fontMessage = '';
    fontProgress = '';
    updateFontPreviewFamily();
    try {
      const state = await invoke('font_apply', { primaryPath: selectedFontPath });
      fontStatus = { ...fontStatus, applied: state, needsRestart: true };
      fontMessage = `Applied "${state.family}". Reboot required to take effect.`;
    } catch (e) {
      fontError = String(e);
      console.warn('[font] apply failed', e);
    } finally {
      fontBusy = false;
      fontProgress = '';
    }
  }

  async function restoreFont() {
    if (fontBusy) return;
    // Restoring schedules a boot-time swap back to the originals, which needs
    // admin like the apply, prompt for a UAC relaunch instead of hitting a
    // disabled button.
    if (!fontStatus.elevated) {
      requestFontElevation();
      return;
    }
    fontBusy = true;
    fontError = '';
    fontMessage = '';
    try {
      await invoke('font_restore');
      // Restore also stages a boot-time swap, so a restart is still needed.
      fontStatus = { ...fontStatus, applied: null, needsRestart: true };
      fontMessage = 'Restore scheduled. Reboot required to take effect.';
    } catch (e) {
      fontError = String(e);
      console.warn('[font] restore failed', e);
    } finally {
      fontBusy = false;
      fontProgress = '';
    }
  }

  // ── Restart System ────────────────────────────────────────────────
  // A font replacement only takes effect at the next boot, so once a font is
  // applied the tab offers a one-click restart. Destructive to other open
  // apps, so it's guarded by a confirmation modal (same conventions as the
  // Settings tab's reset-all confirm: outside click / Escape close, Tab
  // traps between the two buttons).
  let restartConfirmOpen = $state(false);
  let restartConfirmEl = $state(null);
  let restartBtnEl = $state(null);
  let restartError = $state('');

  function openRestartConfirm() {
    restartConfirmOpen = true;
  }
  function closeRestartConfirm() {
    restartConfirmOpen = false;
  }

  function onRestartKeydown(e) {
    if (e.key === 'Escape') {
      closeRestartConfirm();
      restartBtnEl?.focus();
      return;
    }
    if (e.key === 'Tab') {
      const cancel = restartConfirmEl?.querySelector('.modal-btn-cancel');
      const confirm = restartConfirmEl?.querySelector('.modal-btn-danger');
      if (!cancel || !confirm) return;
      if (e.shiftKey && document.activeElement === cancel) {
        e.preventDefault();
        confirm.focus();
      } else if (!e.shiftKey && document.activeElement === confirm) {
        e.preventDefault();
        cancel.focus();
      }
    }
  }
  $effect(() => {
    if (!restartConfirmOpen) return;
    function onDocPointerDown(e) {
      if (restartConfirmEl?.contains(e.target) || restartBtnEl?.contains(e.target)) return;
      closeRestartConfirm();
    }
    function onDocKey(e) {
      if (e.key === 'Escape') {
        closeRestartConfirm();
        restartBtnEl?.focus();
      }
    }
    const focusId = setTimeout(() => restartConfirmEl?.querySelector('.modal-btn-cancel')?.focus(), 0);
    document.addEventListener('pointerdown', onDocPointerDown);
    document.addEventListener('keydown', onDocKey);
    return () => {
      clearTimeout(focusId);
      document.removeEventListener('pointerdown', onDocPointerDown);
      document.removeEventListener('keydown', onDocKey);
    };
  });

  async function confirmRestart() {
    closeRestartConfirm();
    try {
      restartError = '';
      await invoke('system_restart');
    } catch (e) {
      restartError = String(e);
      console.warn('[font] restart failed', e);
    }
  }

  // Download the selected recommended family (static weight files) and apply
  // it, same pipeline as an installed font. The download needs no admin; the
  // apply step enforces it.
  async function applyDownloadedFont() {
    if (fontBusy || !selectedDownloadKey) return;
    fontBusy = true;
    fontError = '';
    fontMessage = '';
    fontProgress = '';
    updateFontPreviewFamily();
    try {
      const state = await invoke('font_download_apply', { familyKey: selectedDownloadKey });
      fontStatus = { ...fontStatus, applied: state, needsRestart: true };
      fontMessage = `Downloaded and applied "${state.family}". Reboot required to take effect.`;
    } catch (e) {
      fontError = String(e);
      console.warn('[font] download apply failed', e);
    } finally {
      fontBusy = false;
      fontProgress = '';
    }
  }
</script>

<div class="tab-body">
  <div class="tab-heading tab-heading-2line">
    <div class="tab-heading-toggle no-hover">
      <TabWindowsFontIcon class="tab-icon" />
      <div class="tab-heading-titles">
        <div class="tab-heading-title-row">
          <h2>Windows Font</h2>
          <span class="text-render-experimental-badge">Experimental</span>
        </div>
        <span class="subtitle">Replace the system UI font</span>
      </div>
    </div>
    <div class="tab-heading-spacer"></div>
    <button class="restore-defaults-btn" disabled={fontBusy || !fontStatus.applied} onclick={restoreFont}>
      ↻ Restore original fonts
    </button>
  </div>
  <div class="separator"></div>

  <div class="font-section">
    {#if !fontStatus.elevated}
      <p class="font-warning">Requires administrator rights. Choosing or restoring a font will prompt you to restart as administrator.</p>
    {/if}

    {#if fontStatus.applied}
      <p class="font-applied font-applied-top">Currently applied: {fontStatus.applied.family}</p>
    {/if}

    <div class="font-row">
      <span class="font-select-wrap" bind:this={fontMenu.installed.wrap}>
        <button
          type="button"
          class="settings-select font-select-trigger"
          aria-haspopup="listbox"
          aria-expanded={fontMenu.installed.open}
          disabled={!fontReady || fontBusy}
          onclick={() => toggleFontMenu('installed')}>
          <span class="font-select-label">{selectedInstalledFamily || 'Choose an installed font…'}</span>
          <span class="font-select-caret" aria-hidden="true"></span>
        </button>
        {#if fontMenu.installed.open}
          <div
            class="dd-menu dropdown-menu-position-{fontMenu.installed.placement}"
            bind:this={fontMenu.installed.el}
            role="listbox"
            aria-label="Choose an installed font">
            {#each fontList as f (f.path)}
              <button
                type="button"
                class="dd-menu-item"
                class:active={selectedFontPath === f.path}
                role="option"
                aria-selected={selectedFontPath === f.path}
                style="font-family: {cssFontFamily(f.family)}"
                onclick={() => selectInstalledFont(f.path)}>
                {f.family}
              </button>
            {/each}
          </div>
        {/if}
      </span>
      <span class="font-select-wrap" bind:this={fontMenu.download.wrap}>
        <button
          type="button"
          class="settings-select font-select-trigger"
          aria-haspopup="listbox"
          aria-expanded={fontMenu.download.open}
          disabled={!fontReady || fontBusy}
          onclick={() => toggleFontMenu('download')}>
          <span class="font-select-label">{selectedDownloadFamily || 'Or download a recommended font…'}</span>
          <span class="font-select-caret" aria-hidden="true"></span>
        </button>
        {#if fontMenu.download.open}
          <div
            class="dd-menu dropdown-menu-position-{fontMenu.download.placement}"
            bind:this={fontMenu.download.el}
            role="listbox"
            aria-label="Download a recommended font">
            {#each downloadCatalog as c (c.key)}
              <button
                type="button"
                class="dd-menu-item"
                class:active={selectedDownloadKey === c.key}
                role="option"
                aria-selected={selectedDownloadKey === c.key}
                style="font-family: {catalogPreviewFont(c.key)}"
                onclick={() => selectDownloadFont(c.key)}>
                {c.family}
              </button>
            {/each}
          </div>
        {/if}
      </span>
    </div>

    {#if fontPreviewFamily}
      <div class="font-preview-line">
        <span class="font-preview-label">Preview:</span>
        <div class="font-preview-sizes">
          {#each [8, 10, 12, 15] as size (size)}
            <div class="font-preview-size-row">
              <span class="font-preview-size-label">{size}</span>
              <span class="font-preview-text" style="font-family: {fontPreviewCss}; font-size: {size}px">
                The quick brown fox jumps over the lazy dog 0123456789
              </span>
            </div>
          {/each}
        </div>
      </div>
    {/if}

    <div class="font-row">
      <button class="font-btn font-btn-apply" disabled={fontBusy || !fontStatus.elevated || (!selectedFontPath && !selectedDownloadKey)} onclick={applySelected}>
        {fontBusy ? '…' : (selectedDownloadKey ? 'Download & apply' : 'Apply')}
      </button>
      {#if fontStatus.needsRestart}
        <button class="font-btn font-btn-restart" bind:this={restartBtnEl} disabled={fontBusy || !fontStatus.elevated} onclick={openRestartConfirm}>
          Restart System
        </button>
      {/if}
    </div>

    {#if fontProgress}
      <p class="font-progress" role="status">{fontProgress}</p>
    {/if}
    {#if fontMessage}
      <p class="font-message" role="status">{fontMessage}</p>
    {/if}
    {#if fontError}
      <p class="font-error" role="status">{fontError}</p>
    {/if}
    {#if restartError}
      <p class="font-error" role="status">{restartError}</p>
    {/if}
  </div>

  {#if restartConfirmOpen}
    <div class="modal-backdrop" role="presentation">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="restart-title" aria-describedby="restart-desc" tabindex="-1" onkeydown={onRestartKeydown} bind:this={restartConfirmEl}>
        <div class="modal-title" id="restart-title">Restart Windows?</div>
        <p class="modal-desc" id="restart-desc">
          Restarting is required for the font replacement to take effect. Save your work in other apps first.
        </p>
        <div class="modal-actions">
          <button class="modal-btn modal-btn-cancel" onclick={closeRestartConfirm}>Cancel</button>
          <button class="modal-btn modal-btn-danger" onclick={confirmRestart}>Restart now</button>
        </div>
      </div>
    </div>
  {/if}
</div>
