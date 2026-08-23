<script>
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { onMount, onDestroy } from 'svelte';
  import TabWindowsFontIcon from './TabWindowsFontIcon.svelte';
  import ConfirmModal from './ConfirmModal.svelte';
  import { createDropdownPositionController } from './dropdown-position.js';
  import { createDropdownKeyboardHandlers } from './dropdown.js';

  // System font replacement (milestone M5). The backend builds impersonated
  // Segoe UI files and schedules a boot-time swap, so this tab is deliberately
  // minimal: pick a family, apply, reboot, or restore. Recommended families
  // (Inter, Roboto, …) can be downloaded on demand from Google Fonts instead
  // of requiring the user to install the static files.
  let fontReady = $state(false);
  let fontBusy = $state(false);

  // Confirmation for the destructive "Restore original fonts" action.
  let restoreConfirmOpen = $state(false);
  let fontList = $state([]); // [{ family, path }], de-duplicated by family
  let selectedFontPath = $state('');
  let fontStatus = $state({ elevated: false, applied: null });
  let fontMessage = $state('');
  let fontError = $state('');
  let fontProgress = $state(''); // live phase text while download/apply runs
  // True only right after an actual apply (until the selection changes or
  // the tab is reopened). The button reads "Applied ✓" just for this window;
  // the auto-selected applied font on its own must not show it forever.
  let fontJustApplied = $state(false);
  let downloadCatalog = $state([]); // [{ key, family, license, description, filter }]
  let selectedDownloadKey = $state('');
  let unlistenFontProgress = null;

  // Module-level cache: the installed list, download catalog, and preview
  // sources are expensive to rebuild (font enumeration + Google Fonts
  // manifest fetch) but identical across tab visits, so they're fetched once
  // and reused on every re-open. Only the cheap status read (elevation check
  // + state file) runs each time, which keeps the status and dropdowns
  // near-instant on subsequent visits.
  let cachedFontData = null;

  onMount(async () => {
    // Fast path first: status is a local read, so it renders immediately
    // instead of waiting for the slow calls below. The backend derives
    // needsRestart from whether a boot-time swap is still staged, so the
    // restart prompt survives tab switches and app restarts until reboot.
    try {
      const status = await invoke('font_get_status');
      fontStatus = {
        elevated: status.elevated,
        applied: status.applied,
        needsRestart: status.needs_restart === true,
      };
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
      syncSelectionToAppliedFont();
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

  // Trigger a UAC relaunch when a font operation needs administrator rights.
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
  // Custom dropdowns keep each installed and downloadable family rendered in
  // its own typeface, while the shared keyboard helper preserves native select
  // navigation behavior.
  let fontMenu = $state({
    installed: { open: false, el: null, wrap: null, placement: 'down-right', highlightIndex: -1 },
    download: { open: false, el: null, wrap: null, placement: 'down-right', highlightIndex: -1 },
  });
  let fontPositionControllers = { installed: null, download: null };

  const selectedInstalledFamily = $derived(
    fontList.find((f) => f.path === selectedFontPath)?.family ?? ''
  );
  const selectedDownloadFamily = $derived(
    downloadCatalog.find((c) => c.key === selectedDownloadKey)?.family ?? ''
  );

  function syncSelectionToAppliedFont() {
    const appliedFamily = fontStatus.applied?.family;
    if (!appliedFamily) return;

    const installed = fontList.find(
      (font) => font.family.localeCompare(appliedFamily, undefined, { sensitivity: 'base' }) === 0
    );
    const downloadable = downloadCatalog.find(
      (font) => font.family.localeCompare(appliedFamily, undefined, { sensitivity: 'base' }) === 0
    );

    // Prefer the downloadable entry when both lists contain the same family.
    // The applied state does not record which picker supplied the font, and a
    // downloadable family is the actionable choice for the replacement flow.
    if (downloadable) {
      selectedFontPath = '';
      selectedDownloadKey = downloadable.key;
    } else if (installed) {
      selectedDownloadKey = '';
      selectedFontPath = installed.path;
    }
    updateFontPreviewFamily();
  }

  function isAppliedFont(family) {
    return Boolean(
      fontStatus.applied?.family &&
      family.localeCompare(fontStatus.applied.family, undefined, { sensitivity: 'base' }) === 0
    );
  }

  function fontOptions(which) {
    return which === 'installed' ? fontList : downloadCatalog;
  }

  function currentFontOptionIndex(which) {
    if (which === 'installed') {
      return fontList.findIndex((font) => font.path === selectedFontPath);
    }
    return downloadCatalog.findIndex((font) => font.key === selectedDownloadKey);
  }

  function selectFontOption(which, index) {
    const option = fontOptions(which)[index];
    if (!option) return;
    if (which === 'installed') selectInstalledFont(option.path);
    else selectDownloadFont(option.key);
  }

  const fontKeyboard = {
    installed: createDropdownKeyboardHandlers({
      getOptions: () => fontList,
      getSelectedIndex: () => currentFontOptionIndex('installed'),
      getHighlightIndex: () => fontMenu.installed.highlightIndex,
      setHighlightIndex: (index) => {
        fontMenu.installed.highlightIndex = index;
        previewFontAt('installed', index);
      },
      isOpen: () => fontMenu.installed.open,
      toggle: () => toggleFontMenu('installed'),
      close: closeFontMenus,
      select: (index) => selectFontOption('installed', index),
      canSelect: () => {
        if (fontStatus.elevated) return true;
        requestFontElevation();
        return false;
      },
      getMenu: () => fontMenu.installed.el,
      getTrigger: () => fontMenu.installed.wrap?.querySelector('.font-select-trigger'),
    }),
    download: createDropdownKeyboardHandlers({
      getOptions: () => downloadCatalog,
      getSelectedIndex: () => currentFontOptionIndex('download'),
      getHighlightIndex: () => fontMenu.download.highlightIndex,
      setHighlightIndex: (index) => {
        fontMenu.download.highlightIndex = index;
        previewFontAt('download', index);
      },
      isOpen: () => fontMenu.download.open,
      toggle: () => toggleFontMenu('download'),
      close: closeFontMenus,
      select: (index) => selectFontOption('download', index),
      canSelect: () => {
        if (fontStatus.elevated) return true;
        requestFontElevation();
        return false;
      },
      getMenu: () => fontMenu.download.el,
      getTrigger: () => fontMenu.download.wrap?.querySelector('.font-select-trigger'),
    }),
  };

  function closeFontMenus() {
    for (const which of ['installed', 'download']) {
      fontMenu[which].open = false;
      fontMenu[which].highlightIndex = -1;
      fontPositionControllers[which]?.stop();
      fontPositionControllers[which] = null;
    }
    clearHoverPreview();
  }

  function toggleFontMenu(which) {
    if (fontBusy || !fontReady) return;
    if (!fontStatus.elevated) {
      requestFontElevation();
      return;
    }
    const menu = fontMenu[which];
    menu.open = !menu.open;
    if (menu.open) {
      menu.highlightIndex = currentFontOptionIndex(which);
      fontPositionControllers[which]?.stop();
      fontPositionControllers[which] = createDropdownPositionController({
        getTrigger: () => fontMenu[which].wrap?.querySelector('.font-select-trigger'),
        getMenu: () => fontMenu[which].el,
        onPlacement: (placement) => { fontMenu[which].placement = placement; },
        options: { gap: 6, viewportMargin: 14, maxHeight: 320, align: 'left' },
      });
      fontPositionControllers[which].start();
      requestAnimationFrame(() => {
        if (fontMenu[which].open) fontMenu[which].el?.focus();
      });
    } else {
      menu.highlightIndex = -1;
      fontPositionControllers[which]?.stop();
      fontPositionControllers[which] = null;
      clearHoverPreview();
    }
  }

  function selectInstalledFont(path) {
    const trigger = fontMenu.installed.wrap?.querySelector('.font-select-trigger');
    closeFontMenus();
    selectedFontPath = path;
    onSelectInstalledFont();
    requestAnimationFrame(() => trigger?.focus());
  }

  function selectDownloadFont(key) {
    const trigger = fontMenu.download.wrap?.querySelector('.font-select-trigger');
    closeFontMenus();
    selectedDownloadKey = key;
    onSelectDownloadFont();
    requestAnimationFrame(() => trigger?.focus());
  }

  $effect(() => {
    if (!fontMenu.installed.open && !fontMenu.download.open) return;
    function onDown(event) {
      for (const which of ['installed', 'download']) {
        const menu = fontMenu[which];
        if (menu.open && menu.wrap && !menu.wrap.contains(event.target)) {
          menu.open = false;
          menu.highlightIndex = -1;
          fontPositionControllers[which]?.stop();
          fontPositionControllers[which] = null;
          clearHoverPreview();
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
  // Hover or keyboard highlight preview: while a dropdown is open the sample
  // line follows the family under the cursor or highlighted by the keyboard,
  // and reverts to the selected family when the menu closes.
  let hoverPreviewFamily = $state('');
  let hoverPreviewCss = $state('');
  const previewFamily = $derived(hoverPreviewFamily || fontPreviewFamily);
  const previewCss = $derived(hoverPreviewCss || fontPreviewCss);
  // CSS font-family for the Currently Applied line: the real family name for
  // installed fonts, the unique GF-<key> name for catalog fonts, empty when
  // the system default is in use.
  const appliedFontCss = $derived(() => {
    const applied = fontStatus.applied?.family;
    if (!applied) return '';
    const installed = fontList.find((f) => f.family.localeCompare(applied, undefined, { sensitivity: 'base' }) === 0);
    if (installed) return cssFontFamily(installed.family);
    const catalog = downloadCatalog.find((c) => c.family.localeCompare(applied, undefined, { sensitivity: 'base' }) === 0);
    return catalog ? catalogPreviewFont(catalog.key) : '';
  });
  // True when the current selection is the font already applied to Windows.
  // The Apply/Download & apply button is hidden for this state: re-applying
  // the same font is a no-op, so the only action left is Restart.
  const selectionIsApplied = $derived(
    (selectedFontPath !== '' && isAppliedFont(selectedInstalledFamily)) ||
    (selectedDownloadKey !== '' && isAppliedFont(selectedDownloadFamily))
  );
  function onSelectInstalledFont() {
    selectedDownloadKey = '';
    fontJustApplied = false;
    updateFontPreviewFamily();
  }
  function onSelectDownloadFont() {
    selectedFontPath = '';
    fontJustApplied = false;
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

  // Show the family at a dropdown index in the sample line (hover or keyboard
  // highlight). Resolves to the real family name for installed fonts and the
  // unique GF-<key> name for catalog fonts, mirroring updateFontPreviewFamily.
  function previewFontAt(which, index) {
    const option = fontOptions(which)[index];
    if (!option) {
      hoverPreviewFamily = '';
      hoverPreviewCss = '';
      return;
    }
    hoverPreviewFamily = option.family;
    hoverPreviewCss = which === 'installed' ? cssFontFamily(option.family) : catalogPreviewFont(option.key);
  }

  function clearHoverPreview() {
    hoverPreviewFamily = '';
    hoverPreviewCss = '';
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
      fontJustApplied = true;
      fontMessage = `Applied "${state.family}".`;
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
      fontJustApplied = false;
      fontMessage = 'Restore scheduled.';
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
      fontJustApplied = true;
      fontMessage = `Downloaded and applied "${state.family}".`;
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
          <span class="tab-monitor-tag">Global</span>
          <span class="text-render-experimental-badge">Experimental</span>
        </div>
        <span class="subtitle">Replace the system UI font.</span>
      </div>
    </div>
    <div class="tab-heading-spacer"></div>
    <button class="restore-defaults-btn" disabled={fontBusy || !fontStatus.applied} onclick={() => restoreConfirmOpen = true}>
      ↻ Restore original fonts
    </button>
  </div>
  <div class="separator"></div>

  {#if !fontStatus.elevated}
    <div class="font-admin-warning" role="alert">
      <p class="font-warning">Administrator rights are required to apply or restore a font.</p>
      <button type="button" class="font-admin-restart-btn" onclick={requestFontElevation}>Restart as administrator</button>
    </div>
  {/if}

  <div class="font-section">
    <div class="font-current-line">
      <span class="font-current-label">Currently Applied:</span>
      <span class="font-current-name" style={appliedFontCss ? `font-family: ${appliedFontCss}` : ''}>
        {fontStatus.applied?.family ?? 'System default'}
      </span>
    </div>

    <div class="font-row font-choice-row">
      <div class="font-choice-group">
        <span class="font-choice-label">Installed fonts</span>
        <span class="font-select-wrap" bind:this={fontMenu.installed.wrap}>
          <button
          type="button"
          class="settings-select font-select-trigger"
          aria-haspopup="listbox"
          aria-expanded={fontMenu.installed.open}
          disabled={!fontReady || fontBusy}
          onclick={() => toggleFontMenu('installed')}
            onkeydown={fontKeyboard.installed.onTriggerKeydown}>
            <span class="font-select-current">
              {#if isAppliedFont(selectedInstalledFamily)}<span class="font-select-check" aria-hidden="true">✓</span>{/if}
              <span class="font-select-label" class:placeholder={!selectedInstalledFamily}>{selectedInstalledFamily || 'Choose an installed font…'}</span>
            </span>
            <span class="font-select-caret" aria-hidden="true"></span>
          </button>
        {#if fontMenu.installed.open}
          <div
            class="dd-menu dropdown-menu-position-{fontMenu.installed.placement}"
            bind:this={fontMenu.installed.el}
            role="listbox"
            aria-label="Choose an installed font"
            tabindex="-1"
            onkeydown={fontKeyboard.installed.onMenuKeydown}>
            {#each fontList as f, index (f.path)}
              <button
                type="button"
                class="dd-menu-item"
                class:active={selectedFontPath === f.path}
                class:keyboard-highlighted={fontMenu.installed.highlightIndex === index}
                data-dropdown-option-index={index}
                role="option"
                aria-selected={selectedFontPath === f.path}
                style="font-family: {cssFontFamily(f.family)}"
                onfocus={() => fontKeyboard.installed.onItemFocus(index)}
                onmouseenter={() => previewFontAt('installed', index)}
                onmouseleave={clearHoverPreview}
                onkeydown={fontKeyboard.installed.onItemKeydown}
                onclick={() => selectInstalledFont(f.path)}>
                <span class="dd-check" aria-hidden="true">{isAppliedFont(f.family) ? '✓' : ''}</span>
                <span>{f.family}</span>
              </button>
            {/each}
          </div>
        {/if}
        </span>
      </div>
      <div class="font-choice-group">
        <span class="font-choice-label">Google Fonts</span>
        <span class="font-select-wrap" bind:this={fontMenu.download.wrap}>
          <button
          type="button"
          class="settings-select font-select-trigger"
          aria-haspopup="listbox"
          aria-expanded={fontMenu.download.open}
          disabled={!fontReady || fontBusy}
          onclick={() => toggleFontMenu('download')}
            onkeydown={fontKeyboard.download.onTriggerKeydown}>
            <span class="font-select-current">
              {#if isAppliedFont(selectedDownloadFamily)}<span class="font-select-check" aria-hidden="true">✓</span>{/if}
              <span class="font-select-label" class:placeholder={!selectedDownloadFamily}>{selectedDownloadFamily || 'Or download a recommended font…'}</span>
            </span>
            <span class="font-select-caret" aria-hidden="true"></span>
          </button>
        {#if fontMenu.download.open}
          <div
            class="dd-menu dropdown-menu-position-{fontMenu.download.placement}"
            bind:this={fontMenu.download.el}
            role="listbox"
            aria-label="Download a recommended font"
            tabindex="-1"
            onkeydown={fontKeyboard.download.onMenuKeydown}>
            {#each downloadCatalog as c, index (c.key)}
              <button
                type="button"
                class="dd-menu-item"
                class:active={selectedDownloadKey === c.key}
                class:keyboard-highlighted={fontMenu.download.highlightIndex === index}
                data-dropdown-option-index={index}
                role="option"
                aria-selected={selectedDownloadKey === c.key}
                style="font-family: {catalogPreviewFont(c.key)}"
                onfocus={() => fontKeyboard.download.onItemFocus(index)}
                onmouseenter={() => previewFontAt('download', index)}
                onmouseleave={clearHoverPreview}
                onkeydown={fontKeyboard.download.onItemKeydown}
                onclick={() => selectDownloadFont(c.key)}>
                <span class="dd-check" aria-hidden="true">{isAppliedFont(c.family) ? '✓' : ''}</span>
                <span class="font-catalog-name">{c.family}</span>
                {#if c.description}
                  <span class="font-catalog-desc">{c.description}</span>
                {/if}
              </button>
            {/each}
          </div>
        {/if}
        </span>
      </div>
    </div>

    {#if previewFamily}
      <div class="font-preview-line">
        <span class="font-preview-label">Preview:</span>
        <div class="font-preview-sizes">
          {#each [8, 10, 12, 15] as size (size)}
            <div class="font-preview-size-row">
              <span class="font-preview-size-label">{size}</span>
              <span class="font-preview-text" style="font-family: {previewCss}; font-size: {size}px">
                The quick brown fox jumps over the lazy dog 0123456789
              </span>
            </div>
          {/each}
        </div>
      </div>
    {/if}

    <div class="font-row">
      {#if fontJustApplied}
        <button class="font-btn font-btn-apply font-btn-applied" disabled>Applied ✓</button>
      {:else if !selectionIsApplied}
        <button class="font-btn font-btn-apply" disabled={fontBusy || !fontStatus.elevated || (!selectedFontPath && !selectedDownloadKey)} onclick={applySelected}>
          {fontBusy ? '…' : (selectedDownloadKey ? 'Download & apply' : 'Apply')}
        </button>
      {/if}
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
      <p class="font-message" role="status">
        {fontMessage}
        {#if fontStatus.needsRestart}
          <span class="font-restart-warning">Restart required to take effect.</span>
        {/if}
      </p>
    {:else if fontStatus.needsRestart}
      <!-- Standalone restart warning: shown whenever a boot-time swap is still
           pending, including after switching tabs (fontMessage is transient
           local state, needsRestart comes from the backend status). -->
      <p class="font-restart-warning" role="status">Restart required to take effect.</p>
    {/if}
    {#if fontError}
      <p class="font-error" role="status">{fontError}</p>
    {/if}
    {#if restartError}
      <p class="font-error" role="status">{restartError}</p>
    {/if}
  </div>

  {#if restoreConfirmOpen}
    <ConfirmModal
      title="Restore original fonts?"
      desc="This switches the system UI font back to the original Windows fonts, taking effect after a restart."
      confirmLabel="Restore"
      onConfirm={restoreFont}
      onClose={() => restoreConfirmOpen = false} />
  {/if}

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
