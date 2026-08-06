<script>
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { app, crush, taskbarDimmer, blackLift, pollDisplayInfo, scheduleDisplayInfoRefresh, pollHDRStatus, pollHDRLuminance, loadConfig, saveConfig, setDimmerSuspendedForFullscreen, setTaskbarDimmer, applyBlackLevelProfileForRefreshRate, persistCurrentBlackLevelProfile, blackLevelDisplayForRefreshRate, displayMenuPrefs, setResolutionSort, setResolutionGroupBy, setShowTags, setRefreshRateSort, dismissCtxMenus } from './lib/stores.svelte.js';
  import { invoke } from '@tauri-apps/api/core';
  import TabCrush from './lib/TabCrush.svelte';
  import TabDimmer from './lib/TabDimmer.svelte';
  import TabHotkeys from './lib/TabHotkeys.svelte';
  import TabDebug from './lib/TabDebug.svelte';
  import TabSystem from './lib/TabSystem.svelte';
  import TabSettings from './lib/TabSettings.svelte';
  import TabAbout from './lib/TabAbout.svelte';

  // Icons are inlined as data URIs (?inline) instead of /icons/*.png URLs so
  // they never depend on the dev server or asset protocol at runtime — a tray
  // restart can leave the vite dev server dead, and any icon requested after
  // that would otherwise render as a broken-image logo.
  import appIcon from './lib/icons/app-icon.png?inline';
  import tabSystemIcon from './lib/icons/tab_system.png?inline';
  import tabAboutIcon from './lib/icons/tab_about.png?inline';
  import tabCrushIcon from './lib/icons/tab_crush.png?inline';
  import tabDimmerIcon from './lib/icons/tab_dimmer.png?inline';
  import tabHotkeysIcon from './lib/icons/tab_hotkeys.png?inline';

  let windowVisible = $state(true);

  // Keep the native tray flyout's accent in sync with the main UI: push the
  // current HDR/SDR status to Rust so the flyout's Keep button matches
  // --accent-hdr / --accent-sdr even while the main window is hidden.
  $effect(() => {
    const status = app.hdrStatus;
    if (status === 'unknown') return;
    invoke('set_flyout_hdr', { hdr: status === 'hdr' }).catch(() => {});
  });

  // Custom click-only dropdown for resolution, replacing the native
  // <select> for the same reason as the Hz dropdown below (see comment
  // there). Mirrors its structure closely: aspect-ratio-grouped sections
  // instead of a flat list, but otherwise identical open/close/outside-click
  // behavior.
  let resMenuOpen = $state(false);
  let resMenuEl = $state(null);
  let resButtonEl = $state(null);

  function toggleResMenu() {
    if (app.displayModeBusy || app.refreshRateBusy || pendingModeConfirmation || app.displayInfo.resolutions.length === 0) return;
    resMenuOpen = !resMenuOpen;
  }
  function closeResMenu() {
    resMenuOpen = false;
  }
  function pickResolution(width, height) {
    closeResMenu();
    changeResolution({ currentTarget: { value: `${width}x${height}` } });
  }

  $effect(() => {
    if (!resMenuOpen) return;
    function onDocPointerDown(e) {
      if (resMenuEl?.contains(e.target) || resButtonEl?.contains(e.target) || resCtxMenuEl?.contains(e.target)) return;
      closeResMenu();
    }
    function onKeydown(e) {
      if (e.key === 'Escape') { closeResMenu(); resButtonEl?.focus(); }
    }
    document.addEventListener('pointerdown', onDocPointerDown);
    document.addEventListener('keydown', onKeydown);
    // Jump the list so the currently-selected resolution is already in view
    // (centered where possible) instead of opening scrolled to the top —
    // matches how native <select> popups position themselves around the
    // selected option. block:'center' with behavior:'instant' (not
    // 'smooth') mirrors the native no-animation snap.
    resMenuEl?.querySelector('.hz-menu-item.active')?.scrollIntoView({ block: 'center', behavior: 'instant' });
    return () => {
      document.removeEventListener('pointerdown', onDocPointerDown);
      document.removeEventListener('keydown', onKeydown);
    };
  });

  // Custom click-only dropdown for refresh rate (Hz), replacing the native
  // <select>. Native selects use the OS/Chromium popup, which auto-closes on
  // ANY wheel scroll anywhere on the page (not just over the control) — by
  // design, to avoid the popup misaligning from its anchor while the page
  // scrolls underneath. That's what caused the "dropdown closes itself"
  // reports. A plain absolutely-positioned list has no such built-in
  // dismiss-on-scroll behavior, so it stays open through an incidental page
  // scroll and only closes on an explicit outside click, Escape, or picking
  // an option — exactly the click-only interaction that was asked for.
  let hzMenuOpen = $state(false);
  let hzMenuEl = $state(null);
  let hzButtonEl = $state(null);

  function toggleHzMenu() {
    if (app.refreshRateBusy || app.displayModeBusy || pendingModeConfirmation || app.displayInfo.refreshRates.length === 0) return;
    hzMenuOpen = !hzMenuOpen;
  }
  function closeHzMenu() {
    hzMenuOpen = false;
  }
  function pickHz(hz) {
    closeHzMenu();
    changeRefreshRate({ currentTarget: { value: hz } });
  }

  $effect(() => {
    if (!hzMenuOpen) return;
    function onDocPointerDown(e) {
      if (hzMenuEl?.contains(e.target) || hzButtonEl?.contains(e.target) || hzCtxMenuEl?.contains(e.target)) return;
      closeHzMenu();
    }
    function onKeydown(e) {
      if (e.key === 'Escape') { closeHzMenu(); hzButtonEl?.focus(); }
    }
    // pointerdown (not click) so the same click that opened the menu via the
    // button doesn't immediately also count as an "outside" click closing it.
    document.addEventListener('pointerdown', onDocPointerDown);
    document.addEventListener('keydown', onKeydown);
    hzMenuEl?.querySelector('.hz-menu-item.active')?.scrollIntoView({ block: 'center', behavior: 'instant' });
    return () => {
      document.removeEventListener('pointerdown', onDocPointerDown);
      document.removeEventListener('keydown', onKeydown);
    };
  });

  // ── Right-click context menus for the resolution and refresh-rate
  // dropdowns. Mirror the calibration strip's .ctx-menu pattern in TabCrush.
  let resCtxMenu = $state({ open: false, x: 0, y: 0 });
  let resCtxMenuEl = $state(null);
  // Position a context menu at the cursor, clamped to stay on screen. Shared
  // by the open and reposition paths (right-clicking the open menu moves it
  // to the new cursor position instead of closing it). The menuW/menuH
  // estimates track the compact menu's actual size so a near-the-bottom
  // right-click doesn't float the menu unnecessarily high.
  function clampCtxMenu(clientX, clientY, menuW, menuH) {
    return {
      open: true,
      x: Math.max(8, Math.min(clientX, window.innerWidth - menuW - 8)),
      y: Math.max(8, Math.min(clientY, window.innerHeight - menuH - 8)),
    };
  }
  function openResContextMenu(e) {
    e.preventDefault();
    e.stopPropagation();
    if (app.displayInfo.resolutions.length === 0) return;
    // Keep the resolution list open whether the right-click landed on the
    // button or on a row — the context menu overlays it instead of replacing
    // it. Only the other dropdown and its context menu close (their
    // document-level close listener can't fire because this handler stops
    // propagation of the contextmenu event). The calibration strip's menu
    // also can't hear the event (stopPropagation), so dismiss it via the
    // shared tick instead.
    dismissCtxMenus();
    closeHzMenu();
    closeHzContextMenu();
    resCtxMenu = clampCtxMenu(e.clientX, e.clientY, 190, 190);
  }
  function repositionResContextMenu(e) {
    e.preventDefault();
    e.stopPropagation();
    resCtxMenu = clampCtxMenu(e.clientX, e.clientY, 190, 190);
  }
  function closeResContextMenu() {
    resCtxMenu = { ...resCtxMenu, open: false };
  }

  let hzCtxMenu = $state({ open: false, x: 0, y: 0 });
  let hzCtxMenuEl = $state(null);
  function openHzContextMenu(e) {
    e.preventDefault();
    e.stopPropagation();
    if (app.displayInfo.refreshRates.length === 0) return;
    // See openResContextMenu — same stopPropagation caveat, so dismiss the
    // strip menu through the shared tick.
    dismissCtxMenus();
    closeResMenu();
    closeResContextMenu();
    hzCtxMenu = clampCtxMenu(e.clientX, e.clientY, 140, 90);
  }
  function repositionHzContextMenu(e) {
    e.preventDefault();
    e.stopPropagation();
    hzCtxMenu = clampCtxMenu(e.clientX, e.clientY, 140, 90);
  }
  function closeHzContextMenu() {
    hzCtxMenu = { ...hzCtxMenu, open: false };
  }

  // Pressing on the context menu's chrome (scrollbar, padding, labels)
  // dismisses it; pressing on an option button keeps it open. mousedown is
  // used because Chromium may not surface scrollbar interactions as
  // click/pointer events, but mousedown always fires for them.
  function ctxChromeDismiss(e, closeFn) {
    if (e.button === 0 && !e.target.closest('button')) closeFn();
  }

  // Choosing an option from a dropdown's context menu applies the preference
  // and dismisses the context menu, but the dropdown list stays open so the
  // change is visible immediately (new sort order, grouped sections, tags on
  // or off) before picking a resolution. The other dropdown still closes.
  function resCtxAction(action) {
    action();
    closeHzMenu();
    closeHzContextMenu();
    closeResContextMenu();
  }
  function hzCtxAction(action) {
    action();
    closeResMenu();
    closeResContextMenu();
    closeHzContextMenu();
  }

  function handleCtxMenuKeydown(e, closeFn) {
    const items = [...e.currentTarget.querySelectorAll('[role="menuitem"]:not(:disabled)')];
    const currentIndex = items.indexOf(document.activeElement);
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      const direction = e.key === 'ArrowDown' ? 1 : -1;
      const nextIndex = (currentIndex + direction + items.length) % items.length;
      items[nextIndex]?.focus();
    } else if (e.key === 'Home' || e.key === 'End') {
      e.preventDefault();
      (e.key === 'Home' ? items[0] : items[items.length - 1])?.focus();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      closeFn();
    }
    e.stopPropagation();
  }

  $effect(() => {
    if (!resCtxMenu.open) return;
    // The open-time position estimate can be smaller than the rendered menu
    // (it grew with the sort/display sections), which clips the bottom when
    // opened near the status bar. Measure it after paint and pull it back
    // on-screen if it overflows the window.
    const resMenuEl = resCtxMenuEl;
    if (resMenuEl) {
      const r = resMenuEl.getBoundingClientRect();
      const dx = Math.min(0, window.innerWidth - r.right - 8);
      const dy = Math.min(0, window.innerHeight - r.bottom - 8);
      if (dx || dy) resCtxMenu = { ...resCtxMenu, x: r.left + dx, y: r.top + dy };
    }
    const focusId = setTimeout(() => {
      resCtxMenuEl?.querySelector('[role="menuitem"]:not(:disabled)')?.focus();
    }, 0);
    function onDocClick() { closeResContextMenu(); }
    function onDocKey(e) { if (e.key === 'Escape') closeResContextMenu(); }
    function onDocPointerDown(e) {
      // Any press outside the menu dismisses it. mousedown (not click) also
      // fires for scrollbar interactions, which covers e.g. clicking the
      // dropdown list's scrollbar while the context menu overlays it.
      if (resCtxMenuEl?.contains(e.target)) return;
      closeResContextMenu();
    }
    const id = setTimeout(() => {
      document.addEventListener('click', onDocClick);
      document.addEventListener('contextmenu', onDocClick);
      document.addEventListener('mousedown', onDocPointerDown);
      document.addEventListener('keydown', onDocKey);
    }, 0);
    return () => {
      clearTimeout(focusId);
      clearTimeout(id);
      document.removeEventListener('click', onDocClick);
      document.removeEventListener('contextmenu', onDocClick);
      document.removeEventListener('mousedown', onDocPointerDown);
      document.removeEventListener('keydown', onDocKey);
    };
  });

  $effect(() => {
    if (!hzCtxMenu.open) return;
    const hzMenuEl = hzCtxMenuEl;
    if (hzMenuEl) {
      const r = hzMenuEl.getBoundingClientRect();
      const dx = Math.min(0, window.innerWidth - r.right - 8);
      const dy = Math.min(0, window.innerHeight - r.bottom - 8);
      if (dx || dy) hzCtxMenu = { ...hzCtxMenu, x: r.left + dx, y: r.top + dy };
    }
    const focusId = setTimeout(() => {
      hzCtxMenuEl?.querySelector('[role="menuitem"]:not(:disabled)')?.focus();
    }, 0);
    function onDocClick() { closeHzContextMenu(); }
    function onDocKey(e) { if (e.key === 'Escape') closeHzContextMenu(); }
    function onDocPointerDown(e) {
      if (hzCtxMenuEl?.contains(e.target)) return;
      closeHzContextMenu();
    }
    const id = setTimeout(() => {
      document.addEventListener('click', onDocClick);
      document.addEventListener('contextmenu', onDocClick);
      document.addEventListener('mousedown', onDocPointerDown);
      document.addEventListener('keydown', onDocKey);
    }, 0);
    return () => {
      clearTimeout(focusId);
      clearTimeout(id);
      document.removeEventListener('click', onDocClick);
      document.removeEventListener('contextmenu', onDocClick);
      document.removeEventListener('mousedown', onDocPointerDown);
      document.removeEventListener('keydown', onDocKey);
    };
  });

  // Toggling real Windows HDR (Settings > Display > HDR) via the backend.
  // Detection already exists (app.hdrStatus, driven by pollHDRStatus's
  // matchMedia check below) — this just adds the write side.
  let windowsHdrBusy = $state(false);
  let windowsHdrError = $state('');
  let refreshRateError = $state('');
  let resolutionError = $state('');
  let pendingModeConfirmation = $state(null);
  let modeConfirmationTimer = $state(null);
  let modeConfirmationBusy = $state(false);
  let modeConfirmationError = $state('');

  function clearModeConfirmation() {
    if (modeConfirmationTimer) clearInterval(modeConfirmationTimer);
    modeConfirmationTimer = null;
    pendingModeConfirmation = null;
  }

  function startModeConfirmation(result) {
    clearModeConfirmation();
    modeConfirmationError = '';
    const seconds = Number(result?.confirmation_seconds) || 15;
    const started = Date.now();
    pendingModeConfirmation = { ...result, remaining: seconds };
    modeConfirmationTimer = setInterval(() => {
      const remaining = Math.max(0, seconds - Math.ceil((Date.now() - started) / 1000));
      if (remaining <= 0) {
        // Let the frontend perform the same rollback as the backend watchdog.
        // Keeping the modal until the command succeeds gives the user a
        // visible retry path if the display driver temporarily refuses restore.
        clearInterval(modeConfirmationTimer);
        modeConfirmationTimer = null;
        if (pendingModeConfirmation && !modeConfirmationBusy) rollbackModeChange();
      } else if (pendingModeConfirmation) {
        pendingModeConfirmation = { ...pendingModeConfirmation, remaining };
      }
    }, 250);
  }

  async function confirmModeChange() {
    if (modeConfirmationBusy || !pendingModeConfirmation) return;
    modeConfirmationBusy = true;
    try {
      await invoke('confirm_display_mode_change');
      clearModeConfirmation();
      resolutionError = '';
      refreshRateError = '';
    } catch (e) {
      modeConfirmationError = String(e);
    } finally {
      modeConfirmationBusy = false;
      pollDisplayInfo();
    }
  }

  async function rollbackModeChange() {
    if (modeConfirmationBusy || !pendingModeConfirmation) return;
    modeConfirmationBusy = true;
    try {
      await invoke('rollback_display_mode_change');
      clearModeConfirmation();
    } catch (e) {
      modeConfirmationError = String(e);
    } finally {
      modeConfirmationBusy = false;
      pollDisplayInfo();
    }
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

  function currentResolutionName() {
    return resolutionNames.get(`${app.displayInfo.width}x${app.displayInfo.height}`) || '';
  }

  // Anything wider than a standard 16:9 panel (1.9 gives headroom above
  // 16:9's 1.78 without also catching it) is treated as ultrawide.
  function isUltrawide(width, height) {
    return width / height > 1.9;
  }

  // Simplify a width/height pair to a small aspect-ratio label for the
  // "Group by aspect ratio" display mode. Raw GCD reduction gives
  // mathematically-exact but unfamiliar fractions (3840x1600 reduces to
  // 12:5), so match against the small set of ratios actually used to market
  // and describe monitors first, and only fall back to GCD reduction for a
  // resolution that doesn't correspond to any of them.
  const COMMON_ASPECT_RATIOS = [
    { label: '16:9', value: 16 / 9 },
    { label: '16:10', value: 16 / 10 },
    { label: '21:9', value: 21 / 9 },
    { label: '32:9', value: 32 / 9 },
    { label: '4:3', value: 4 / 3 },
    { label: '5:4', value: 5 / 4 },
    { label: '3:2', value: 3 / 2 },
    { label: '1:1', value: 1 },
  ];
  function gcd(a, b) { return b === 0 ? a : gcd(b, a % b); }
  function aspectRatioLabel(width, height) {
    const ratio = width / height;
    // 0.5% tolerance absorbs the off-by-a-few-px rounding some resolutions
    // have from their nominal ratio (e.g. 3840x1600 is 2.4, exactly 12:5,
    // but is marketed and commonly labeled as 21:9 — nominally 2.333 —
    // rather than its precise fraction).
    let best = null;
    let bestDiff = Infinity;
    for (const candidate of COMMON_ASPECT_RATIOS) {
      const diff = Math.abs(ratio - candidate.value) / candidate.value;
      if (diff < bestDiff) { bestDiff = diff; best = candidate; }
    }
    if (best && bestDiff <= 0.03) return best.label;
    const d = gcd(width, height) || 1;
    return `${width / d}:${height / d}`;
  }

  // Popularity rank for common display aspect ratios, most-used first —
  // used to order the aspect-ratio groups themselves (see groupedResolutions
  // below), independent from COMMON_ASPECT_RATIOS's declaration order which
  // exists only for the nearest-match search in aspectRatioLabel.
  const ASPECT_RATIO_POPULARITY = ['16:9', '16:10', '21:9', '32:9', '4:3', '3:2', '5:4', '1:1'];
  function aspectRatioRank(label) {
    const i = ASPECT_RATIO_POPULARITY.indexOf(label);
    return i === -1 ? ASPECT_RATIO_POPULARITY.length : i;
  }

  // Sorted (by pixel count, direction from displayMenuPrefs) and optionally
  // grouped by aspect ratio. Ungrouped matches the plain flat-list style
  // Windows' own Display Settings resolution dropdown uses; grouped adds
  // aspect-ratio section headers ordered by how common that ratio is (not by
  // resolution size), resolutions within a group still following the chosen
  // sort direction. The display's native resolution comes from its EDID
  // preferred timing (queried in Rust, same data Windows uses for its
  // "(Recommended)" label); when the EDID can't be read the largest
  // supported mode is used as the closest available proxy.
  function sortedResolutions() {
    const dir = displayMenuPrefs.resolutionSort === 'asc' ? 1 : -1;
    return [...app.displayInfo.resolutions].sort((a, b) => dir * ((a.width * a.height) - (b.width * b.height)));
  }

  function groupedResolutions() {
    const dir = displayMenuPrefs.resolutionSort === 'asc' ? 1 : -1;
    const groups = new Map();
    for (const r of app.displayInfo.resolutions) {
      const label = aspectRatioLabel(r.width, r.height);
      if (!groups.has(label)) groups.set(label, []);
      groups.get(label).push(r);
    }
    const entries = [...groups.entries()].map(([label, list]) => {
      list.sort((a, b) => dir * ((a.width * a.height) - (b.width * b.height)));
      const maxPixels = Math.max(...list.map((r) => r.width * r.height));
      return { label, list, maxPixels, rank: aspectRatioRank(label) };
    });
    // Popularity rank first (16:9 group before 21:9 group, etc. regardless of
    // which one happens to have the larger resolutions available); within the
    // same rank (e.g. two GCD-fallback ratios that both missed the common
    // list) fall back to largest-resolution-first as a stable tiebreaker.
    entries.sort((a, b) => a.rank - b.rank || b.maxPixels - a.maxPixels);
    return entries;
  }

  function isRecommendedResolution(resolution) {
    const { nativeWidth, nativeHeight, resolutions } = app.displayInfo;
    if (nativeWidth && nativeHeight) {
      return resolution.width === nativeWidth && resolution.height === nativeHeight;
    }
    if (resolutions.length === 0) return false;
    const largest = resolutions.reduce((best, r) => (r.width * r.height > best.width * best.height ? r : best));
    return resolution.width === largest.width && resolution.height === largest.height;
  }

  async function changeResolution(event) {
    const [width, height] = String(event.currentTarget.value).split('x').map(Number);
    const currentWidth = app.displayInfo.width;
    const currentHeight = app.displayInfo.height;
    if (!Number.isFinite(width) || !Number.isFinite(height)
      || width <= 0 || height <= 0 || app.displayModeBusy
      || app.refreshRateBusy || pendingModeConfirmation || (width === currentWidth && height === currentHeight)) return;

    app.displayModeBusy = true;
    // Save the profile for the mode we are leaving before the optimistic UI
    // update changes the resolution fields used by the scoped profile key.
    persistCurrentBlackLevelProfile();
    app.displayInfo.width = width;
    app.displayInfo.height = height;
    resolutionError = '';
    try {
      const result = await invoke('set_resolution', {
        width,
        height,
        preferredHz: app.displayInfo.refreshRateHz || 0,
      });
      // The backend only returns after the temporary mode has stabilized, so
      // load the profile for the newly active display/mode immediately.
      applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
      startModeConfirmation(result);
    } catch (e) {
      resolutionError = String(e);
      app.displayInfo.width = currentWidth;
      app.displayInfo.height = currentHeight;
      applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
    } finally {
      app.displayModeBusy = false;
      // Re-query the mode so the dropdown and available refresh rates reflect
      // the temporary mode Windows actually accepted.
      pollDisplayInfo();
    }
  }

  async function changeRefreshRate(event) {
    const hz = Number(event.currentTarget.value);
    const currentHz = app.displayInfo.refreshRateHz;
    if (!Number.isFinite(hz) || hz <= 0 || app.refreshRateBusy || app.displayModeBusy || pendingModeConfirmation || hz === currentHz) return;

    // Save the profile for the mode we are leaving before changing the mode.
    persistCurrentBlackLevelProfile();
    app.refreshRateBusy = true;
    app.displayInfo.refreshRateHz = hz;
    refreshRateError = '';
    try {
      const result = await invoke('set_refresh_rate', { hz });
      startModeConfirmation(result);
      // Apply the profile immediately; the next display poll still confirms
      // the temporary mode, and rollback restores the previous display state.
      applyBlackLevelProfileForRefreshRate(hz);
    } catch (e) {
      refreshRateError = String(e);
      // Restore the rejected mode before resolving its profile; otherwise the
      // next poll could briefly save the old value under the target Hz.
      app.displayInfo.refreshRateHz = currentHz;
      applyBlackLevelProfileForRefreshRate(currentHz);
      saveConfig();
    } finally {
      app.refreshRateBusy = false;
      // Read the mode back after Windows has accepted or rejected the change.
      // This also restores the old value if the command failed.
      pollDisplayInfo();
    }
  }

  async function toggleWindowsHdr() {
    if (windowsHdrBusy || app.hdrStatus === 'unknown') return;
    const next = app.hdrStatus !== 'hdr';
    const previous = app.hdrStatus;
    windowsHdrBusy = true;
    // Reflect the switch instantly instead of waiting for the next 1s poll
    // tick to pick it up via matchMedia — that wait is what made toggling
    // from inside the app feel slower than the Windows keyboard shortcut,
    // even though the underlying OS-level change completes at the same speed
    // either way. Reverted in the catch block if the backend call fails.
    app.hdrStatus = next ? 'hdr' : 'sdr';
    try {
      await invoke('set_windows_hdr_enabled', { enabled: next });
      windowsHdrError = '';
    } catch (e) {
      windowsHdrError = String(e);
      app.hdrStatus = previous;
    } finally {
      windowsHdrBusy = false;
      // The next poll tick's matchMedia read will still confirm/correct this
      // against the real OS state shortly after.
    }
  }

  // Global hotkey actions, dispatched by Rust's hotkeys module (WM_HOTKEY).
  // The strip's own keyboard shortcuts are unaffected — these only fire for
  // the registered global bindings.
  let hotkeySaveTimer = null;

  function applyGammaForHotkey() {
    invoke('apply_gamma_ramp', { blackLift: blackLift() }).catch(() => {});
  }

  function scheduleHotkeyConfigSave() {
    clearTimeout(hotkeySaveTimer);
    hotkeySaveTimer = setTimeout(() => saveConfig(), 400);
  }

  function handleHotkeyAction(action) {
    switch (action) {
      case 'toggle_black_crush':
        crush.blackEnabled = !crush.blackEnabled;
        applyGammaForHotkey();
        scheduleHotkeyConfigSave();
        break;
      case 'decrease_black_crush':
        crush.adjustment = Math.max(-150, crush.adjustment - 10);
        applyGammaForHotkey();
        scheduleHotkeyConfigSave();
        break;
      case 'increase_black_crush':
        crush.adjustment = Math.min(150, crush.adjustment + 10);
        applyGammaForHotkey();
        scheduleHotkeyConfigSave();
        break;
      case 'toggle_taskbar_dimmer':
        setTaskbarDimmer(!taskbarDimmer.enabled);
        break;
      case 'toggle_hdr':
        toggleWindowsHdr();
        break;
    }
  }

  onMount(() => {
    // One-time: seed display info and HDR status. Mode changes re-poll
    // explicitly afterward — there is no periodic poll on the main thread.
    pollDisplayInfo();
    pollHDRStatus();

    let unlisten;
    let unlistenDisplayChanged;
    let unlistenHotkeyPressed;
    let unlistenTrayMode;
    // The Rust window subclass forwards WM_DISPLAYCHANGE and WM_DEVICECHANGE.
    // Query only in response to those native notifications; there is no
    // periodic display poll. Sunshine/virtual-display attach and detach events
    // therefore refresh the monitor identity, mode lists, resolution, and Hz
    // while this process remains open.
    listen('display-changed', () => {
      scheduleDisplayInfoRefresh();
    }).then((stop) => { unlistenDisplayChanged = stop; }).catch(() => {});
    listen('window-visibility', (event) => {
      const visible = event.payload !== false;
      windowVisible = visible;
      if (visible) {
        // Re-query display info when the window is shown again (e.g. after
        // sleep/resume while hidden in the tray). The cached mode can be
        // stale or empty from a transitional query during wake; this makes
        // it self-heal without a periodic poll.
        pollDisplayInfo();
      }
    }).then((stop) => { unlisten = stop; }).catch(() => {});
    // Global hotkey dispatch (see hotkeys.rs). Payload is the action name.
    listen('hotkey-pressed', (event) => handleHotkeyAction(event.payload)).then((stop) => { unlistenHotkeyPressed = stop; }).catch(() => {});
    // A resolution or refresh rate picked from the tray menu. Runs the same
    // flow as the in-app dropdowns: save the profile for the mode being left,
    // apply the new one, show the confirmation popup with the auto-revert
    // countdown, then re-query so the dropdowns match the new mode.
    listen('display-mode-applied', (event) => {
      const mode = event.payload;
      persistCurrentBlackLevelProfile();
      if (mode?.width) app.displayInfo.width = mode.width;
      if (mode?.height) app.displayInfo.height = mode.height;
      if (mode?.refresh_hz) app.displayInfo.refreshRateHz = mode.refresh_hz;
      applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
      startModeConfirmation(mode);
      pollDisplayInfo();
    }).then((stop) => { unlistenTrayMode = stop; }).catch(() => {});
    return () => {
      unlisten?.();
      unlistenDisplayChanged?.();
      unlistenHotkeyPressed?.();
      unlistenTrayMode?.();
    };
  });

  $effect(() => {
    document.documentElement.setAttribute('data-hdr', app.hdrStatus === 'hdr' ? 'true' : 'false');
  });

  // Load persisted config (black level) once on startup.
  $effect(() => {
    loadConfig();
  });

  // Event-driven HDR detection — matchMedia fires when Windows HDR is
  // toggled from Settings or via the keyboard shortcut.
  $effect(() => {
    const mq = window.matchMedia('(dynamic-range: high)');
    function onHdrChange() { pollHDRStatus(); }
    try { mq.addEventListener('change', onHdrChange); } catch { /* old browser */ }
    return () => {
      try { mq.removeEventListener('change', onHdrChange); } catch {}
    };
  });

  // HDR luminance is a DXGI call; only refresh while the System Info tab is
  // open (it owns the HDR Max Luminance row), at a gentle 5-second cadence
  // (purely informational).
  $effect(() => {
    if (app.activeTab === 'system' && windowVisible) {
      pollHDRLuminance();
      const timer = setInterval(() => pollHDRLuminance(), 5000);
      return () => clearInterval(timer);
    }
  });

  // Monitor fullscreen of the calibration strip: suspend the taskbar dimmer
  // so Windows auto-hides the taskbar. The dimmer's always-on-top overlay
  // would otherwise keep the taskbar visible on top of the fullscreen strip.
  $effect(() => {
    setDimmerSuspendedForFullscreen(crush.fullscreen === 'monitor');
  });
</script>

<div class="app-shell">
  <div class="top-bar">
    <div class="top-bar-left">
      <img class="app-icon" src={appIcon} alt="OLED Helper" />
      <div class="title-block">
        <span class="title">OLED HELPER</span>
        <span class="version">v0.1.0</span>
      </div>
    </div>
    <div class="spacer"></div>
    <button class="top-btn" class:active={app.activeTab === 'settings'} onclick={() => app.activeTab = 'settings'}>
      <span class="top-btn-inner"><img class="top-btn-icon" src={tabSystemIcon} alt="" /> Settings</span>
    </button>
    <button class="top-btn" class:active={app.activeTab === 'about'} onclick={() => app.activeTab = 'about'}>
      <span class="top-btn-inner"><img class="top-btn-icon" src={tabAboutIcon} alt="" /> About</span>
    </button>
  </div>

  <nav class="sidebar">
    <button class="nav-btn" class:active={app.activeTab === 'crush'} onclick={() => app.activeTab = 'crush'}>
      <img class="nav-icon" src={tabCrushIcon} alt="" /> Black Crush Tweak
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'dimmer'} onclick={() => app.activeTab = 'dimmer'}>
      <img class="nav-icon" src={tabDimmerIcon} alt="" /> Taskbar Dimmer
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'hotkeys'} onclick={() => app.activeTab = 'hotkeys'}>
      <img class="nav-icon" src={tabHotkeysIcon} alt="" /> Hotkeys
    </button>

    <div class="sidebar-spacer"></div>
  </nav>

  <main class="content">
    {#if windowVisible}
      {#if app.activeTab === 'crush'}
        <TabCrush />
      {:else if app.activeTab === 'dimmer'}
        <TabDimmer />
      {:else if app.activeTab === 'hotkeys'}
        <TabHotkeys />
      {:else if app.activeTab === 'debug'}
        <TabDebug />
      {:else if app.activeTab === 'system'}
        <TabSystem />
      {:else if app.activeTab === 'settings'}
        <TabSettings />
      {:else if app.activeTab === 'about'}
        <TabAbout />
      {/if}
    {/if}
  </main>

  <div class="status-bar">
    <span>v0.1.0</span>
    <div class="status-bar-right">
      {#if app.displayInfo.monitorName}
        <span class="current-monitor" title={`${app.displayInfo.monitorName}${app.displayInfo.monitorId ? ` · ${app.displayInfo.monitorId}` : ''}`}>
          {app.displayInfo.monitorName}
        </span>
      {/if}
      <div class="resolution-wrap">
        <button type="button" class="resolution-select" class:active={resMenuOpen}
          bind:this={resButtonEl}
          aria-label="Resolution"
          aria-haspopup="listbox"
          aria-expanded={resMenuOpen}
          title={resolutionError || 'Choose resolution · right-click for list options'}
          disabled={app.displayModeBusy || app.refreshRateBusy || pendingModeConfirmation || app.displayInfo.resolutions.length === 0}
          onclick={toggleResMenu}
          oncontextmenu={openResContextMenu}>
          {app.displayInfo.width && app.displayInfo.height ? `${app.displayInfo.width}×${app.displayInfo.height}` : '—×—'}{#if currentResolutionName()}<span class="refresh-rate-select-value"> · {currentResolutionName()}</span>{/if}
        </button>
        {#snippet resItem(resolution)}
          {@const name = resolutionName(resolution)}
          {@const current = resolution.width === app.displayInfo.width && resolution.height === app.displayInfo.height}
          {@const recommended = isRecommendedResolution(resolution)}
          {@const ultrawide = isUltrawide(resolution.width, resolution.height)}
          <button type="button" class="hz-menu-item" class:active={current}
            role="option" aria-selected={current}
            onclick={() => pickResolution(resolution.width, resolution.height)}>
            {resolution.width}×{resolution.height}
            {#if displayMenuPrefs.showTags}
              {#if ultrawide}<span class="ultrawide-tag"><span class="ultrawide-separator">·</span><svg class="ultrawide-icon" viewBox="0 0 32 16" fill="none" aria-label="Ultrawide"><rect x="0.9" y="0.9" width="30.2" height="14.2" rx="2" stroke="currentColor" stroke-width="1.1"/><text x="16" y="10.8" text-anchor="middle" fill="currentColor" font-size="9" font-family="Inter, sans-serif" font-weight="700" letter-spacing="0.2">UW</text></svg></span>{/if}
              {name ? ` · ${name}` : ''}
            {/if}
            {recommended ? ' (Native)' : ''}
          </button>
        {/snippet}
        {#if resMenuOpen}
          <div class="hz-menu res-menu" bind:this={resMenuEl} role="listbox" aria-label="Resolution options" tabindex="-1" oncontextmenu={openResContextMenu}>
            {#if app.displayInfo.resolutions.length === 0}
              <div class="hz-menu-item active">
                {app.displayInfo.width && app.displayInfo.height ? `${app.displayInfo.width}×${app.displayInfo.height}` : '—×—'}
              </div>
            {:else if displayMenuPrefs.resolutionGroupBy === 'aspectRatio'}
              {#each groupedResolutions() as group}
                <div class="hz-menu-group-label">{group.label}</div>
                {#each group.list as resolution}
                  {@render resItem(resolution)}
                {/each}
              {/each}
            {:else}
              {#each sortedResolutions() as resolution}
                {@render resItem(resolution)}
              {/each}
            {/if}
          </div>
        {/if}
        {#if resCtxMenu.open}
          <div class="ctx-menu compact" bind:this={resCtxMenuEl} style="left:{resCtxMenu.x}px; top:{resCtxMenu.y}px;" role="menu" aria-label="Resolution list options" tabindex="-1" onmousedown={(e) => ctxChromeDismiss(e, closeResContextMenu)} onclick={(e) => e.stopPropagation()} onkeydown={(e) => handleCtxMenuKeydown(e, closeResContextMenu)} oncontextmenu={repositionResContextMenu}>
            <div class="ctx-menu-subheading">Sort</div>
            <button class="ctx-item" role="menuitem" class:active={displayMenuPrefs.resolutionSort === 'asc'} onclick={() => resCtxAction(() => setResolutionSort('asc'))}><span class="ctx-radio-dot" aria-hidden="true"></span>Ascending</button>
            <button class="ctx-item" role="menuitem" class:active={displayMenuPrefs.resolutionSort === 'desc'} onclick={() => resCtxAction(() => setResolutionSort('desc'))}><span class="ctx-radio-dot" aria-hidden="true"></span>Descending</button>
            <div class="ctx-menu-sep"></div>
            <div class="ctx-menu-subheading">Display</div>
            <button class="ctx-item" role="menuitem" class:active={displayMenuPrefs.resolutionGroupBy === 'aspectRatio'} onclick={() => resCtxAction(() => setResolutionGroupBy(displayMenuPrefs.resolutionGroupBy === 'aspectRatio' ? 'none' : 'aspectRatio'))}><span class="ctx-checkbox" aria-hidden="true"></span>Group by aspect ratio</button>
            <button class="ctx-item" role="menuitem" class:active={displayMenuPrefs.showTags} onclick={() => resCtxAction(() => setShowTags(!displayMenuPrefs.showTags))}><span class="ctx-checkbox" aria-hidden="true"></span>Show tags (4K, QHD, UW)</button>
          </div>
        {/if}
        {#if resolutionError}
          <span class="resolution-error" role="status" aria-label="Resolution change failed" title={resolutionError}>!</span>
        {/if}
      </div>
      <div class="refresh-rate-wrap">
        <button type="button" class="refresh-rate-select" class:active={hzMenuOpen}
          bind:this={hzButtonEl}
          aria-label="Refresh rate"
          aria-haspopup="listbox"
          aria-expanded={hzMenuOpen}
          title={refreshRateError || 'Choose refresh rate · right-click for list options'}
          disabled={app.refreshRateBusy || app.displayModeBusy || pendingModeConfirmation || app.displayInfo.refreshRates.length === 0}
          onclick={toggleHzMenu}
          oncontextmenu={openHzContextMenu}>
          {app.displayInfo.refreshRateHz ? `${app.displayInfo.refreshRateHz} Hz` : '— Hz'}{#if app.displayInfo.refreshRateHz && blackLevelDisplayForRefreshRate(app.displayInfo.refreshRateHz)}<span class="refresh-rate-select-value"> ({blackLevelDisplayForRefreshRate(app.displayInfo.refreshRateHz)})</span>{/if}
        </button>
        {#if hzMenuOpen}
          <div class="hz-menu" bind:this={hzMenuEl} role="listbox" aria-label="Refresh rate options" tabindex="-1" oncontextmenu={openHzContextMenu}>
            {#each [...app.displayInfo.refreshRates].sort((a, b) => (displayMenuPrefs.refreshRateSort === 'asc' ? a - b : b - a)) as hz}
              <button type="button" class="hz-menu-item" class:active={hz === app.displayInfo.refreshRateHz}
                role="option" aria-selected={hz === app.displayInfo.refreshRateHz}
                onclick={() => pickHz(hz)}>
                <span>{hz} Hz</span>
                {#if blackLevelDisplayForRefreshRate(hz)}<span class="hz-profile-value">{blackLevelDisplayForRefreshRate(hz)}</span>{/if}
              </button>
            {/each}
          </div>
        {/if}
        {#if hzCtxMenu.open}
          <div class="ctx-menu compact" bind:this={hzCtxMenuEl} style="left:{hzCtxMenu.x}px; top:{hzCtxMenu.y}px;" role="menu" aria-label="Refresh rate list options" tabindex="-1" onmousedown={(e) => ctxChromeDismiss(e, closeHzContextMenu)} onclick={(e) => e.stopPropagation()} onkeydown={(e) => handleCtxMenuKeydown(e, closeHzContextMenu)} oncontextmenu={repositionHzContextMenu}>
            <button class="ctx-item" role="menuitem" class:active={displayMenuPrefs.refreshRateSort === 'asc'} onclick={() => hzCtxAction(() => setRefreshRateSort('asc'))}><span class="ctx-radio-dot" aria-hidden="true"></span>Ascending</button>
            <button class="ctx-item" role="menuitem" class:active={displayMenuPrefs.refreshRateSort === 'desc'} onclick={() => hzCtxAction(() => setRefreshRateSort('desc'))}><span class="ctx-radio-dot" aria-hidden="true"></span>Descending</button>
          </div>
        {/if}
        {#if refreshRateError}
          <span class="refresh-rate-error" role="status" aria-label="Refresh rate change failed" title={refreshRateError}>!</span>
        {/if}
      </div>
      <div class="hdr-switch-wrap">
        <button class="hdr-switch" class:hdr={app.hdrStatus === 'hdr'} disabled={windowsHdrBusy || app.hdrStatus === 'unknown'}
          aria-label="Windows HDR"
          onclick={toggleWindowsHdr}>
          <span class="hdr-switch-opt" class:active={app.hdrStatus !== 'hdr'}>SDR</span>
          <span class="hdr-switch-opt" class:active={app.hdrStatus === 'hdr'}>HDR</span>
          <svg class="hdr-switch-arrow hdr-switch-arrow-right" viewBox="0 0 10 10" fill="none" aria-hidden="true"><path d="M3 2L7 5L3 8" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>
          <svg class="hdr-switch-arrow hdr-switch-arrow-left" viewBox="0 0 10 10" fill="none" aria-hidden="true"><path d="M7 2L3 5L7 8" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>
        </button>
        <div class="hdr-switch-tooltip">
          {windowsHdrError || (app.hdrStatus === 'unknown' ? 'HDR status unknown' : `Switch to ${app.hdrStatus === 'hdr' ? 'SDR' : 'HDR'}`)}
        </div>
      </div>
    </div>
  </div>

  {#if pendingModeConfirmation}
    <div class="modal-backdrop mode-confirm-backdrop" role="presentation">
      <div class="modal mode-confirm-modal" role="dialog" aria-modal="true" aria-labelledby="mode-confirm-title" tabindex="-1">
        <h2 id="mode-confirm-title" class="modal-title">Keep display mode?</h2>
        <p class="modal-desc">
          {pendingModeConfirmation.monitor_name || 'Current monitor'} is temporarily using
          <strong>{pendingModeConfirmation.width}×{pendingModeConfirmation.height} at {pendingModeConfirmation.refresh_hz} Hz</strong>.
          Confirm if the picture looks correct. It will automatically revert in {pendingModeConfirmation.remaining}s.
        </p>
        {#if modeConfirmationError}<p class="modal-error">Rollback failed: {modeConfirmationError}</p>{/if}
        <div class="modal-actions">
          <button class="modal-btn" onclick={confirmModeChange} disabled={modeConfirmationBusy}>Keep this mode</button>
          <button class="modal-btn modal-btn-cancel" onclick={rollbackModeChange} disabled={modeConfirmationBusy}>Revert ({pendingModeConfirmation.remaining}s)</button>
        </div>
      </div>
    </div>
  {/if}
</div>
