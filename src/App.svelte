<script>
  import { onMount } from 'svelte';
  import { emit, listen } from '@tauri-apps/api/event';
  import { app, appSettings, crush, taskbarDimmer, screenDimmer, screenDimmerHelper, refreshScreenDimmerHelperReady, blackLift, pollDisplayInfo, scheduleDisplayInfoRefresh, refreshVendorDisplayInfo, pollHDRStatus, pollHDRLuminance, refreshSdrWhiteLevel, setSdrWhiteNits, saveConfig, setDimmerSuspendedForFullscreen, setTaskbarDimmer, setTaskbarDimmerEnabledForAll, applyBlackLevelProfileForRefreshRate, persistCurrentBlackLevelProfile, blackLevelDisplayForRefreshRate, displayMenuPrefs, setResolutionSort, setResolutionGroupBy, setShowTags, setRefreshRateSort, dismissCtxMenus, displayFavorites, displayModeFavoriteKey, isDisplayModeFavorited, addFavoriteDisplayMode, removeFavoriteDisplayMode, markFavoriteConfirmed, favoriteForAction, updateState, checkForUpdates, monitors, activeMonitor, loadMonitors, setActiveMonitor, applyGamma, setScreenDimmerEnabled, startReferenceModePolling, stopReferenceModePolling, configLoaded, monProfileId } from './lib/stores.svelte.js';
  import { invoke } from '@tauri-apps/api/core';
  import { getVersion } from '@tauri-apps/api/app';
  import TabCrush from './lib/TabCrush.svelte';
  import TabDimmer from './lib/TabDimmer.svelte';
  import TabHotkeys from './lib/TabHotkeys.svelte';
  import TabPower from './lib/TabPower.svelte';
  import TabPowerIcon from './lib/TabPowerIcon.svelte';
  import TabWindowsHdr from './lib/TabWindowsHdr.svelte';
  import TabDebug from './lib/TabDebug.svelte';
  import TabSystem from './lib/TabSystem.svelte';
  import TabSettings from './lib/TabSettings.svelte';
  import TabAbout from './lib/TabAbout.svelte';
  import TabText from './lib/TabText.svelte';
  import TabTextIcon from './lib/TabTextIcon.svelte';
  import TabWindowsFont from './lib/TabWindowsFont.svelte';
  import TabWindowsFontIcon from './lib/TabWindowsFontIcon.svelte';

  // Icons are inlined as data URIs (?inline) instead of /icons/*.png URLs so
  // they never depend on the dev server or asset protocol at runtime, a tray
  // restart can leave the vite dev server dead, and any icon requested after
  // that would otherwise render as a broken-image logo.
  import appIcon from './lib/icons/app-icon.png?inline';
  import tabSystemIcon from './lib/icons/tab_system.png?inline';
  import tabAboutIcon from './lib/icons/tab_about.png?inline';
  import tabCrushIcon from './lib/icons/tab_crush.png?inline';
  import tabDimmerIcon from './lib/icons/tab_dimmer.png?inline';
  import tabScreenDimmerIcon from './lib/icons/tab_screen_dimmer.png?inline';
  import tabHdrIcon from './lib/icons/tab_hdr.png?inline';
  import tabDebugIcon from './lib/icons/tab_debug.png?inline';
  import tabHotkeysIcon from './lib/icons/tab_hotkeys.png?inline';
  import { dropdownCloseEffect, ctxMenuCloseEffect } from './lib/dropdown.js';
  import UpdateModal from './lib/UpdateModal.svelte';
  import MonitorSelector from './lib/MonitorSelector.svelte';

  let windowVisible = $state(true);
  // Current app version read from the Tauri package at runtime, so the
  // status-bar version stays in sync with the version the updater compares
  // against (Cargo.toml / tauri.conf.json) instead of a hardcoded string.
  let appVersion = $state('');
  // Coalesces the display-changed burst (hotplug/mode change emits several
  // WM_DISPLAYCHANGE/WM_DEVICECHANGE messages) into a single SDR-white base
  // re-capture, and gives the OS a beat to settle before the new value is read.
  let sdrWhiteBaseRecaptureTimer = null;
  let hdrStatusRefreshTimer = null;
  let vendorDisplayRefreshTimer = null;

  // Keep the native tray flyout's accent in sync with the main UI: push the
  // current HDR/SDR status to Rust so the flyout's Keep button matches
  // --accent-hdr / --accent-sdr even while the main window is hidden.
  $effect(() => {
    const status = app.hdrStatus;
    if (status === 'unknown') return;
    invoke('set_flyout_hdr', { hdr: status === 'hdr' }).catch(() => {});
    // Keep the separate tray WebView synchronized even when its own
    // matchMedia listener misses a remote-display transition.
    emit('hdr-status-changed', status).catch(() => {});
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
    if (app.displayModeBusy || app.refreshRateBusy || app.displayInfo.resolutions.length === 0) return;
    resMenuOpen = !resMenuOpen;
  }
  function closeResMenu() {
    resMenuOpen = false;
  }
  function pickResolution(width, height) {
    // A press on the list while its context menu was open sets
    // resCtxDismissClick (document capture guard below). If a click still
    // fires despite preventDefault, swallow it here instead of applying the
    // item under the cursor. Also covers keyboard activation, Enter on a
    // focused item, where no pointer event precedes the click: dismiss the
    // menu instead of applying the item underneath it.
    if (resCtxDismissClick) {
      resCtxDismissClick = false;
      return;
    }
    if (resCtxMenu.open) {
      closeResContextMenu();
      return;
    }
    closeResMenu();
    changeResolution({ currentTarget: { value: `${width}x${height}` } });
  }

  $effect(() => {
    if (!resMenuOpen) return;
    return dropdownCloseEffect(resMenuEl, resButtonEl, resCtxMenuEl, closeResMenu, resButtonEl);
  });

  // Custom click-only dropdown for refresh rate (Hz), replacing the native
  // <select>. Native selects use the OS/Chromium popup, which auto-closes on
  // ANY wheel scroll anywhere on the page (not just over the control), by
  // design, to avoid the popup misaligning from its anchor while the page
  // scrolls underneath. That's what caused the "dropdown closes itself"
  // reports. A plain absolutely-positioned list has no such built-in
  // dismiss-on-scroll behavior, so it stays open through an incidental page
  // scroll and only closes on an explicit outside click, Escape, or picking
  // an option, exactly the click-only interaction that was asked for.
  let hzMenuOpen = $state(false);
  let hzMenuEl = $state(null);
  let hzButtonEl = $state(null);

  function toggleHzMenu() {
    if (app.refreshRateBusy || app.displayModeBusy || app.displayInfo.refreshRates.length === 0) return;
    hzMenuOpen = !hzMenuOpen;
  }
  function closeHzMenu() {
    hzMenuOpen = false;
  }
  function pickHz(hz) {
    // Same dismissal-first behavior as pickResolution: swallow a click that
    // slipped through after a dismiss press, and dismiss the menu instead of
    // applying the item for keyboard activation.
    if (hzCtxDismissClick) {
      hzCtxDismissClick = false;
      return;
    }
    if (hzCtxMenu.open) {
      closeHzContextMenu();
      return;
    }
    closeHzMenu();
    changeRefreshRate({ currentTarget: { value: hz } });
  }

  $effect(() => {
    if (!hzMenuOpen) return;
    return dropdownCloseEffect(hzMenuEl, hzButtonEl, hzCtxMenuEl, closeHzMenu, hzButtonEl);
  });

  // The status-bar monitor selector lives in MonitorSelector.svelte
  // (trigger + collision-aware menu + per-monitor hover tooltip), styled
  // transparent like the resolution/Hz selects.

  // ── Right-click context menus for the resolution and refresh-rate
  // dropdowns. Mirror the calibration strip's .ctx-menu pattern in TabCrush.
  let resCtxMenu = $state({ open: false, x: 0, y: 0 });
  let resCtxMenuEl = $state(null);
  // Set when a press on the resolution list dismissed the context menu, so a
  // click that still fires (despite preventDefault) is swallowed by
  // pickResolution instead of applying the item under the cursor.
  let resCtxDismissClick = false;
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
    // button or on a row, the context menu overlays it instead of replacing
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
  let hzCtxDismissClick = false;
  function openHzContextMenu(e) {
    e.preventDefault();
    e.stopPropagation();
    if (app.displayInfo.refreshRates.length === 0) return;
    // See openResContextMenu, same stopPropagation caveat, so dismiss the
    // strip menu through the shared tick.
    dismissCtxMenus();
    closeResMenu();
    closeResContextMenu();
    hzCtxMenu = clampCtxMenu(e.clientX, e.clientY, 190, 190);
  }
  function repositionHzContextMenu(e) {
    e.preventDefault();
    e.stopPropagation();
    hzCtxMenu = clampCtxMenu(e.clientX, e.clientY, 190, 190);
  }
  function closeHzContextMenu() {
    hzCtxMenu = { ...hzCtxMenu, open: false };
  }

  // While a context menu overlays its dropdown, a press anywhere on the
  // dropdown list should only dismiss the context menu, never pick the item
  // under the cursor. This is a native capture-phase listener on document so
  // it runs before any element-level or document bubble handler (including
  // the context menu's own outside-click closers, which would otherwise close
  // the menu before the click could be attributed to it). Only the primary
  // button is intercepted: right-click keeps its open/reposition behavior.
  // preventDefault suppresses the click Chromium would synthesize for the
  // item button; the dismiss flags below are the safety net in case one still
  // fires. The flags are consumed by the pick functions and cleared on every
  // other press, so they can never block a legitimate pick.
  $effect(() => {
    function onDocPointerDownCapture(e) {
      const onResList = !!resMenuEl?.contains(e.target) && !resCtxMenuEl?.contains(e.target);
      const onHzList = !!hzMenuEl?.contains(e.target) && !hzCtxMenuEl?.contains(e.target);
      const resSwallow = resCtxMenu.open && onResList;
      const hzSwallow = hzCtxMenu.open && onHzList;
      if (e.button === 0 && (resSwallow || hzSwallow)) {
        resCtxDismissClick = resSwallow;
        hzCtxDismissClick = hzSwallow;
        e.preventDefault();
        e.stopPropagation();
        if (resSwallow) closeResContextMenu();
        if (hzSwallow) closeHzContextMenu();
      } else {
        resCtxDismissClick = false;
        hzCtxDismissClick = false;
      }
    }
    document.addEventListener('pointerdown', onDocPointerDownCapture, true);
    return () => document.removeEventListener('pointerdown', onDocPointerDownCapture, true);
  });

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
    const menuEl = resCtxMenuEl;
    if (menuEl) {
      const r = menuEl.getBoundingClientRect();
      const dx = Math.min(0, window.innerWidth - r.right - 8);
      const dy = Math.min(0, window.innerHeight - r.bottom - 8);
      if (dx || dy) resCtxMenu = { ...resCtxMenu, x: r.left + dx, y: r.top + dy };
    }
    const focusId = setTimeout(() => {
      resCtxMenuEl?.querySelector('[role="menuitem"]:not(:disabled)')?.focus();
    }, 0);
    const cleanup = ctxMenuCloseEffect(resCtxMenuEl, closeResContextMenu);
    return () => { clearTimeout(focusId); cleanup(); };
  });

  $effect(() => {
    if (!hzCtxMenu.open) return;
    const menuEl = hzCtxMenuEl;
    if (menuEl) {
      const r = menuEl.getBoundingClientRect();
      const dx = Math.min(0, window.innerWidth - r.right - 8);
      const dy = Math.min(0, window.innerHeight - r.bottom - 8);
      if (dx || dy) hzCtxMenu = { ...hzCtxMenu, x: r.left + dx, y: r.top + dy };
    }
    const focusId = setTimeout(() => {
      hzCtxMenuEl?.querySelector('[role="menuitem"]:not(:disabled)')?.focus();
    }, 0);
    const cleanup = ctxMenuCloseEffect(hzCtxMenuEl, closeHzContextMenu);
    return () => { clearTimeout(focusId); cleanup(); };
  });

  // Toggling real Windows HDR (Settings > Display > HDR) via the backend.
  // Detection already exists (app.hdrStatus, driven by pollHDRStatus's
  // matchMedia check below), this just adds the write side.
  let windowsHdrBusy = $state(false);
  let windowsHdrError = $state('');
  let refreshRateError = $state('');
  let resolutionError = $state('');

  // Show the flyout confirmation dialog centered on screen.
  // Tauri 2 commands expect camelCase argument keys (refreshHz, monitorName,
  // confirmationSeconds), not snake_case, or the invoke is rejected.
  function showFlyoutConfirmation(result) {
    invoke('show_mode_confirmation', {
      width: result.width,
      height: result.height,
      refreshHz: result.refresh_hz,
      monitorName: result.monitor_name,
      confirmationSeconds: result.confirmation_seconds,
      // The absolute deadline computed by the backend when the mode was
      // applied; the flyout counts down to it so the confirmation expires at
      // the exact moment the automatic rollback fires, no matter how long the
      // show handshake took.
      confirmationDeadlineMs: result.confirmation_deadline_ms,
    }).catch((e) => {
      resolutionError = String(e);
      app.displayModeBusy = false;
      app.refreshRateBusy = false;
      pollDisplayInfo();
    });
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
    // but is marketed and commonly labeled as 21:9, nominally 2.333.
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

  // Popularity rank for common display aspect ratios, most-used first.
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

  // Shared safe apply path for the resolution dropdown and the favorites
  // menu: requests a mode at (width, height) with a preferred refresh rate
  // through set_resolution (validated against the target monitor, returns a
  // pending confirmation with automatic rollback). preferredHz 0 keeps the
  // current rate, matching the resolution dropdown's behavior. deviceName is
  // null for the primary monitor (the dropdown path); per-monitor favorites
  // pass the GDI device name of the monitor they were saved on.
  async function applyModeCombo(width, height, preferredHz, deviceName = null, skipConfirmation = false, favoriteId = null) {
    const primaryTarget = !deviceName;
    const currentWidth = app.displayInfo.width;
    const currentHeight = app.displayInfo.height;
    if (!Number.isFinite(width) || !Number.isFinite(height)
      || width <= 0 || height <= 0 || app.displayModeBusy
      || app.refreshRateBusy
      // The no-op guard only applies to the primary path, where the status
      // bar's current mode is the target. A non-primary favorite is compared
      // against its own monitor's live mode before applying (see
      // applyFavoriteMode).
      || (primaryTarget && width === currentWidth && height === currentHeight && preferredHz === (app.displayInfo.refreshRateHz || 0))) return;

    app.displayModeBusy = true;
    // Save the profile for the mode we are leaving before the optimistic UI
    // update changes the resolution fields used by the scoped profile key.
    persistCurrentBlackLevelProfile();
    // The status bar reflects the primary monitor; a non-primary favorite
    // must not pretend the primary changed.
    if (primaryTarget) {
      app.displayInfo.width = width;
      app.displayInfo.height = height;
    }
    resolutionError = '';
    try {
      const result = await invoke('set_resolution', {
        width,
        height,
        preferredHz,
        deviceName,
        skipConfirmation,
        // Favorite id only, when this apply came from a favorite. The backend
        // arms the trusted-favorite credit with it and records it on a Keep
        // (see favorite_credit.rs).
        favoriteId: favoriteId || null,
      });
      // The backend only returns after the temporary mode has stabilized, so
      // load the profile for the newly active display/mode immediately.
      applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
      if (skipConfirmation) {
        app.displayModeBusy = false;
        app.refreshRateBusy = false;
        pollDisplayInfo();
      } else {
        showFlyoutConfirmation(result);
      }
    } catch (e) {
      resolutionError = String(e);
      app.displayInfo.width = currentWidth;
      app.displayInfo.height = currentHeight;
      applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
      app.displayModeBusy = false;
      pollDisplayInfo();
    }
  }

  async function changeResolution(event) {
    const [width, height] = String(event.currentTarget.value).split('x').map(Number);
    await applyModeCombo(width, height, app.displayInfo.refreshRateHz || 0);
  }

  async function changeRefreshRate(event) {
    const hz = Number(event.currentTarget.value);
    const currentHz = app.displayInfo.refreshRateHz;
    if (!Number.isFinite(hz) || hz <= 0 || app.refreshRateBusy || app.displayModeBusy || hz === currentHz) return;

    // Save the profile for the mode we are leaving before changing the mode.
    persistCurrentBlackLevelProfile();
    app.refreshRateBusy = true;
    app.displayInfo.refreshRateHz = hz;
    refreshRateError = '';
    try {
      const result = await invoke('set_refresh_rate', { hz });
      showFlyoutConfirmation(result);
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
      app.refreshRateBusy = false;
      pollDisplayInfo();
    }
  }

  // ── Status-bar favorites menu (favorite resolution@hz combos) ─────
  let favoritesMenuOpen = $state(false);
  let favoritesMenuEl = $state(null);
  let favoritesBtnEl = $state(null);
  const favoriteOfCurrent = $derived({
    width: app.displayInfo.width,
    height: app.displayInfo.height,
    hz: app.displayInfo.refreshRateHz || 0,
  });
  // Favorites are scoped to the currently selected monitor (the top-bar
  // selector); the mode being saved comes from the status bar's display info.
  function currentMonitorSig() {
    return activeMonitor.mon?.sig || '';
  }
  function currentMonitorId() {
    return activeMonitor.mon?.monitor_id || '';
  }
  function currentMonitorName() {
    return activeMonitor.mon?.friendly_name || '';
  }
  function currentMonitorDevice() {
    return activeMonitor.mon?.device_name || '';
  }
  const isCurrentFavorited = $derived(
    favoriteOfCurrent.width > 0 && favoriteOfCurrent.height > 0 && favoriteOfCurrent.hz > 0
      && isDisplayModeFavorited(favoriteOfCurrent.width, favoriteOfCurrent.height, favoriteOfCurrent.hz, currentMonitorId(), currentMonitorSig())
  );
  const currentComboLabel = $derived(
    favoriteOfCurrent.width > 0 && favoriteOfCurrent.height > 0 && favoriteOfCurrent.hz > 0
      ? `${favoriteOfCurrent.width}×${favoriteOfCurrent.height} @ ${favoriteOfCurrent.hz} Hz`
      : ''
  );
  const isFavoriteCurrent = (fav) => fav.width === favoriteOfCurrent.width
    && fav.height === favoriteOfCurrent.height
    && fav.hz === favoriteOfCurrent.hz;
  function toggleFavoritesMenu() {
    clearTimeout(favoritesHoverTimer);
    favoritesMenuOpen = !favoritesMenuOpen;
  }
  function closeFavoritesMenu() {
    clearTimeout(favoritesHoverTimer);
    favoritesMenuOpen = false;
  }
  // Close every open dropdown/context menu in one go. Called when the window
  // hides to the tray or loses focus (minimize, backgrounded): a dropdown
  // left open would otherwise still be open, stale and floating over the
  // restored window, when the user comes back.
  function closeAllMenus() {
    resMenuOpen = false;
    hzMenuOpen = false;
    favoritesMenuOpen = false;
    if (resCtxMenu.open) closeResContextMenu();
    if (hzCtxMenu.open) closeHzContextMenu();
  }
  // Hover-open for the favorites menu: a short delay so a quick pass over the
  // star in the status bar does not flash the menu, and a grace period on
  // leave so moving the pointer from the icon into the menu itself (or across
  // its edge) does not close it. The menu is a child of .favorites-wrap, so
  // pointerenter/pointerleave on the wrap cover the icon and the open menu.
  let favoritesHoverTimer = null;
  function favoritesCanOpen() {
    return !app.displayModeBusy && !app.refreshRateBusy;
  }
  function openFavoritesOnHover() {
    if (!favoritesCanOpen()) return;
    clearTimeout(favoritesHoverTimer);
    favoritesHoverTimer = setTimeout(() => { favoritesMenuOpen = true; }, 120);
  }
  function closeFavoritesOnHoverLeave() {
    clearTimeout(favoritesHoverTimer);
    favoritesHoverTimer = setTimeout(() => { favoritesMenuOpen = false; }, 200);
  }
  // Transient confirmation shown in the status bar to the left of the
  // favorites icon. Appears only while a favorite is actually added or
  // removed, then auto-clears after a short delay (same pattern as the HDR
  // tab's transient "Applied" messages).
  let favoritesStatusMsg = $state('');
  let favoritesStatusIsRemove = $state(false);
  let favoritesStatusTimer = null;
  function flashFavoritesStatus(text, isRemove) {
    favoritesStatusMsg = text;
    favoritesStatusIsRemove = isRemove;
    clearTimeout(favoritesStatusTimer);
    favoritesStatusTimer = setTimeout(() => {
      favoritesStatusMsg = '';
      favoritesStatusIsRemove = false;
    }, 2000);
  }
  function saveCurrentFavorite() {
    addFavoriteDisplayMode(
      favoriteOfCurrent.width,
      favoriteOfCurrent.height,
      favoriteOfCurrent.hz,
      currentMonitorSig(),
      currentMonitorName(),
      currentMonitorId(),
      currentMonitorDevice(),
    );
    flashFavoritesStatus('Saved', false);
  }
  // The resolution-independent portion of a monitor sig ("3840x2160|
  // 1600x900|192x192" -> "1600x900|192x192"): physical size + DPI do not
  // change when the mode changes, so a legacy favorite saved without an EDID
  // monitor_id can still be matched to its panel after a resolution switch.
  function stableSigPart(sig) {
    const parts = String(sig || '').split('|');
    return parts.length >= 3 ? `${parts[1]}|${parts[2]}` : String(sig || '');
  }
  // Resolve a favorite's target monitor at apply time. Returns null for the
  // primary (legacy favorites saved without a monitor), a device name string
  // for a connected monitor, or 'disconnected' when the favorite's monitor is
  // not currently connected. The EDID monitor_id is the primary key (it is
  // stable across resolution changes); the sig and its physical-size+DPI
  // portion are fallbacks for legacy favorites.
  function favoriteTargetDevice(fav) {
    if (!fav.monitorId && !fav.monitorSig && !fav.monitorDevice) return null;
    // The GDI device name is the unique per-physical-monitor identity: two
    // identical panels share the EDID id and (at the same resolution) the
    // sig, so the device name must win when the favorite carries one, or an
    // apply would hit the first matching sibling instead of the saved panel.
    const mon = monitors.find((m) =>
      (fav.monitorDevice && m.device_name === fav.monitorDevice)
      || (fav.monitorId && m.monitor_id === fav.monitorId)
      || m.sig === fav.monitorSig
      || (fav.monitorSig && stableSigPart(m.sig) === stableSigPart(fav.monitorSig)));
    return mon && mon.device_name ? mon.device_name : 'disconnected';
  }
  function favoriteMonitorConnected(fav) {
    return favoriteTargetDevice(fav) !== 'disconnected';
  }
  // Monitor tag shown next to a favorite: only when it differs from the
  // currently selected monitor (mirrors how resolution tags are shown), or
  // when the monitor is disconnected, so the entry stays identifiable.
  function favoriteMonitorLabel(fav) {
    const name = fav.monitorName || '';
    if (favoriteTargetDevice(fav) === 'disconnected') {
      return name ? `${name} (not connected)` : 'Monitor not connected';
    }
    if (name && name !== currentMonitorName()) return name;
    return '';
  }
  // Shared apply path for the favorites menu and the favorite hotkey slots:
  // resolves the saved monitor, skips no-op applies (the monitor already runs
  // this exact mode), and flashes a status message when the target is gone.
  async function applyFavoriteMode(fav) {
    const deviceName = favoriteTargetDevice(fav);
    const skipConfirmation = appSettings.skipFavoriteConfirmation
      && Boolean(fav.confirmedMonitorId)
      && Boolean(fav.monitorId)
      && fav.confirmedMonitorId === fav.monitorId;
    if (deviceName === 'disconnected') {
      flashFavoritesStatus('Monitor not connected', true);
      return;
    }
    // A favorite for a non-primary monitor is compared against that monitor's
    // live mode so pressing the hotkey for an already-active mode does not
    // open a pointless confirmation dialog.
    if (deviceName) {
      try {
        const info = await invoke('get_display_info_for', { deviceName });
        if (info && Number(info.width) === fav.width && Number(info.height) === fav.height
          && (Number(info.refresh_hz) || 0) === fav.hz) {
          return;
        }
      } catch {
        // The backend validates the mode anyway; never block on this check.
      }
    }
    // The backend credits this favorite as trusted when the resulting Keep
    // succeeds, from any UI, so pass its stable id down (see favorite_credit.rs).
    await applyModeCombo(fav.width, fav.height, fav.hz, deviceName, skipConfirmation, fav.favId || null);
  }
  function applyFavorite(fav) {
    closeFavoritesMenu();
    void applyFavoriteMode(fav);
  }
  function removeFavorite(fav) {
    removeFavoriteDisplayMode(fav.width, fav.height, fav.hz, fav.monitorId, fav.monitorSig);
    flashFavoritesStatus('Removed', true);
  }
  // Confirmation dialog before removing a saved favorite, reusing the app's
  // modal styles. Removing is destructive (the entry is gone until re-saved),
  // so a stray star click should not drop it silently.
  let favoritePendingRemove = $state(null);
  let favoriteConfirmCancelEl = $state(null);
  function askRemoveFavorite(fav) { favoritePendingRemove = fav; }
  function cancelRemoveFavorite() { favoritePendingRemove = null; }
  function confirmRemoveFavorite() {
    if (!favoritePendingRemove) return;
    removeFavorite(favoritePendingRemove);
    favoritePendingRemove = null;
  }
  // Trap Tab between the two dialog buttons so focus never escapes to the
  // page behind the backdrop, mirroring ConfirmModal's trap.
  function trapModalTab(e) {
    if (e.key !== 'Tab') return;
    const buttons = e.currentTarget ? [...e.currentTarget.querySelectorAll('button:not(:disabled)')] : [];
    if (buttons.length === 0) return;
    const first = buttons[0];
    const last = buttons[buttons.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }
  function onFavoriteConfirmKeydown(e) {
    if (e.key === 'Escape') {
      e.preventDefault();
      cancelRemoveFavorite();
      return;
    }
    trapModalTab(e);
  }
  // Focus the safe action (Cancel) when the dialog opens, like the mode
  // confirm modal defaults to Revert so Enter/Escape do the safe thing.
  $effect(() => {
    if (favoritePendingRemove) favoriteConfirmCancelEl?.focus();
  });
  // Star icon on each resolution row: favorites that resolution at the
  // current refresh rate, or removes it if already favorited. Adding is
  // instant; removing goes through the same confirmation dialog as the
  // favorites menu so a stray click does not drop a saved combo.
  function toggleResolutionFavorite(resolution) {
    const hz = app.displayInfo.refreshRateHz || 0;
    if (hz <= 0) return;
    const sig = currentMonitorSig();
    const id = currentMonitorId();
    if (isDisplayModeFavorited(resolution.width, resolution.height, hz, id, sig)) {
      askRemoveFavorite({ width: resolution.width, height: resolution.height, hz, monitorId: id, monitorSig: sig });
    } else {
      addFavoriteDisplayMode(resolution.width, resolution.height, hz, sig, currentMonitorName(), id);
      flashFavoritesStatus('Saved', false);
    }
  }

  $effect(() => {
    if (!favoritesMenuOpen) return;
    return dropdownCloseEffect(favoritesMenuEl, favoritesBtnEl, null, closeFavoritesMenu, favoritesBtnEl);
  });

  async function toggleWindowsHdr() {
    if (windowsHdrBusy || app.hdrStatus === 'unknown') return;
    const next = app.hdrStatus !== 'hdr';
    const previous = app.hdrStatus;
    windowsHdrBusy = true;
    // Reflect the switch instantly instead of waiting for the next 1s poll
    // tick to pick it up via matchMedia, that wait is what made toggling
    // from inside the app feel slower than the Windows keyboard shortcut,
    // even though the underlying OS-level change completes at the same speed
    // either way. Reverted in the catch block if the backend call fails.
    app.hdrStatus = next ? 'hdr' : 'sdr';
    try {
      await invoke('set_windows_hdr_enabled', { enabled: next });
      windowsHdrError = '';
      // Enabling HDR makes the WebView re-read the display and its frozen SDR
      // white level, so the Brightness strip's base must follow or it drifts
      // (the user's "toggling HDR messes up the calibration" report). Harmless
      // when disabling HDR (strip renders SDR then), but re-capturing always
      // also covers a subsequent re-enable.
      refreshSdrWhiteLevel(true);
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
  // The strip's own keyboard shortcuts are unaffected, these only fire for
  // the registered global bindings.
  let hotkeySaveTimer = null;

  function applyGammaForHotkey() {
    applyGamma(blackLift()).catch((e) => console.warn('[hotkey] gamma apply failed', e));
  }

  // SDR Content Brightness is a Windows setting, not an OLED Helper setting.
  // Read it immediately before each hotkey step so the action remains correct
  // when Windows Settings or another tool changed it while the app was hidden.
  // Queue repeated WM_HOTKEY events rather than allowing overlapping reads and
  // writes to race while the user holds the shortcut.
  const SDR_BRIGHTNESS_STEP_NITS = 50;
  const SDR_BRIGHTNESS_MIN_NITS = 80;
  const SDR_BRIGHTNESS_MAX_NITS = 480;
  let pendingSdrBrightnessDelta = 0;
  let sdrBrightnessHotkeyBusy = false;

  async function drainSdrBrightnessHotkeys() {
    if (sdrBrightnessHotkeyBusy) return;
    sdrBrightnessHotkeyBusy = true;
    try {
      while (pendingSdrBrightnessDelta !== 0) {
        const delta = pendingSdrBrightnessDelta;
        pendingSdrBrightnessDelta = 0;
        try {
          const info = await invoke('get_sdr_white_level');
          const current = Number(info?.nits);
          if (!Number.isFinite(current)) continue;
          const target = Math.max(
            SDR_BRIGHTNESS_MIN_NITS,
            Math.min(SDR_BRIGHTNESS_MAX_NITS, current + delta),
          );
          if (target === current) continue;
          const result = await invoke('set_sdr_brightness_nits', { nits: target });
          const applied = Number(result?.nits);
          if (Number.isFinite(applied)) {
            setSdrWhiteNits(applied);
            emit('sdr-content-brightness-main-changed', { nits: applied }).catch(() => {});
          }
        } catch (error) {
          console.warn('[sdr-brightness-hotkey] update failed', error);
        }
      }
    } finally {
      sdrBrightnessHotkeyBusy = false;
      // Handle an event that arrived between the final loop check and the
      // busy flag being cleared without recursively awaiting from the event.
      if (pendingSdrBrightnessDelta !== 0) void drainSdrBrightnessHotkeys();
    }
  }

  function adjustSdrBrightnessForHotkey(direction) {
    pendingSdrBrightnessDelta += direction * SDR_BRIGHTNESS_STEP_NITS;
    void drainSdrBrightnessHotkeys();
  }

  function scheduleHotkeyConfigSave() {
    clearTimeout(hotkeySaveTimer);
    hotkeySaveTimer = setTimeout(() => saveConfig(), 400);
  }

  // ── Persistent HDR presentation layer ─────────────────────────────
  // The WebView window is composited through ONE of two paths depending on
  // whether the DOM contains an HDR WebGPU surface: with one present,
  // Chromium presents the whole window via the HDR path (the SDR chrome is
  // scaled by the frozen SDR-white reference); without one, the window is a
  // plain SDR swapchain and Windows scales the chrome by the LIVE SDR
  // slider. The calibration tabs own HDR canvases (Crush, Windows HDR), and
  // leaving those tabs destroys them via onDestroy, so the window flips to
  // SDR presentation and the whole interface visibly brightens. This
  // always-mounted 1×1 HDR canvas keeps the window in the HDR path for the
  // entire session, making the chrome brightness uniform across all tabs.
  const HDR_LAYER_SHADER = `struct VO{@builtin(position)p:vec4f}
  @vertex fn vs(@builtin(vertex_index)i:u32)->VO{
    let pos=array(vec2f(-1,-1),vec2f(3,-1),vec2f(-1,3));
    return VO(vec4f(pos[i],0,1));
  }
  @fragment fn fs()->@location(0)vec4f{return vec4f(1.5,1.5,1.5,1.0);}`;

  let hdrLayerCanvas = $state(null);
  let hdrLayerInitStarted = false;
  let hdrLayerDevice = null;
  let hdrLayerCtx = null;
  let hdrLayerModule = null;
  let hdrLayerLayout = null;
  let hdrLayerBg = null;
  let hdrLayerPipe = null;
  let hdrLayerFormat = null;
  let hdrLayerRetryCount = 0;

  async function initHdrLayer() {
    if (hdrLayerInitStarted) return;
    hdrLayerInitStarted = true;
    try {
      if (!navigator.gpu) return;
      const adapter = await navigator.gpu.requestAdapter();
      if (!adapter) return;
      hdrLayerDevice = await adapter.requestDevice();
      const ctx = hdrLayerCanvas.getContext('webgpu');
      if (!ctx) { hdrLayerDevice.destroy(); hdrLayerDevice = null; return; }
      hdrLayerCtx = ctx;
      hdrLayerModule = hdrLayerDevice.createShaderModule({ code: HDR_LAYER_SHADER });
      const bgl = hdrLayerDevice.createBindGroupLayout({ entries: [] });
      hdrLayerLayout = hdrLayerDevice.createPipelineLayout({ bindGroupLayouts: [bgl] });
      hdrLayerBg = hdrLayerDevice.createBindGroup({ layout: bgl, entries: [] });
      // Transient device losses (driver reset, GPU re-init) would silently
      // drop the persistent layer, the exact failure this feature exists to
      // prevent. Tear down and re-init once the hardware recovers (bounded
      // retries so a permanently lost GPU can't spawn a retry loop).
      hdrLayerDevice.lost.then((info) => {
        console.warn('[gpu] persistent HDR layer device lost', info?.message || '');
        hdrLayerDevice = null;
        hdrLayerCtx = null;
        hdrLayerFormat = null;
        hdrLayerPipe = null;
        if (hdrLayerRetryCount < 2) {
          hdrLayerRetryCount++;
          hdrLayerInitStarted = false;
          setTimeout(() => { if (hdrLayerCanvas) initHdrLayer(); }, 1000);
        }
      }).catch(() => {});
      configureHdrLayer();
    } catch (e) {
      console.warn('[gpu] persistent HDR layer init failed', e);
    }
  }

  function configureHdrLayer() {
    if (!hdrLayerDevice || !hdrLayerCtx) return;
    try {
      const wantHdr = app.hdrStatus === 'hdr';
      let format = wantHdr ? 'rgba16float' : 'rgba8unorm';
      let colorSpace = wantHdr ? 'display-p3' : 'srgb';
      if (format !== hdrLayerFormat) {
        try {
          hdrLayerCtx.configure({ device: hdrLayerDevice, format, usage: GPUTextureUsage.RENDER_ATTACHMENT, colorSpace });
        } catch (e) {
          if (!wantHdr) throw e;
          format = 'rgba8unorm';
          colorSpace = 'srgb';
          hdrLayerCtx.configure({ device: hdrLayerDevice, format, usage: GPUTextureUsage.RENDER_ATTACHMENT, colorSpace });
        }
        hdrLayerFormat = format;
        hdrLayerPipe = hdrLayerDevice.createRenderPipeline({
          layout: hdrLayerLayout,
          vertex: { module: hdrLayerModule, entryPoint: 'vs' },
          fragment: { module: hdrLayerModule, entryPoint: 'fs', targets: [{ format }] },
          primitive: { topology: 'triangle-list' },
        });
      }
      // Render a bright extended-range pixel once so the surface genuinely
      // carries HDR content, a transparent/black 1×1 could be optimized away.
      hdrLayerCanvas.width = 1;
      hdrLayerCanvas.height = 1;
      const tex = hdrLayerCtx.getCurrentTexture();
      const enc = hdrLayerDevice.createCommandEncoder();
      const pass = enc.beginRenderPass({ colorAttachments: [{ view: tex.createView(), loadOp: 'clear', storeOp: 'store', clearValue: [0, 0, 0, 1] }] });
      pass.setPipeline(hdrLayerPipe); pass.setBindGroup(0, hdrLayerBg); pass.draw(3); pass.end();
      hdrLayerDevice.queue.submit([enc.finish()]);
    } catch (e) {
      console.warn('[gpu] persistent HDR layer configure failed', e);
    }
  }

  $effect(() => {
    if (hdrLayerCanvas && !hdrLayerInitStarted) initHdrLayer();
  });
  // Follow HDR toggles (and the initial unknown→hdr transition) so the
  // persistent layer tracks the same format/colorSpace as the calibration
  // canvases. Re-renders when the window becomes visible again too, the
  // swapchain can be recreated while the app is hidden in the tray.
  $effect(() => {
    void app.hdrStatus; void windowVisible;
    if (hdrLayerCanvas && hdrLayerInitStarted) configureHdrLayer();
  });

  function emitBlackLevelMainChanged() {
    emit('black-level-main-changed', { adjustment: crush.adjustment, enabled: crush.blackEnabled, monitorKey: monProfileId(activeMonitor.mon) }).catch(() => {});
  }

  // Sidebar mini-switches: each tab with a master toggle gets a small switch
  // at the right edge of its nav item so the state is visible and flippable
  // without opening the tab. Each mirrors the tab's own toggle path.
  function toggleBlackCrushFromNav() {
    crush.blackEnabled = !crush.blackEnabled;
    applyGammaForHotkey();
    scheduleHotkeyConfigSave();
    emitBlackLevelMainChanged();
  }

  function toggleTaskbarDimmerFromNav() {
    // The nav switch is the global convenience: it flips every monitor's
    // taskbar dimmer at once, keeping each monitor's own opacity/fades.
    setTaskbarDimmerEnabledForAll(!taskbarDimmer.enabled);
  }

  function toggleScreenDimmerFromNav() {
    setScreenDimmerEnabled(!screenDimmer.enabled, app.activeTab)
      .catch((e) => console.warn('[sidebar] screen dimmer toggle failed', e));
  }

  // One-time setup of the elevated helper task (UAC prompt on first use,
  // or again after the portable exe moved). The warning glyph next to the
  // nav switch calls this directly so the user can fix a missing or stale
  // registration without toggling the dimmer off and on.
  function setupScreenDimmerHelperFromNav() {
    invoke('ensure_helper_task')
      .then(() => { screenDimmerHelper.ready = true; })
      .catch((e) => console.warn('[sidebar] screen dimmer helper setup failed', e));
  }

  function handleHotkeyAction(action) {
    switch (action) {
      case 'toggle_black_crush':
        crush.blackEnabled = !crush.blackEnabled;
        applyGammaForHotkey();
        scheduleHotkeyConfigSave();
        emitBlackLevelMainChanged();
        break;
      case 'decrease_black_crush':
        crush.adjustment = Math.max(-150, crush.adjustment - 10);
        applyGammaForHotkey();
        scheduleHotkeyConfigSave();
        emitBlackLevelMainChanged();
        break;
      case 'increase_black_crush':
        crush.adjustment = Math.min(150, crush.adjustment + 10);
        applyGammaForHotkey();
        scheduleHotkeyConfigSave();
        emitBlackLevelMainChanged();
        break;
      case 'toggle_taskbar_dimmer':
        // Hotkey follows the bottom-bar monitor selection, like the Black
        // Crush hotkeys, so it toggles the selected monitor's own overlay.
        setTaskbarDimmer(!taskbarDimmer.enabled);
        break;
      case 'toggle_screen_dimmer':
        setScreenDimmerEnabled(!screenDimmer.enabled, app.activeTab)
          .catch((e) => console.warn('[hotkey] screen dimmer toggle failed', e));
        break;
      case 'toggle_hdr':
        toggleWindowsHdr();
        break;
      case 'decrease_sdr_brightness':
        adjustSdrBrightnessForHotkey(-1);
        break;
      case 'increase_sdr_brightness':
        adjustSdrBrightnessForHotkey(1);
        break;
      case 'turn_off_display':
        invoke('power_turn_off_display').catch(() => {});
        break;
      default: {
        // Per-favorite resolution hotkeys (action name embeds the favorite's
        // id): apply the favorite to the monitor it was saved on. No-ops
        // safely when the favorite no longer exists; a disconnected monitor
        // flashes a status message via applyFavoriteMode.
        const fav = favoriteForAction(action);
        if (fav) void applyFavoriteMode(fav);
        break;
      }
    }
  }

  onMount(() => {
    // An elevated RTX HDR restart carries --open-tab=<tab> so the user lands
    // back where they were; consume it once (cleared after first read).
    invoke('take_startup_tab')
      .then((tab) => {
        if (typeof tab === 'string' && tab) app.activeTab = tab;
      })
      .catch(() => {});
    // One-time: seed display info and HDR status. Mode changes re-poll
    // explicitly afterward, there is no periodic poll on the main thread.
    pollDisplayInfo();
    pollHDRStatus();
    // Seed the SDR white level used by the Brightness strip compensation
    // (Chromium maps the HDR calibration canvas relative to this value).
    refreshSdrWhiteLevel();
    // Load the monitor list for the top-bar per-monitor selector; a saved
    // selection is restored in loadConfig.
    loadMonitors().catch(() => {});
    // Read the runtime version for the status bar (keeps it in sync with
    // the version the updater compares against).
    getVersion().then((v) => { if (typeof v === 'string' && v) appVersion = v; }).catch(() => {});
    // Silent startup update check; failure never disturbs the UI.
    checkForUpdates();
    // Whether the Screen Dimmer's one-time elevated helper is set up; when
    // the dimmer is on but the helper is missing or stale (portable exe
    // moved), the nav switch shows a warning glyph to re-register it.
    refreshScreenDimmerHelperReady();

    let unlisten;
    let unlistenDisplayChanged;
    let unlistenHotkeyPressed;
    // The Rust window subclass forwards WM_DISPLAYCHANGE and WM_DEVICECHANGE.
    // Full display-info queries remain event-driven. Vendor control panels can
    // silently change range/VRR, so a separate low-frequency vendor-only
    // refresh below handles that case without touching mode lists or EDID.
    listen('display-changed', () => {
      scheduleDisplayInfoRefresh();
      // Hotplug/mode changes can add or remove monitors; keep the selector
      // list in sync so a stale entry is never shown.
      const selectedSigBefore = activeMonitor.mon?.sig || '';
      loadMonitors()
        .then((loaded) => {
          // A mode change on the SELECTED monitor (including a per-monitor
          // favorite hotkey on a secondary display) changes its sig, which
          // re-keys its per-mode black-lift profile. The apply right after
          // set_resolution runs with the stale sig and is skipped, so apply
          // again now that the list reflects the new mode. Guarded on the sig
          // actually changing, so the noisy settings broadcasts that also
          // reach this listener never disturb the screen dimmer's active
          // ramp with a redundant gamma write.
          if (loaded && selectedSigBefore && activeMonitor.mon?.sig && selectedSigBefore !== activeMonitor.mon.sig
            && configLoaded.v && app.displayInfo.refreshRateHz) {
            applyBlackLevelProfileForRefreshRate(app.displayInfo.refreshRateHz);
          }
        })
        .catch(() => {});
      // A mode change can briefly make the full display query incomplete, and
      // that query rebuilds displayInfo. Re-check the lightweight GPU payload
      // after the driver has settled so NVIDIA-only controls do not depend on
      // visiting another tab to repopulate the vendor field.
      clearTimeout(vendorDisplayRefreshTimer);
      vendorDisplayRefreshTimer = setTimeout(() => {
        vendorDisplayRefreshTimer = null;
        refreshVendorDisplayInfo();
      }, 350);
      // Display topology changes (incl. HDR toggles, resolution/hotplug)
      // make the WebView re-read its display info, including the SDR white
      // level it froze at startup, so re-capture the strip's base to match.
      // Debounced: hotplug emits a burst of WM_* messages, and waiting lets
      // the OS settle before the new value is read. A plain SDR-brightness
      // slider move does NOT emit this event, so the base stays frozen there
      // (exactly what keeps the strip stable).
      // Reconfirm the HDR state after the remote display and WebView context
      // have settled. This also publishes the result to the separate tray
      // WebView, which may miss its own matchMedia transition event.
      clearTimeout(hdrStatusRefreshTimer);
      hdrStatusRefreshTimer = setTimeout(() => {
        hdrStatusRefreshTimer = null;
        pollHDRStatus();
      }, 400);
      clearTimeout(sdrWhiteBaseRecaptureTimer);
      sdrWhiteBaseRecaptureTimer = setTimeout(() => {
        sdrWhiteBaseRecaptureTimer = null;
        refreshSdrWhiteLevel(true);
      }, 150);
      // A display topology change (driver restart, hotplug, sleep/resume)
      // can recover a lost GPU device. Reset the persistent HDR layer
      // retry budget so it is re-created on the next frame.
      hdrLayerRetryCount = 0;
      hdrLayerInitStarted = false;
    }).then((stop) => { unlistenDisplayChanged = stop; }).catch(() => {});
    // Central handler for the window's shown/hidden state. Rust's
    // window-visibility event is the fast path for tray show/hide, but a
    // restore that happens outside that path (taskbar click after minimize,
    // or a dropped event) never emits it, so `windowVisible`, the flag that
    // gates the active tab's markup, can go stale and leave the tab area
    // blank. Routing the state change through here (and re-checking the real
    // state on window focus below) makes the flag self-heal.
    function applyWindowVisible(visible, animated = false) {
      const wasVisible = windowVisible;
      windowVisible = visible;
      if (!visible) {
        // Window hid to the tray (or a self-heal read found it hidden):
        // close any open dropdown so it never greets the user on restore.
        closeAllMenus();
        scheduleVisibilityReconcile();
        return;
      }
      if (wasVisible) return;
      if (visibilityReconcileTimer) {
        clearTimeout(visibilityReconcileTimer);
        visibilityReconcileTimer = null;
      }
      // Stamp the restore moment. The Text tab's native preview reads this
      // on mount and holds itself hidden until the window's restore
      // animation has had time to finish, so the text sample never appears
      // over the transition. The hold only applies when the restore actually
      // animates (the window was minimized), not for an instant close-to-tray
      // show.
      app.windowRestoredAt = Date.now();
      app.windowRestoreAnimated = !!animated;
      app.windowShownAt = Date.now();
      // The strip selection is session-only: always land back on the Black
      // strip when the window is restored, so a leftover Brightness pick
      // never greets the user after returning from the tray.
      crush.stripType = 'black';
      // Re-query display info when the window is shown again (e.g. after
      // sleep/resume while hidden in the tray). The cached mode can be
      // stale or empty from a transitional query during wake; this makes
      // it self-heal without a periodic poll.
      pollDisplayInfo();
      refreshSdrWhiteLevel();
      hdrLayerRetryCount = 0;
      hdrLayerInitStarted = false;
      // Silent update check whenever the app is restored from the tray
      // (cooldown throttles it). Never disturbs the UI on failure.
      checkForUpdates();
    }
    listen('window-visibility', (event) => {
      const payload = event.payload;
      const object = payload !== null && typeof payload === 'object';
      applyWindowVisible(
        object ? payload.visible !== false : payload !== false,
        object ? payload.animated === true : false,
      );
    }).then((stop) => { unlisten = stop; }).catch(() => {});
    // Self-heal: a restore that doesn't go through show_main_window (e.g.
    // clicking the taskbar button after minimizing) fires window focus but no
    // window-visibility event, which would leave windowVisible false and the
    // active tab blank. Re-read the real visibility from Rust on focus so the
    // flag always converges to the actual state.
    function reconcileWindowVisible() {
      invoke('is_main_window_visible')
        .then((visible) => { applyWindowVisible(!!visible); })
        .catch(() => {});
    }
    window.addEventListener('focus', reconcileWindowVisible);
    // The focus-based reconcile above cannot fire when the window stays
    // visible and focused, e.g. a close-to-tray whose hide did not take
    // effect: the backend emitted window-visibility false, this flag went
    // false and unmounted the tab, but the window is still on screen. While
    // the flag is false but the document is genuinely visible, poll the real
    // state until it converges. A real close-to-tray flips
    // document.visibilityState to hidden, which pauses the poll, so nothing
    // runs while the app sits in the tray.
    let visibilityReconcileTimer = null;
    function scheduleVisibilityReconcile() {
      if (visibilityReconcileTimer) return;
      visibilityReconcileTimer = setTimeout(() => {
        visibilityReconcileTimer = null;
        if (windowVisible || document.visibilityState !== 'visible') return;
        reconcileWindowVisible();
        scheduleVisibilityReconcile();
      }, 500);
    }
    // A dropdown left open when the window is minimized or backgrounded
    // (another app focused) would still be open, and stale, when the user
    // comes back. The window-visibility event covers the close-to-tray path
    // (applyWindowVisible above); blur and visibilitychange cover minimize
    // and backgrounding, where no Rust event is emitted.
    window.addEventListener('blur', closeAllMenus);
    function onDocumentVisibilityChange() {
      if (document.visibilityState === 'hidden') closeAllMenus();
    }
    document.addEventListener('visibilitychange', onDocumentVisibilityChange);
    // Global hotkey dispatch (see hotkeys.rs). Payload is the action name.
    listen('hotkey-pressed', (event) => handleHotkeyAction(event.payload)).then((stop) => { unlistenHotkeyPressed = stop; }).catch(() => {});
    // A favorite removal requested from the Hotkeys tab (its slot rows have a
    // remove button). Opens the same confirmation dialog as the status-bar
    // star, so removal is always confirmed before it happens.
    let unlistenFavoriteRemove;
    listen('favorite-remove-request', (event) => {
      const fav = event.payload;
      if (fav && Number.isFinite(fav.width) && Number.isFinite(fav.height) && Number.isFinite(fav.hz)) {
        favoritePendingRemove = fav;
      }
    }).then((stop) => { unlistenFavoriteRemove = stop; }).catch(() => {});
    // When the flyout confirmation dialog completes, clear busy flags and
    // re-poll display info. The trusted-favorite credit is decided and written
    // by the backend from the transaction's own result (see favorite_credit),
    // so nothing here has to judge whether the mode was kept.
    let unlistenFlyoutCompleted;
    listen('flyout-completed', () => {
      app.displayModeBusy = false;
      app.refreshRateBusy = false;
      pollDisplayInfo();
    }).then((stop) => { unlistenFlyoutCompleted = stop; }).catch(() => {});
    // A Keep just recorded this favorite as trusted on its monitor. Mirror it
    // into the in-memory favorites: they are what later config saves serialize,
    // so a stale confirmed_monitor_id here would write the credit back out as
    // empty the next time any setting is saved.
    let unlistenFavoriteConfirmed;
    listen('favorite-confirmed', (event) => {
      const favId = event.payload?.fav_id;
      const monitorId = event.payload?.confirmed_monitor_id;
      if (!favId || !monitorId) return;
      const fav = displayFavorites.find((entry) => entry.favId === favId);
      if (fav) markFavoriteConfirmed(fav, monitorId);
    }).then((stop) => { unlistenFavoriteConfirmed = stop; }).catch(() => {});
    return () => {
      unlisten?.();
      unlistenDisplayChanged?.();
      clearTimeout(vendorDisplayRefreshTimer);
      clearTimeout(hdrStatusRefreshTimer);
      clearTimeout(sdrWhiteBaseRecaptureTimer);
      unlistenHotkeyPressed?.();
      unlistenFavoriteRemove?.();
      unlistenFlyoutCompleted?.();
      unlistenFavoriteConfirmed?.();
      window.removeEventListener('focus', reconcileWindowVisible);
      window.removeEventListener('blur', closeAllMenus);
      if (visibilityReconcileTimer) {
        clearTimeout(visibilityReconcileTimer);
        visibilityReconcileTimer = null;
      }
      document.removeEventListener('visibilitychange', onDocumentVisibilityChange);
    };
  });

  $effect(() => {
    document.documentElement.setAttribute('data-hdr', app.hdrStatus === 'hdr' ? 'true' : 'false');
  });

  // Event-driven HDR detection, matchMedia fires when Windows HDR is
  // toggled from Settings or via the keyboard shortcut.
  $effect(() => {
    const mq = window.matchMedia('(dynamic-range: high)');
    function onHdrChange() { pollHDRStatus(); }
    try { mq.addEventListener('change', onHdrChange); } catch { /* old browser */ }
    return () => {
      try { mq.removeEventListener('change', onHdrChange); } catch {}
    };
  });

  // Driver-level range/VRR/reference-mode settings may change without a
  // Windows display notification. The full vendor NVAPI query chain is only
  // consumed by the System Info tab, so the periodic refresh runs only while
  // that tab is open (the Windows HDR tab reads the single RTX HDR DRS value
  // through its own 500 ms poll, which stays independent of this).
  //
  // A one-shot fetch always runs on window-visible: it populates the GPU
  // vendor/model that gates NVIDIA-only UI (the RTX HDR section, vendor
  // tags), that must not depend on which tab is open. Entering the Crush
  // tab also re-fetches once so its reference-mode warning reflects the
  // current driver state even if System Info was never opened (the effect
  // re-runs on tab switches, covering that case).
  // Keep one vendor fetch tied only to visibility, not to activeTab. This
  // discovers the GPU vendor/model on startup and after returning from the
  // tray, including an elevated restart that opens directly on Windows HDR.
  $effect(() => {
    if (windowVisible) refreshVendorDisplayInfo();
  });

  // The full vendor query is needed on the tabs that consume those settings.
  // Do not run it merely because an unrelated tab became active, that caused
  // a full NVAPI color/VRR/reference-mode probe on every tab switch.
  $effect(() => {
    if (!windowVisible) return;
    const tab = app.activeTab;
    if (tab === 'crush') {
      refreshVendorDisplayInfo();
      // Reference Mode is a driver setting that can be toggled while the tab
      // is open with no display notification. Poll the cheap registry-backed
      // flag so the warning clears and gamma resumes without running the full
      // NVAPI probe on every tick (and without needing a tab switch). The
      // poll stops when the tab (or window) is left, matching the system tab.
      startReferenceModePolling();
      return () => stopReferenceModePolling();
    }
    if (tab === 'system') {
      refreshVendorDisplayInfo();
      const timer = setInterval(() => refreshVendorDisplayInfo(), 5000);
      return () => clearInterval(timer);
    }
  });

  // The Windows SDR white level can be changed from Settings or other tools
  // with no display-topology event. Re-read it periodically (2s, only while
  // the window is visible) to keep System Info and the Windows HDR tab fresh.
  // The Brightness strip does NOT follow this live value: Chromium/WebView2
  // froze the SDR white level at WebView startup, so the strip encodes against
  // the first-read base (app.sdrWhiteBaseNits) captured here instead.
  $effect(() => {
    if (!windowVisible) return;
    refreshSdrWhiteLevel();
    const timer = setInterval(() => refreshSdrWhiteLevel(), 2000);
    return () => clearInterval(timer);
  });

  // HDR luminance is a DXGI capability read. The status-bar monitor tooltip
  // shows the panel's HDR max nits, so fetch it once whenever the window is
  // visible (mount, show-from-tray, sleep/resume); the System Info tab owns
  // the 5-second refresh cadence since it displays the value live.
  $effect(() => {
    if (!windowVisible) return;
    pollHDRLuminance();
    if (app.activeTab !== 'system') return;
    const timer = setInterval(() => pollHDRLuminance(), 5000);
    return () => clearInterval(timer);
  });

  // Monitor fullscreen of the calibration strip: suspend the taskbar dimmer
  // so Windows auto-hides the taskbar. The dimmer's always-on-top overlay
  // would otherwise keep the taskbar visible on top of the fullscreen strip.
  $effect(() => {
    setDimmerSuspendedForFullscreen(crush.fullscreen === 'monitor');
  });

  // Fully quits the app from the top-bar Exit button. Same graceful shutdown
  // path as the tray's Exit (quit_app → request_shutdown_and_exit): the
  // backend releases dimmer/hotkey/gamma resources and flushes the config
  // before the process exits, closing the main window and tray together.
  async function quitApp() {
    try {
      await invoke('quit_app');
    } catch (e) {
      console.warn('[app] quit failed', e);
    }
  }

  // Confirm before quitting: the Exit button is a destructive, session-ending
  // action (the app fully stops, including the tray icon), so a stray click
  // should not silently end it. Mirrors the favorite-remove confirmation.
  let exitConfirmOpen = $state(false);
  let exitConfirmCancelEl = $state(null);
  function askQuit() { exitConfirmOpen = true; }
  function cancelQuit() { exitConfirmOpen = false; }
  function confirmQuit() {
    exitConfirmOpen = false;
    void quitApp();
  }
  function onExitConfirmKeydown(e) {
    if (e.key === 'Escape') {
      e.preventDefault();
      cancelQuit();
      return;
    }
    trapModalTab(e);
  }
  // Focus the safe action (Cancel) when the dialog opens, so Enter and
  // Escape both take the safe path (Enter activates the focused button).
  $effect(() => {
    if (exitConfirmOpen) exitConfirmCancelEl?.focus();
  });
</script>

<a href="#main-content" class="skip-to-content">Skip to main content</a>

<div class="app-shell">
  <div class="top-bar">
    <div class="top-bar-left">
      <img class="app-icon" src={appIcon} alt="OLED Helper" />
      <div class="title-block">
        <span class="title">OLED HELPER</span>
        <span class="version">{appVersion ? `v${appVersion}` : ''}</span>
      </div>
    </div>
    <div class="spacer"></div>
    <button class="top-btn" class:active={app.activeTab === 'settings'} aria-current={app.activeTab === 'settings' ? 'page' : undefined} onclick={() => app.activeTab = 'settings'}>
      <span class="top-btn-inner"><img class="top-btn-icon" src={tabSystemIcon} alt="" /> Settings</span>
    </button>
    <button class="top-btn" class:active={app.activeTab === 'about'} aria-current={app.activeTab === 'about' ? 'page' : undefined} onclick={() => app.activeTab = 'about'}>
      <span class="top-btn-inner"><img class="top-btn-icon" src={tabAboutIcon} alt="" /> About</span>
    </button>
    <button class="top-btn top-exit-btn" title="Exit OLED Helper" aria-label="Exit OLED Helper" onclick={askQuit}>
      <span class="top-btn-inner">
        <svg class="top-exit-icon" width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path d="M8 1.5v6" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>
          <path d="M4.7 3.7a6 6 0 1 0 6.6 0" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>
        </svg>
        Exit
      </span>
    </button>
  </div>

  <nav class="sidebar">
    <div class="nav-item">
      <button class="nav-btn" class:active={app.activeTab === 'crush'} aria-current={app.activeTab === 'crush' ? 'page' : undefined} onclick={() => app.activeTab = 'crush'}>
        <img class="nav-icon" src={tabCrushIcon} alt="" /> Black Crush Tweak
      </button>
      <button type="button" class="nav-switch" class:on={crush.blackEnabled} aria-pressed={crush.blackEnabled} onclick={toggleBlackCrushFromNav} title="Toggle Black Crush Tweak" aria-label="Toggle Black Crush Tweak">
        <span class="nav-switch-thumb" aria-hidden="true"></span>
      </button>
    </div>
    <div class="nav-item">
      <button class="nav-btn" class:active={app.activeTab === 'dimmer'} aria-current={app.activeTab === 'dimmer' ? 'page' : undefined} onclick={() => app.activeTab = 'dimmer'}>
        <img class="nav-icon" src={tabDimmerIcon} alt="" /> Taskbar Dimmer
      </button>
      <button type="button" class="nav-switch" class:on={taskbarDimmer.enabled} aria-pressed={taskbarDimmer.enabled} onclick={toggleTaskbarDimmerFromNav} title="Toggle Taskbar Dimmer" aria-label="Toggle Taskbar Dimmer">
        <span class="nav-switch-thumb" aria-hidden="true"></span>
      </button>
    </div>
    <div class="nav-item">
      <button class="nav-btn" class:active={app.activeTab === 'screen-dimmer'} aria-current={app.activeTab === 'screen-dimmer' ? 'page' : undefined} onclick={() => app.activeTab = 'screen-dimmer'}>
        <img class="nav-icon" src={tabScreenDimmerIcon} alt="" /> Screen Dimmer
      </button>
      {#if screenDimmer.enabled && !screenDimmerHelper.ready && app.isElevated === false}
        <button
          type="button"
          class="nav-screen-dimmer-warn"
          aria-label="Set up Screen Dimmer media detection"
          title="Screen Dimmer video-pause detection (pause while video plays) needs a one-time elevated helper setup. Idle dimming works without it. Click to set it up now."
          onclick={setupScreenDimmerHelperFromNav}
        >⚠</button>
      {/if}
      <button type="button" class="nav-switch" class:on={screenDimmer.enabled} aria-pressed={screenDimmer.enabled} onclick={toggleScreenDimmerFromNav} title="Toggle Screen Dimmer" aria-label="Toggle Screen Dimmer">
        <span class="nav-switch-thumb" aria-hidden="true"></span>
      </button>
    </div>
    <div class="sidebar-sep" role="separator"></div>
    <button class="nav-btn" class:active={app.activeTab === 'text'} aria-current={app.activeTab === 'text' ? 'page' : undefined} onclick={() => app.activeTab = 'text'}>
      <TabTextIcon class="nav-icon" /> Text Rendering
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'windows-font'} aria-current={app.activeTab === 'windows-font' ? 'page' : undefined} onclick={() => app.activeTab = 'windows-font'}>
      <TabWindowsFontIcon class="nav-icon" /> Windows Font <span class="nav-global-tag">Global</span>
    </button>
    <div class="sidebar-sep" role="separator"></div>
    <button class="nav-btn" class:active={app.activeTab === 'windows-hdr'} aria-current={app.activeTab === 'windows-hdr' ? 'page' : undefined} onclick={() => app.activeTab = 'windows-hdr'}>
      <img class="nav-icon" src={tabHdrIcon} alt="" /> Windows HDR <span class="nav-global-tag">Global</span>
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'power'} aria-current={app.activeTab === 'power' ? 'page' : undefined} onclick={() => app.activeTab = 'power'}>
      <TabPowerIcon class="nav-icon" /> Power Saving <span class="nav-global-tag">Global</span>
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'hotkeys'} aria-current={app.activeTab === 'hotkeys' ? 'page' : undefined} onclick={() => app.activeTab = 'hotkeys'}>
      <img class="nav-icon" src={tabHotkeysIcon} alt="" /> Hotkeys
    </button>

    <div class="sidebar-spacer"></div>
  </nav>

  <main class="content" id="main-content">
    <div class="sr-only" aria-live="polite" aria-atomic="true" id="sr-announcer"></div>
    {#if windowVisible}
      {#if app.activeTab === 'crush'}
        <TabCrush />
      {:else if app.activeTab === 'dimmer'}
        <TabDimmer />
      {:else if app.activeTab === 'screen-dimmer'}
        <TabDimmer screen={true} />
      {:else if app.activeTab === 'hotkeys'}
        <TabHotkeys />
      {:else if app.activeTab === 'text'}
        <TabText modalOpen={!!favoritePendingRemove || exitConfirmOpen || updateState.modalOpen} menuOpen={resMenuOpen || hzMenuOpen || favoritesMenuOpen || resCtxMenu.open || hzCtxMenu.open} />
      {:else if app.activeTab === 'windows-font'}
        <TabWindowsFont />
      {:else if app.activeTab === 'power'}
        <TabPower />
      {:else if app.activeTab === 'windows-hdr'}
        <TabWindowsHdr />
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
    <div class="status-bar-left">
      {#if updateState.status === 'available'}
        <button
          type="button"
          class="update-available-btn"
          title={`Update available: v${updateState.version}`}
          onclick={() => { updateState.modalOpen = true; }}>
          <svg width="10" height="10" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <path d="M8 1.5V9.5M8 9.5L5 6.5M8 9.5L11 6.5" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>
            <path d="M2.5 13.5h11" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>
          </svg>
          Update available
        </button>
      {/if}
    </div>
    <div class="status-bar-center">
      <MonitorSelector variant="status" />
      <div class="resolution-wrap">
        <button type="button" class="resolution-select" class:active={resMenuOpen} class:no-tags={!displayMenuPrefs.showTags}
          bind:this={resButtonEl}
          aria-label="Resolution"
          aria-haspopup="listbox"
          aria-expanded={resMenuOpen}
          title={resolutionError || 'Choose resolution · right-click for list options'}
          disabled={app.displayModeBusy || app.refreshRateBusy || app.displayInfo.resolutions.length === 0}
          onclick={toggleResMenu}
          oncontextmenu={openResContextMenu}>          {app.displayInfo.width && app.displayInfo.height ? `${app.displayInfo.width}×${app.displayInfo.height}` : '-- × --'}{#if displayMenuPrefs.showTags && currentResolutionName()}<span class="refresh-rate-select-value"> · {currentResolutionName()}</span>{/if}
        </button>
        {#snippet resItem(resolution)}
          {@const name = resolutionName(resolution)}
          {@const current = resolution.width === app.displayInfo.width && resolution.height === app.displayInfo.height}
          {@const recommended = isRecommendedResolution(resolution)}
          {@const ultrawide = isUltrawide(resolution.width, resolution.height)}
          {@const hz = app.displayInfo.refreshRateHz || 0}
          {@const favored = hz > 0 && isDisplayModeFavorited(resolution.width, resolution.height, hz, currentMonitorId(), currentMonitorSig())}
          <div class="res-item" class:active={current}>
            <button type="button" class="dd-menu-item res-item-main" class:active={current}
              role="option" aria-selected={current}
              onclick={() => pickResolution(resolution.width, resolution.height)}>
              <span class="dd-check" aria-hidden="true">{current ? '✓' : ''}</span>
              {resolution.width}×{resolution.height}
              {#if displayMenuPrefs.showTags}
                {#if ultrawide}<span class="ultrawide-tag"><span class="ultrawide-separator">·</span><svg class="ultrawide-icon" viewBox="0 0 32 16" fill="none" aria-label="Ultrawide"><rect x="0.9" y="0.9" width="30.2" height="14.2" rx="2" stroke="currentColor" stroke-width="1.1"/><text x="16" y="10.8" text-anchor="middle" fill="currentColor" font-size="9" font-family="Inter, sans-serif" font-weight="700" letter-spacing="0.2">UW</text></svg></span>{/if}
                {#if name}<span class="res-item-tag"> · {name}</span>{/if}
              {/if}
              {#if recommended}<span class="res-item-tag"> (Native)</span>{/if}
            </button>
            {#if hz > 0}
              <button
                type="button"
                class="res-item-fav"
                class:favorited={favored}
                title={favored ? `Remove ${resolution.width}×${resolution.height} @ ${hz} Hz from favorites` : `Favorite ${resolution.width}×${resolution.height} @ ${hz} Hz`}
                aria-label={favored ? `Remove ${resolution.width}×${resolution.height} @ ${hz} Hz from favorites` : `Favorite ${resolution.width}×${resolution.height} @ ${hz} Hz`}
                onclick={() => toggleResolutionFavorite(resolution)}>
                <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                  <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill={favored ? 'currentColor' : 'none'} stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
                </svg>
              </button>
            {/if}
          </div>
        {/snippet}
        {#if resMenuOpen}
          <div class="dd-menu res-menu" bind:this={resMenuEl} role="listbox" aria-label="Resolution options" tabindex="-1" oncontextmenu={openResContextMenu}>
            {#if app.displayInfo.resolutions.length === 0}
              <div class="dd-menu-item active">
                <span class="dd-check" aria-hidden="true">✓</span>
                {app.displayInfo.width && app.displayInfo.height ? `${app.displayInfo.width}×${app.displayInfo.height}` : '-- × --'}
              </div>
            {:else if displayMenuPrefs.resolutionGroupBy === 'aspectRatio'}
              {#each groupedResolutions() as group}
                <div class="dd-menu-group-label">{group.label}</div>
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
            <div class="ctx-menu-subheading">Resolution</div>
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
          disabled={app.refreshRateBusy || app.displayModeBusy || app.displayInfo.refreshRates.length === 0}
          onclick={toggleHzMenu}
          oncontextmenu={openHzContextMenu}>
          {app.displayInfo.refreshRateHz ? `${app.displayInfo.refreshRateHz} Hz` : '-- Hz'}{#if app.displayInfo.refreshRateHz && blackLevelDisplayForRefreshRate(app.displayInfo.refreshRateHz)}<span class="refresh-rate-select-value"> ({blackLevelDisplayForRefreshRate(app.displayInfo.refreshRateHz)})</span>{/if}
        </button>
        {#if hzMenuOpen}
          <div class="dd-menu" bind:this={hzMenuEl} role="listbox" aria-label="Refresh rate options" tabindex="-1" oncontextmenu={openHzContextMenu}>
            {#each [...app.displayInfo.refreshRates].sort((a, b) => (displayMenuPrefs.resolutionSort === 'asc' ? a - b : b - a)) as hz}
              <button type="button" class="dd-menu-item" class:active={hz === app.displayInfo.refreshRateHz}
                role="option" aria-selected={hz === app.displayInfo.refreshRateHz}
                onclick={() => pickHz(hz)}>
                <span class="dd-check" aria-hidden="true">{hz === app.displayInfo.refreshRateHz ? '✓' : ''}</span>
                <span>{hz} Hz</span>
                {#if blackLevelDisplayForRefreshRate(hz)}<span class="dd-profile-value">{blackLevelDisplayForRefreshRate(hz)}</span>{/if}
              </button>
            {/each}
          </div>
        {/if}
        {#if hzCtxMenu.open}
          <div class="ctx-menu compact" bind:this={hzCtxMenuEl} style="left:{hzCtxMenu.x}px; top:{hzCtxMenu.y}px;" role="menu" aria-label="Refresh rate list options" tabindex="-1" onmousedown={(e) => ctxChromeDismiss(e, closeHzContextMenu)} onclick={(e) => e.stopPropagation()} onkeydown={(e) => handleCtxMenuKeydown(e, closeHzContextMenu)} oncontextmenu={repositionHzContextMenu}>
            <div class="ctx-menu-subheading">Sort</div>
            <button class="ctx-item" role="menuitem" class:active={displayMenuPrefs.resolutionSort === 'asc'} onclick={() => hzCtxAction(() => setResolutionSort('asc'))}><span class="ctx-radio-dot" aria-hidden="true"></span>Ascending</button>
            <button class="ctx-item" role="menuitem" class:active={displayMenuPrefs.resolutionSort === 'desc'} onclick={() => hzCtxAction(() => setResolutionSort('desc'))}><span class="ctx-radio-dot" aria-hidden="true"></span>Descending</button>
            <div class="ctx-menu-sep"></div>
            <div class="ctx-menu-subheading">Resolution</div>
            <button class="ctx-item" role="menuitem" class:active={displayMenuPrefs.resolutionGroupBy === 'aspectRatio'} onclick={() => hzCtxAction(() => setResolutionGroupBy(displayMenuPrefs.resolutionGroupBy === 'aspectRatio' ? 'none' : 'aspectRatio'))}><span class="ctx-checkbox" aria-hidden="true"></span>Group by aspect ratio</button>
            <button class="ctx-item" role="menuitem" class:active={displayMenuPrefs.showTags} onclick={() => hzCtxAction(() => setShowTags(!displayMenuPrefs.showTags))}><span class="ctx-checkbox" aria-hidden="true"></span>Show tags (4K, QHD, UW)</button>
          </div>
        {/if}
        {#if refreshRateError}
          <span class="refresh-rate-error" role="status" aria-label="Refresh rate change failed" title={refreshRateError}>!</span>
        {/if}
      </div>
      <div
        class="favorites-wrap"
        onpointerenter={openFavoritesOnHover}
        onpointerleave={closeFavoritesOnHoverLeave}>
        <button
          type="button"
          class="favorites-btn"
          class:active={favoritesMenuOpen}
          class:favorited={isCurrentFavorited}
          bind:this={favoritesBtnEl}
          aria-label="Favorite display modes"
          aria-haspopup="listbox"
          aria-expanded={favoritesMenuOpen}
          title={isCurrentFavorited ? `Current mode is favorited: ${currentComboLabel}` : 'Favorite display modes'}
          disabled={app.displayModeBusy || app.refreshRateBusy}
          onclick={toggleFavoritesMenu}>
          <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill={isCurrentFavorited ? 'currentColor' : 'none'} stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
          </svg>
        </button>
        {#if favoritesMenuOpen}
          <div class="dd-menu favorites-menu" bind:this={favoritesMenuEl} role="listbox" aria-label="Favorite display modes" tabindex="-1">
            <div class="favorites-title">Favorites</div>
            {#if currentComboLabel && !isCurrentFavorited}
              <div class="favorites-item favorites-current">
                <button
                  type="button"
                  class="dd-menu-item favorites-apply"
                  role="option"
                  aria-selected={false}
                  onclick={saveCurrentFavorite}>
                  {currentComboLabel}
                </button>
                <button
                  type="button"
                  class="favorites-star"
                  aria-label={`Save current: ${currentComboLabel}`}
                  title={`Save current: ${currentComboLabel}`}
                  onclick={saveCurrentFavorite}>
                  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                    <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
                  </svg>
                </button>
              </div>
            {/if}
            {#if displayFavorites.length === 0}
              <div class="favorites-empty">No favorites yet. Save the current mode to add one.</div>
            {:else}
              {#each displayFavorites as fav (displayModeFavoriteKey(fav.width, fav.height, fav.hz, fav.monitorId, fav.monitorSig))}
                {@const favTag = favoriteMonitorLabel(fav)}
                {@const favDisconnected = !favoriteMonitorConnected(fav)}
                <div class="favorites-item" class:active={isFavoriteCurrent(fav)} class:disconnected={favDisconnected}>
                  <button
                    type="button"
                    class="dd-menu-item favorites-apply"
                    role="option"
                    aria-selected={isFavoriteCurrent(fav)}
                    disabled={favDisconnected}
                    title={favDisconnected ? 'This monitor is not connected; the favorite applies once it is reconnected' : ''}
                    onclick={() => applyFavorite(fav)}>
                    {fav.width}×{fav.height} @ {fav.hz} Hz{#if favTag}<span class="favorites-monitor-tag">{favTag}</span>{/if}
                  </button>
                  <button
                    type="button"
                    class="favorites-star"
                    class:filled={true}
                    aria-label={`Remove ${fav.width}×${fav.height} @ ${fav.hz} Hz from favorites`}
                    title={`Remove ${fav.width}×${fav.height} @ ${fav.hz} Hz from favorites`}
                    onclick={() => askRemoveFavorite(fav)}>
                    <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                      <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill="currentColor" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
                    </svg>
                  </button>
                </div>
              {/each}
            {/if}
          </div>
        {/if}
        {#if favoritesStatusMsg}
          <span
            class="favorites-status-msg"
            class:favorites-status-remove={favoritesStatusIsRemove}
            aria-live="polite"
            role="status">
            {favoritesStatusMsg}
          </span>
        {/if}
      </div>
    </div>
    <div class="status-bar-right">
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

  {#if favoritePendingRemove}
    <div class="modal-backdrop" role="presentation" onpointerdown={(e) => { if (e.target === e.currentTarget) cancelRemoveFavorite(); }}>
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="favorite-confirm-title" aria-describedby="favorite-confirm-desc" tabindex="-1" onkeydown={onFavoriteConfirmKeydown}>
        <h2 id="favorite-confirm-title" class="modal-title">Remove favorite?</h2>
        <p id="favorite-confirm-desc" class="modal-desc">
          Remove <strong>{favoritePendingRemove.width}×{favoritePendingRemove.height} @ {favoritePendingRemove.hz} Hz</strong> from your favorites?
        </p>
        <div class="modal-actions">
          <button bind:this={favoriteConfirmCancelEl} class="modal-btn modal-btn-cancel" onclick={cancelRemoveFavorite}>Cancel</button>
          <button class="modal-btn" onclick={confirmRemoveFavorite}>Remove</button>
        </div>
      </div>
    </div>
  {/if}

  {#if exitConfirmOpen}
    <div class="modal-backdrop" role="presentation" onpointerdown={(e) => { if (e.target === e.currentTarget) cancelQuit(); }}>
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="exit-confirm-title" aria-describedby="exit-confirm-desc" tabindex="-1" onkeydown={onExitConfirmKeydown}>
        <h2 id="exit-confirm-title" class="modal-title">Exit OLED Helper?</h2>
        <p id="exit-confirm-desc" class="modal-desc">
          The app will close completely and stop running in the system tray.
        </p>
        <div class="modal-actions">
          <button bind:this={exitConfirmCancelEl} class="modal-btn modal-btn-cancel" onclick={cancelQuit}>Cancel</button>
          <button class="modal-btn modal-btn-danger" onclick={confirmQuit}>Exit</button>
        </div>
      </div>
    </div>
  {/if}
</div>

<!-- Persistent 1×1 HDR surface (see initHdrLayer): keeps the window in the
     HDR presentation path for the whole session so chrome brightness is
     uniform across tabs. Invisible, 1px, near-zero opacity, no pointer events. -->
<canvas class="hdr-layer" bind:this={hdrLayerCanvas} aria-hidden="true" tabindex="-1"></canvas>

{#if updateState.modalOpen}
  <UpdateModal onclose={() => { updateState.modalOpen = false; }} />
{/if}
