<script>
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { app, crush, taskbarDimmer, blackLift, pollDisplayInfo, scheduleDisplayInfoRefresh, refreshVendorDisplayInfo, pollHDRStatus, pollHDRLuminance, refreshSdrWhiteLevel, setSdrWhiteNits, loadConfig, saveConfig, setDimmerSuspendedForFullscreen, setTaskbarDimmer, applyBlackLevelProfileForRefreshRate, persistCurrentBlackLevelProfile, blackLevelDisplayForRefreshRate, displayMenuPrefs, setResolutionSort, setResolutionGroupBy, setShowTags, setRefreshRateSort, dismissCtxMenus } from './lib/stores.svelte.js';
  import { invoke } from '@tauri-apps/api/core';
  import TabCrush from './lib/TabCrush.svelte';
  import TabDimmer from './lib/TabDimmer.svelte';
  import TabHotkeys from './lib/TabHotkeys.svelte';
  import TabWindowsHdr from './lib/TabWindowsHdr.svelte';
  import TabExperimental from './lib/TabExperimental.svelte';
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
  import tabHdrIcon from './lib/icons/tab_hdr.png?inline';
  import tabDebugIcon from './lib/icons/tab_debug.png?inline';
  import tabHotkeysIcon from './lib/icons/tab_hotkeys.png?inline';

  let windowVisible = $state(true);
  // Coalesces the display-changed burst (hotplug/mode change emits several
  // WM_DISPLAYCHANGE/WM_DEVICECHANGE messages) into a single SDR-white base
  // re-capture, and gives the OS a beat to settle before the new value is read.
  let sdrWhiteBaseRecaptureTimer = null;
  let vendorDisplayRefreshTimer = null;

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
    // A press on the list while its context menu was open sets
    // resCtxDismissClick (document capture guard below). If a click still
    // fires despite preventDefault, swallow it here instead of applying the
    // item under the cursor. Also covers keyboard activation — Enter on a
    // focused item — where no pointer event precedes the click: dismiss the
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
  let hzCtxDismissClick = false;
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

  // While a context menu overlays its dropdown, a press anywhere on the
  // dropdown list should only dismiss the context menu — never pick the item
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
  // Plays the shared exit ease (mode-confirm-exit-in) before the modal is
  // removed — mirrors Flyout.svelte's `exiting` state so the in-app modal
  // and the tray flyout animate identically.
  let modeConfirmationExiting = $state(false);
  let modeConfirmKeepBtnEl = $state(null);
  let modeConfirmRevertBtnEl = $state(null);
  // Element that had focus before the modal opened; restored on close so
  // keyboard users don't get stranded on <body> after the modal unmounts.
  let modeConfirmLastFocused = null;

  // Keyboard handling for the confirmation dialog, mirroring Flyout.svelte's
  // onDialogKeydown and Windows' own display-confirm semantics: Revert is the
  // default action (initial focus), Enter on the focused button is native,
  // Enter/Escape with no focused button reverts, and Tab stays trapped
  // between the two buttons.
  function onModeConfirmKeydown(e) {
    if (modeConfirmationBusy) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      rollbackModeChange();
    } else if (e.key === 'Enter') {
      if (e.target?.closest?.('button')) return;
      e.preventDefault();
      rollbackModeChange();
    } else if (e.key === 'Tab') {
      const first = modeConfirmKeepBtnEl;
      const last = modeConfirmRevertBtnEl;
      if (!first || !last) return;
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    }
  }

  function clearModeConfirmationTimer() {
    if (modeConfirmationTimer) clearInterval(modeConfirmationTimer);
    modeConfirmationTimer = null;
  }

  function clearModeConfirmation() {
    clearModeConfirmationTimer();
    pendingModeConfirmation = null;
    modeConfirmationExiting = false;
    // The modal is about to unmount; put focus back where it was before it
    // opened (the dropdown button that triggered the change, etc.).
    modeConfirmLastFocused?.focus?.();
    modeConfirmLastFocused = null;
  }

  function startModeConfirmation(result) {
    clearModeConfirmation();
    modeConfirmationError = '';
    const seconds = Number(result?.confirmation_seconds) || 15;
    const started = Date.now();
    // Capture what had focus BEFORE the modal renders (still the dropdown
    // button that triggered the change) so closeModeConfirmation can restore
    // it. Must happen here, not in an $effect: the countdown reassigns
    // pendingModeConfirmation every 250ms, and an effect keyed on it would
    // re-run each tick — stealing focus back to Revert after the user tabs
    // to Keep this mode, and re-capturing the Revert button as the restore
    // target just before it unmounts.
    modeConfirmLastFocused = document.activeElement;
    pendingModeConfirmation = { ...result, remaining: seconds, totalSeconds: seconds };
    // Windows' display-confirm dialogs default to Revert: focus it as the
    // safe action (after the modal is in the DOM, so the ref is bound).
    setTimeout(() => modeConfirmRevertBtnEl?.focus(), 0);
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

  // Shared close path for Keep and Revert, mirroring Flyout.svelte's
  // closeFlyout. The exit ease plays FIRST with nothing running concurrently
  // — confirm_mode_change / rollback both re-apply the display mode via
  // ChangeDisplaySettingsExW, and doing that while the animation plays stalls
  // the compositor and makes it stutter. So the order is: play the exit
  // animation (smooth), then run the mode-change invoke. On success the modal
  // is removed; on failure it is re-shown (removing the exiting class
  // re-applies the entrance animation) so the error is visible and retryable.
  async function closeModeConfirmation(action) {
    if (modeConfirmationBusy || !pendingModeConfirmation) return;
    modeConfirmationBusy = true;
    modeConfirmationError = '';
    // Scope to the mode-confirm modal specifically — the generic
    // .modal-backdrop .modal selector could match TabSettings' reset-all
    // confirm if both were ever open at once.
    const modal = document.querySelector('.mode-confirm-backdrop .modal');
    modeConfirmationExiting = true;
    const exitFinished = modal
      ? new Promise((resolve) => {
          let settled = false;
          const finish = () => {
            if (settled) return;
            settled = true;
            clearTimeout(failsafe);
            modal.removeEventListener('animationend', onEnd);
            modal.removeEventListener('animationcancel', onCancel);
            resolve();
          };
          // Only the exit animation ends the dialog — a leftover entrance
          // animationend (mode-confirm-rise-in) must not resolve this early.
          const onEnd = (event) => {
            if (event.animationName === 'mode-confirm-exit-in') finish();
          };
          // If the exit class is removed before the animation completes
          // (error path), the exit is cancelled rather than ended. Resolve
          // and detach there too, or repeated attempts would accumulate
          // listeners on the same modal.
          const onCancel = (event) => {
            if (event.animationName === 'mode-confirm-exit-in') finish();
          };
          modal.addEventListener('animationend', onEnd);
          modal.addEventListener('animationcancel', onCancel);
          // Safety net: never wait forever if the event is missed.
          const failsafe = setTimeout(finish, 600);
        })
      : Promise.resolve();
    try {
      // Exit animation plays first — smooth, nothing concurrent yet.
      await exitFinished;
      // The mode change runs now; its display re-apply happens after the
      // modal has faded out, so the compositor hiccup is invisible.
      await invoke(action);
      clearModeConfirmationTimer();
      clearModeConfirmation();
      resolutionError = '';
      refreshRateError = '';
    } catch (e) {
      modeConfirmationError = String(e);
      // Re-show the dialog: removing the exiting class re-applies the
      // entrance animation, keeping the error visible and retryable.
      modeConfirmationExiting = false;
      // Re-focus the default action so keyboard users can immediately retry.
      modeConfirmRevertBtnEl?.focus();
    } finally {
      modeConfirmationBusy = false;
      // Confirming persists the already-active mode via CDS_NORESET, so the
      // display is unchanged and a full re-poll (mode enumeration + EDID
      // reads) right after the commit is redundant work landing in the same
      // frame as the registry persist. Rollback genuinely changes the mode
      // and re-polls; a failed invoke re-polls to resync the dropdowns.
      if (action !== 'confirm_display_mode_change' || modeConfirmationError) {
        pollDisplayInfo();
      }
    }
  }

  async function confirmModeChange() {
    await closeModeConfirmation('confirm_display_mode_change');
  }

  async function rollbackModeChange() {
    await closeModeConfirmation('rollback_display_mode_change');
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

  // Desktop color depth as Windows' classic Display Properties reports it
  // (GetDeviceCaps BITSPIXEL on the screen DC — 32 on modern setups). The
  // browser's screen.colorDepth reports 30 on 10-bit panels, which is the
  // signal bit depth, not the desktop color mode.
  function desktopColorDepth() {
    return app.displayInfo.desktopBits ? `${app.displayInfo.desktopBits}-bit` : '—';
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
  // The strip's own keyboard shortcuts are unaffected — these only fire for
  // the registered global bindings.
  let hotkeySaveTimer = null;

  function applyGammaForHotkey() {
    invoke('apply_gamma_ramp', { blackLift: blackLift() }).catch(() => {});
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
          if (Number.isFinite(applied)) setSdrWhiteNits(applied);
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
  // leaving those tabs destroys them via onDestroy — so the window flips to
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
      // drop the persistent layer — the exact failure this feature exists to
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
      // carries HDR content — a transparent/black 1×1 could be optimized away.
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
  // canvases. Re-renders when the window becomes visible again too — the
  // swapchain can be recreated while the app is hidden in the tray.
  $effect(() => {
    void app.hdrStatus; void windowVisible;
    if (hdrLayerCanvas && hdrLayerInitStarted) configureHdrLayer();
  });

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
      case 'decrease_sdr_brightness':
        adjustSdrBrightnessForHotkey(-1);
        break;
      case 'increase_sdr_brightness':
        adjustSdrBrightnessForHotkey(1);
        break;
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
    // explicitly afterward — there is no periodic poll on the main thread.
    pollDisplayInfo();
    pollHDRStatus();
    // Seed the SDR white level used by the Brightness strip compensation
    // (Chromium maps the HDR calibration canvas relative to this value).
    refreshSdrWhiteLevel();

    let unlisten;
    let unlistenDisplayChanged;
    let unlistenHotkeyPressed;
    let unlistenTrayMode;
    // The Rust window subclass forwards WM_DISPLAYCHANGE and WM_DEVICECHANGE.
    // Full display-info queries remain event-driven. Vendor control panels can
    // silently change range/VRR, so a separate low-frequency vendor-only
    // refresh below handles that case without touching mode lists or EDID.
    listen('display-changed', () => {
      scheduleDisplayInfoRefresh();
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
      // make the WebView re-read its display info — including the SDR white
      // level it froze at startup — so re-capture the strip's base to match.
      // Debounced: hotplug emits a burst of WM_* messages, and waiting lets
      // the OS settle before the new value is read. A plain SDR-brightness
      // slider move does NOT emit this event, so the base stays frozen there
      // (exactly what keeps the strip stable).
      clearTimeout(sdrWhiteBaseRecaptureTimer);
      sdrWhiteBaseRecaptureTimer = setTimeout(() => {
        sdrWhiteBaseRecaptureTimer = null;
        refreshSdrWhiteLevel(true);
      }, 150);
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
        refreshSdrWhiteLevel();
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
      clearTimeout(vendorDisplayRefreshTimer);
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

  // Driver-level range/VRR/reference-mode settings may change without a
  // Windows display notification. The full vendor NVAPI query chain is only
  // consumed by the System Info tab, so the periodic refresh runs only while
  // that tab is open (the Windows HDR tab reads the single RTX HDR DRS value
  // through its own 500 ms poll, which stays independent of this).
  //
  // A one-shot fetch always runs on window-visible: it populates the GPU
  // vendor/model that gates NVIDIA-only UI (the RTX HDR section, vendor
  // tags) — that must not depend on which tab is open. Entering the Crush
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
  // Do not run it merely because an unrelated tab became active — that caused
  // a full NVAPI color/VRR/reference-mode probe on every tab switch.
  $effect(() => {
    if (!windowVisible) return;
    const tab = app.activeTab;
    if (tab === 'crush') refreshVendorDisplayInfo();
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

<a href="#main-content" class="skip-to-content">Skip to main content</a>

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
    <button class="nav-btn" class:active={app.activeTab === 'windows-hdr'} onclick={() => app.activeTab = 'windows-hdr'}>
      <img class="nav-icon" src={tabHdrIcon} alt="" /> Windows HDR
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'experimental'} onclick={() => app.activeTab = 'experimental'}>
      <img class="nav-icon" src={tabDebugIcon} alt="" /> Experimental
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'hotkeys'} onclick={() => app.activeTab = 'hotkeys'}>
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
      {:else if app.activeTab === 'hotkeys'}
        <TabHotkeys />
      {:else if app.activeTab === 'experimental'}
        <TabExperimental />
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
    <span>v0.1.0</span>
    <div class="status-bar-right">
      {#if app.displayInfo.monitorName}
        <span class="monitor-tip-wrap" tabindex="0" aria-label={`Monitor info: ${app.displayInfo.monitorName}`}>
          <span class="current-monitor">
            {app.displayInfo.monitorName}
          </span>
          <span class="monitor-tip" role="tooltip">
            <span class="monitor-tip-title">{app.displayInfo.monitorName}</span>
            <span class="monitor-tip-row"><span>Connector</span><strong>{app.displayInfo.connector || '—'}</strong></span>
            <span class="monitor-tip-row"><span>Color format</span><strong>{app.displayInfo.colorFormat || '—'}{app.displayInfo.bitsPerChannel ? ` · ${app.displayInfo.bitsPerChannel} bpc` : ''}</strong></span>
            <span class="monitor-tip-row"><span>Desktop color</span><strong>{desktopColorDepth()}</strong></span>
            <span class="monitor-tip-row"><span>Dynamic range</span><strong>{app.displayInfo.dynamicRange || '—'}</strong></span>
            <span class="monitor-tip-row"><span>VRR</span><strong>{app.displayInfo.vrr || '—'}</strong></span>
            <span class="monitor-tip-row"><span>Current</span><strong>{app.displayInfo.width}×{app.displayInfo.height}{app.displayInfo.refreshRateHz ? ` @ ${app.displayInfo.refreshRateHz}Hz` : ''}</strong></span>
          </span>
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
            <div class="ctx-menu-subheading">Sort</div>
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
      <div class="modal mode-confirm-modal" class:mode-confirm-exiting={modeConfirmationExiting} role="dialog" aria-modal="true" aria-labelledby="mode-confirm-title" aria-describedby="mode-confirm-desc" tabindex="-1" onkeydown={onModeConfirmKeydown}>
        <h2 id="mode-confirm-title" class="modal-title">Keep display mode?</h2>
        <p id="mode-confirm-desc" class="modal-desc">
          {pendingModeConfirmation.monitor_name || 'Current monitor'} is temporarily using
          <strong>{pendingModeConfirmation.width}×{pendingModeConfirmation.height} at {pendingModeConfirmation.refresh_hz} Hz</strong>.
          Confirm if the picture looks correct. It will automatically revert in {pendingModeConfirmation.remaining}s.
        </p>
        {#if modeConfirmationError}<p class="modal-error">Rollback failed: {modeConfirmationError}</p>{/if}
        <div class="modal-actions">
          <button bind:this={modeConfirmKeepBtnEl} class="modal-btn" onclick={confirmModeChange} disabled={modeConfirmationBusy}>Keep this mode</button>
          <button bind:this={modeConfirmRevertBtnEl} class="modal-btn modal-btn-cancel" onclick={rollbackModeChange} disabled={modeConfirmationBusy}>Revert (<span class="modal-btn-countdown">{pendingModeConfirmation.remaining}</span>s)</button>
        </div>
        <div class="mode-confirm-timerbar" style="animation-duration: {pendingModeConfirmation.totalSeconds || 15}s" role="presentation" aria-hidden="true"></div>
      </div>
    </div>
  {/if}
</div>

<!-- Persistent 1×1 HDR surface (see initHdrLayer): keeps the window in the
     HDR presentation path for the whole session so chrome brightness is
     uniform across tabs. Invisible — 1px, near-zero opacity, no pointer events. -->
<canvas class="hdr-layer" bind:this={hdrLayerCanvas} aria-hidden="true" tabindex="-1"></canvas>
