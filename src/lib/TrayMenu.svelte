<script>
  // Custom CSS tray popup, replacement for the native menu (tray_menu.rs).
  // Rendered in its own small always-on-top WebviewWindow (see
  // src-tauri/src/tray_menu_window.rs), anchored to the tray icon.
  //
  // Mirrors tray_menu.rs's native menu structure: Resolution, Refresh Rate,
  // and Favorites submenus, then Restart/Exit. This window is a separate
  // WebView from the main app window, so it cannot reach into
  // stores.svelte.js's in-memory state, favorites are read/written directly
  // through load_config/save_config, the same way tray_menu.rs's native
  // Favorites submenu does.
  import { onMount, tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { emit, listen } from '@tauri-apps/api/event';
  import { getVersion } from '@tauri-apps/api/app';
  import appIcon from './icons/app-icon.png?inline';
  import tabDimmerIcon from './icons/tab_dimmer.png?inline';
  import tabScreenDimmerIcon from './icons/tab_screen_dimmer.png?inline';
  import tabCrushIcon from './icons/tab_crush.png?inline';
  import tabHdrIcon from './icons/tab_hdr.png?inline';
  import { taskbarDimmer, setTaskbarDimmer, screenDimmer, setScreenDimmer, setScreenDimmerEnabled } from './stores.svelte.js';

  let trayBlack = $state({ enabled: true, adjustment: 0 });
  let trayBlackProfiles = $state({});
  let trayLegacyBlackProfiles = $state({});
  let traySdrBrightness = $state({ available: false, nits: null });
  // App version for the tray header, read the same way App.svelte reads it
  // for the main window's status bar (keeps both in sync with the version
  // the updater compares against).
  let appVersion = $state('');
  // Mirror of the main window's Windows HDR state ('unknown' | 'hdr' | 'sdr'),
  // read through the same matchMedia probe the main app uses. Drives the
  // header's SDR/HDR switch and hides the SDR Content Brightness row in SDR.
  let trayHdrStatus = $state('unknown');
  // SDR→HDR inverse tone mapping mode, mirroring the main app's Windows HDR
  // tab: 'off' | 'auto' | 'rtx' | null (null = not loaded yet). The RTX
  // option only exists on NVIDIA GPUs, like the main tab.
  let trayHdrMode = $state(null);
  let trayHdrModeBusy = $state(false);
  let trayHdrModeError = $state('');
  let trayHdrIsNvidia = $state(false);

  // Mirror the main app's accent switch: app.css flips --accent to the gold
  // HDR tone via `:root[data-hdr="true"]`, so the tray sliders/toggles follow
  // the accent color too (not just the switch in the header).
  $effect(() => {
    document.documentElement.setAttribute('data-hdr', trayHdrStatus === 'hdr' ? 'true' : 'false');
  });
  let windowsHdrBusy = $state(false);
  let windowsHdrError = $state('');
  // Whether this process is elevated. The Screen Dimmer only runs in an
  // elevated session; the tray warns on its row when enabled but not
  // elevated (same rule as the Screen Dimmer tab's admin banner).
  let trayElevated = $state(true);
  let dimmerSaveTimer = null;
  let dimmerSaveInFlight = false;
  let quickInteractionVersion = 0;
  let blackApplyInFlight = false;
  let pendingBlackApply = null;
  let sdrApplyInFlight = false;
  let pendingSdrNits = null;
  let sdrApplyTimer = null;
  // Leading-edge throttle for the SDR brightness slider, matching the main
  // window's APPLY_THROTTLE_MS so both sliders update the screen live while
  // dragging instead of only after a pause.
  const SDR_APPLY_THROTTLE_MS = 50;
  let sdrLastApplyTime = 0;
  let dimmerSaveQueued = false;

  async function persistTrayDimmerSettings() {
    if (dimmerSaveInFlight) {
      dimmerSaveQueued = true;
      return;
    }
    dimmerSaveInFlight = true;
    const saveVersion = quickInteractionVersion;
    try {
      // Read the native status immediately before persisting. The tray UI is
      // optimistic while the shared command queue drains; if the overlay
      // rejects a toggle/slider update, never write that failed state to disk.
      const [cfg, status, screenStatus] = await Promise.all([
        invoke('load_config'),
        invoke('get_taskbar_dimmer_status', { monitorKey: trayDimmerMonKey() }),
        invoke('power_get_screen_dimmer_status', { monitorKey: trayDimmerMonKey() }),
      ]);
      if (saveVersion !== quickInteractionVersion) {
        scheduleTrayDimmerSave();
        return;
      }
      if (status && status.available !== false) applyTaskbarDimmerStatus(status);
      if (screenStatus) applyScreenDimmerStatus(screenStatus);
      const dimmerSettings = status && status.available !== false
        ? {
            enabled: status.enabled === true,
            opacity: Math.max(0, Math.min(1, Number(status.opacity) || 0)),
            fadeInDurationMs: Math.max(0, Math.min(1000, Math.round(Number(status.fade_in_duration_ms) || 0))),
            fadeOutDurationMs: Math.max(0, Math.min(1000, Math.round(Number(status.fade_out_duration_ms) || 0))),
          }
        : {
            enabled: cfg?.taskbar_dimmer_enabled === true,
            opacity: Math.max(0, Math.min(1, Number(cfg?.taskbar_dimmer_opacity) || 0)),
            fadeInDurationMs: Math.max(0, Math.min(1000, Math.round(Number(cfg?.taskbar_dimmer_fade_in_duration_ms) || 0))),
            fadeOutDurationMs: Math.max(0, Math.min(1000, Math.round(Number(cfg?.taskbar_dimmer_fade_out_duration_ms) || 0))),
          };
      const blackProfiles = {
        ...(cfg?.black_lift_profiles && typeof cfg.black_lift_profiles === 'object' ? cfg.black_lift_profiles : {}),
      };
      // The primary's profile identity (EDID id, disambiguated for identical
      // panels) so the tray's primary-scoped black-lift edit lands on the
      // same key the main window reads.
      const displayIdentity = String(display?.profile_id || display?.monitor_id || display?.device_name || '').trim();
      if (displayIdentity && display?.width && display?.height && display?.refresh_hz) {
        blackProfiles[`${displayIdentity}|${display.width}x${display.height}|${Math.round(display.refresh_hz)}`] = trayBlack.adjustment / 10;
      }
      await invoke('save_config', {
        config: {
          ...cfg,
          taskbar_dimmer_enabled: dimmerSettings.enabled,
          taskbar_dimmer_opacity: dimmerSettings.opacity,
          taskbar_dimmer_fade_in_duration_ms: dimmerSettings.fadeInDurationMs,
          taskbar_dimmer_fade_out_duration_ms: dimmerSettings.fadeOutDurationMs,
          // Keep the tray's (primary-scoped) edit in the per-monitor map so
          // the main window and a later startup read it back as a profile.
          taskbar_dimmer_profiles: {
            ...(cfg?.taskbar_dimmer_profiles && typeof cfg.taskbar_dimmer_profiles === 'object' ? cfg.taskbar_dimmer_profiles : {}),
            ...(trayDimmerMonKey() ? {
              [trayDimmerMonKey()]: {
                opacity: dimmerSettings.opacity,
                fadeInDurationMs: dimmerSettings.fadeInDurationMs,
                fadeOutDurationMs: dimmerSettings.fadeOutDurationMs,
                hoverDetection: cfg?.taskbar_dimmer_hover_detection || 'event',
                pollIntervalMs: Number(cfg?.taskbar_dimmer_poll_interval_ms) || 100,
              },
            } : {}),
          },
          // Screen Dimmer, same primary-scoped handling as the taskbar
          // dimmer: flat fields stay in sync with the primary's profile and
          // the per-monitor map keeps the tray's edit readable by the main
          // window and future startups.
          screen_protector_enabled: screenDimmer.enabled,
          screen_protector_seconds: screenDimmer.seconds,
          screen_protector_opacity: screenDimmer.opacity,
          screen_protector_fade_in_duration_ms: screenDimmer.fadeInDurationMs,
          screen_protector_fade_out_duration_ms: screenDimmer.fadeOutDurationMs,
          screen_protector_fade_linked: screenDimmer.fadeLinked,
          screen_protector_fullscreen_detection: screenDimmer.fullscreenDetection,
          screen_dimmer_profiles: {
            ...(cfg?.screen_dimmer_profiles && typeof cfg.screen_dimmer_profiles === 'object' ? cfg.screen_dimmer_profiles : {}),
            ...(trayDimmerMonKey() ? {
              [trayDimmerMonKey()]: {
                seconds: screenDimmer.seconds,
                opacity: screenDimmer.opacity,
                fadeInDurationMs: screenDimmer.fadeInDurationMs,
                fadeOutDurationMs: screenDimmer.fadeOutDurationMs,
                fullscreenDetection: screenDimmer.fullscreenDetection,
              },
            } : {}),
          },
          black_enabled: trayBlack.enabled,
          black_lift: trayBlack.adjustment / 10,
          black_lift_profiles: blackProfiles,
        },
      });
    } catch (e) {
      console.error('tray-menu: failed to save taskbar dimmer settings', e);
    } finally {
      dimmerSaveInFlight = false;
      if (dimmerSaveQueued) {
        dimmerSaveQueued = false;
        persistTrayDimmerSettings();
      }
    }
  }

  function scheduleTrayDimmerSave() {
    clearTimeout(dimmerSaveTimer);
    dimmerSaveTimer = setTimeout(() => {
      dimmerSaveTimer = null;
      persistTrayDimmerSettings();
    }, 250);
  }

  function applyTaskbarDimmerStatus(status) {
    if (!status) return;
    taskbarDimmer.enabled = status.enabled === true;
    taskbarDimmer.opacity = Math.max(0, Math.min(1, Number(status.opacity) || 0));
    taskbarDimmer.fadeInDurationMs = Math.max(0, Math.min(1000, Math.round(Number(status.fade_in_duration_ms) || 0)));
    taskbarDimmer.fadeOutDurationMs = Math.max(0, Math.min(1000, Math.round(Number(status.fade_out_duration_ms) || 0)));
    taskbarDimmer.available = status.available !== false;
  }

  async function loadTaskbarDimmer() {
    const requestVersion = quickInteractionVersion;
    try {
      const status = await invoke('get_taskbar_dimmer_status', { monitorKey: trayDimmerMonKey() });
      if (requestVersion === quickInteractionVersion) applyTaskbarDimmerStatus(status);
    } catch (e) {
      taskbarDimmer.available = false;
      console.error('tray-menu: failed to load taskbar dimmer status', e);
    }
  }

  async function loadScreenDimmer() {
    const requestVersion = quickInteractionVersion;
    try {
      const status = await invoke('power_get_screen_dimmer_status', { monitorKey: trayDimmerMonKey() });
      if (requestVersion === quickInteractionVersion) applyScreenDimmerStatus(status);
    } catch (e) {
      console.error('tray-menu: failed to load screen dimmer status', e);
    }
  }

  // The tray menu has no monitor selector: it operates on the primary
  // monitor's taskbar. Its profile key is the primary's EDID id (what
  // tray_menu_get_display_info reports), matching the main window's keying.
  function trayDimmerMonKey() {
    // The primary's profile identity (EDID id, disambiguated for identical
    // panels), matching the main window's key builders.
    return String(display?.profile_id || display?.monitor_id || '').trim();
  }

  function toggleTaskbarDimmer() {
    if (!taskbarDimmer.available) return;
    quickInteractionVersion++;
    setTaskbarDimmer(
      !taskbarDimmer.enabled,
      taskbarDimmer.opacity,
      taskbarDimmer.fadeInDurationMs,
      taskbarDimmer.fadeOutDurationMs,
    );
    scheduleTrayDimmerSave();
  }

  function handleTaskbarDimmerInput(event) {
    if (!taskbarDimmer.available) return;
    quickInteractionVersion++;
    const opacity = Math.max(0, Math.min(1, Number(event.currentTarget.value) / 100));
    setTaskbarDimmer(
      taskbarDimmer.enabled,
      opacity,
      taskbarDimmer.fadeInDurationMs,
      taskbarDimmer.fadeOutDurationMs,
      true,
      true,
    );
    scheduleTrayDimmerSave();
  }

  function applyScreenDimmerStatus(status) {
    if (!status) return;
    screenDimmer.enabled = status.enabled === true;
    screenDimmer.seconds = Math.max(5, Math.min(180, Math.round(Number(status.seconds) || 30)));
    screenDimmer.opacity = Math.max(0, Math.min(1, Number(status.opacity) || 0));
    screenDimmer.fadeInDurationMs = Math.max(0, Math.min(5000, Math.round(Number(status.fade_in_duration_ms) || 0)));
    screenDimmer.fadeOutDurationMs = Math.max(0, Math.min(5000, Math.round(Number(status.fade_out_duration_ms) || 0)));
    screenDimmer.fullscreenDetection = status.fullscreen_detection === true;
  }

  function toggleScreenDimmer() {
    quickInteractionVersion++;
    // Route through the elevation-aware toggle (same path the Screen Dimmer
    // tab uses): the dimmer only runs in an elevated session, so enabling
    // from a non-elevated tray must trigger the UAC relaunch too, otherwise
    // the flag flips but the watcher never starts. tab=null because the tray
    // has no tab context to reopen after the elevated restart.
    setScreenDimmerEnabled(!screenDimmer.enabled, null)
      .catch((e) => console.warn('[screen-dimmer] tray toggle failed', e));
    scheduleTrayDimmerSave();
  }

  async function loadTrayElevation() {
    try {
      const elevated = await invoke('is_process_elevated');
      trayElevated = elevated === true;
    } catch (e) {
      console.error('tray-menu: failed to check elevation', e);
    }
  }

  // The Screen Dimmer only runs elevated. If the user enabled it in a
  // non-elevated session (declined the UAC prompt, or the startup prompt was
  // declined), this relaunches the app elevated so the dimmer actually
  // starts. Same command the Screen Dimmer tab's admin banner uses.
  function relaunchScreenDimmerElevated() {
    invoke('relaunch_elevated', {
      rtxHdrFlags: null,
      rtxHdrEnabled: null,
      hdrMode: null,
      screenDimmerEnabled: true,
      tab: null,
    }).catch((e) => console.warn('[screen-dimmer] tray relaunch elevated failed', e));
  }

  // The tray is narrow, so the timer slider moves in 5-second steps (the
  // tab uses 1-second steps), coarse enough to resolve on a ~170px drag
  // across the full 5-180 range.
  function handleScreenDimmerInput(event) {
    quickInteractionVersion++;
    const seconds = Math.max(5, Math.min(180, Math.round(Number(event.currentTarget.value) || 30)));
    // Update the view state so the slider tracks the drag immediately, then
    // push to the backend (setScreenDimmer does not update the view itself).
    screenDimmer.seconds = seconds;
    setScreenDimmer(
      screenDimmer.enabled,
      seconds,
      screenDimmer.opacity,
      screenDimmer.fadeInDurationMs,
      screenDimmer.fadeOutDurationMs,
      screenDimmer.fullscreenDetection,
      true,
    );
    scheduleTrayDimmerSave();
  }

  function applyTaskbarDimmerEvent(status) {
    if (!status) return;
    // The tray menu is primary-scoped: ignore dimmer edits the main window
    // made on a secondary monitor (their monitorKey won't match the primary's
    // profile identity the tray reports).
    const trayKey = String(display?.profile_id || display?.monitor_id || '').trim();
    if (trayKey && status.monitorKey && trayKey !== status.monitorKey) return;
    quickInteractionVersion++;
    if (typeof status.enabled === 'boolean') taskbarDimmer.enabled = status.enabled;
    if (typeof status.opacity === 'number') taskbarDimmer.opacity = Math.max(0, Math.min(1, status.opacity));
    if (typeof status.fadeInDurationMs === 'number') taskbarDimmer.fadeInDurationMs = Math.max(0, Math.min(1000, Math.round(status.fadeInDurationMs)));
    if (typeof status.fadeOutDurationMs === 'number') taskbarDimmer.fadeOutDurationMs = Math.max(0, Math.min(1000, Math.round(status.fadeOutDurationMs)));
  }

  function formatTrayBlackLevel() {
    if (!trayBlack.enabled) return 'OFF';
    return `${trayBlack.adjustment > 0 ? '+' : ''}${(trayBlack.adjustment / 10).toFixed(1)}`;
  }

  function queueBlackLevelApply() {
    pendingBlackApply = {
      blackLift: trayBlack.enabled ? trayBlack.adjustment : 0,
      adjustment: trayBlack.adjustment,
      enabled: trayBlack.enabled,
    };
    if (blackApplyInFlight) return;
    const next = pendingBlackApply;
    pendingBlackApply = null;
    blackApplyInFlight = true;
    invoke('apply_gamma_ramp', { blackLift: next.blackLift })
      .then(() => {
        emit('black-level-changed', { adjustment: next.adjustment, enabled: next.enabled }).catch(() => {});
        scheduleTrayDimmerSave();
      })
      .catch((e) => console.error('tray-menu: failed to apply Black Level', e))
      .finally(() => {
        blackApplyInFlight = false;
        if (pendingBlackApply) queueBlackLevelApply();
      });
  }

  function toggleTrayBlackLevel() {
    quickInteractionVersion++;
    trayBlack.enabled = !trayBlack.enabled;
    queueBlackLevelApply();
  }

  // The tray is narrow, so user interaction moves in 0.5 steps (5 raw
  // units), coarse enough to resolve on a ~170px drag across the full
  // ±15.0 range. The input itself keeps step=1 so values set by the main
  // app (any 0.1 increment) render exactly; only drag/wheel/arrow input is
  // snapped to 0.5 here.
  function handleTrayBlackLevelInput(event) {
    if (!trayBlack.enabled) return;
    quickInteractionVersion++;
    const value = Math.round(Number(event.currentTarget.value));
    trayBlack.adjustment = Math.max(-150, Math.min(150, Math.round(value / 5) * 5));
    queueBlackLevelApply();
  }

  function handleTrayBlackWheel(event) {
    if (!trayBlack.enabled) return;
    event.preventDefault();
    quickInteractionVersion++;
    const dir = event.deltaY < 0 ? 1 : -1;
    trayBlack.adjustment = Math.max(-150, Math.min(150, trayBlack.adjustment + dir * 5));
    queueBlackLevelApply();
  }

  function handleTrayBlackKeydown(event) {
    if (!trayBlack.enabled) return;
    if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key)) return;
    event.preventDefault();
    quickInteractionVersion++;
    const dir = (event.key === 'ArrowRight' || event.key === 'ArrowUp') ? 1 : -1;
    trayBlack.adjustment = Math.max(-150, Math.min(150, trayBlack.adjustment + dir * 5));
    queueBlackLevelApply();
  }

  // Read the current SDR→HDR mode the same way the main app's Windows HDR
  // tab derives it: Auto HDR state from the registry, RTX HDR state from the
  // NVIDIA driver (only when the primary GPU is NVIDIA). Both read commands
  // are registered for this window's IPC. Uses the CACHED vendor query
  // (query_vendor_display_info, 60 s TTL) instead of the fresh one: the tray
  // opens on demand, and paying a full NVAPI/ADLX session query per open
  // made it slower than it needs to be. The display-change path invalidates
  // the driver context caches, so the first open after a resolution/DPI
  // change still re-queries fresh.
  async function loadHdrEnhancementMode() {
    try {
      const vendorInfo = await invoke('query_vendor_display_info');
      trayHdrIsNvidia = vendorInfo?.vendor === 'nvidia';
      const autoEnabled = !!await invoke('get_auto_hdr_enabled');
      let rtxEnabled = false;
      if (trayHdrIsNvidia) {
        const rtxSettings = await invoke('get_rtx_hdr_settings_state').catch(() => null);
        rtxEnabled = !!rtxSettings?.enabled;
      }
      trayHdrMode = autoEnabled ? 'auto' : rtxEnabled ? 'rtx' : 'off';
      trayHdrModeError = '';
    } catch (e) {
      console.error('tray-menu: failed to load HDR mode', e);
    }
  }

  // Apply an SDR→HDR mode from the tray. Same single backend command as the
  // main app's selector; RTX writes that need elevation relaunch the app
  // elevated (the elevated child performs the full transition).
  async function selectTrayHdrMode(mode) {
    if (trayHdrModeBusy || mode === trayHdrMode) return;
    if (mode === 'rtx' && !trayHdrIsNvidia) return;
    trayHdrModeBusy = true;
    trayHdrModeError = '';
    try {
      await invoke('set_hdr_enhancement_mode', { mode });
      await loadHdrEnhancementMode();
    } catch (e) {
      trayHdrModeError = String(e);
      if (trayHdrModeError.startsWith('ADMIN_REQUIRED:')) {
        // Elevated relaunch applies the pending mode itself; there is no
        // main-window tab to reopen on from the tray.
        invoke('relaunch_elevated', {
          rtxHdrFlags: null,
          rtxHdrEnabled: null,
          hdrMode: mode,
          tab: null,
        }).catch(() => {});
      }
    } finally {
      trayHdrModeBusy = false;
    }
  }

  async function loadSdrContentBrightness() {
    const requestVersion = quickInteractionVersion;
    try {
      const info = await invoke('get_sdr_white_level');
      const nits = Number(info?.nits);
      if (requestVersion !== quickInteractionVersion) return;
      traySdrBrightness = Number.isFinite(nits) && nits >= 80 && nits <= 480
        ? { available: true, nits }
        : { available: false, nits: null };
    } catch (e) {
      if (requestVersion === quickInteractionVersion) traySdrBrightness = { available: false, nits: null };
      console.error('tray-menu: failed to load SDR Content Brightness', e);
    }
  }

  // Windows HDR status via the same matchMedia probe the main window uses.
  // The tray is a separate WebView, so it keeps its own copy instead of
  // importing the main window's in-memory store.
  function pollTrayHdrStatus() {
    let status = 'unknown';
    try { status = window.matchMedia('(dynamic-range: high)').matches ? 'hdr' : 'sdr'; } catch {}
    trayHdrStatus = status;
    // SDR Content Brightness only exists while HDR is active; clear the row's
    // value when the mode flips to SDR so it never shows a stale reading.
    if (status !== 'hdr') traySdrBrightness = { available: false, nits: null };
  }

  // Same safe toggle path as the main app's status-bar switch: optimistically
  // flip the UI, apply through the OS-level command, revert on failure.
  async function toggleTrayHdr() {
    if (windowsHdrBusy || trayHdrStatus === 'unknown') return;
    const next = trayHdrStatus !== 'hdr';
    const previous = trayHdrStatus;
    windowsHdrBusy = true;
    trayHdrStatus = next ? 'hdr' : 'sdr';
    windowsHdrError = '';
    try {
      await invoke('set_windows_hdr_enabled', { enabled: next });
      // SDR Content Brightness only becomes readable while HDR is active, so
      // re-read it after the mode settles.
      loadSdrContentBrightness();
    } catch (e) {
      windowsHdrError = String(e);
      trayHdrStatus = previous;
    } finally {
      windowsHdrBusy = false;
      // The SDR row mounts/unmounts with the mode, changing the base menu's
      // height, re-measure so the native click-through region matches the
      // visible card (syncHitRegion awaits tick internally).
      syncHitRegion();
    }
  }

  function drainSdrBrightnessApply() {
    if (sdrApplyInFlight || pendingSdrNits === null) return;
    const requestedNits = pendingSdrNits;
    pendingSdrNits = null;
    sdrApplyInFlight = true;
    sdrLastApplyTime = Date.now();
    invoke('set_sdr_brightness_nits', { nits: requestedNits })
      .then((result) => {
        const applied = Number(result?.nits);
        if (Number.isFinite(applied) && traySdrBrightness.nits === requestedNits) traySdrBrightness.nits = applied;
        if (Number.isFinite(applied)) emit('sdr-content-brightness-changed', { nits: applied }).catch(() => {});
      })
      .catch((e) => console.error('tray-menu: failed to apply SDR Content Brightness', e))
      .finally(() => {
        sdrApplyInFlight = false;
        // A newer value queued while this write was in flight applies right
        // away so the final value always lands (trailing guarantee), same as
        // the main window's slider.
        if (pendingSdrNits !== null) drainSdrBrightnessApply();
      });
  }

  function handleTraySdrBrightnessInput(event) {
    if (!traySdrBrightness.available) return;
    quickInteractionVersion++;
    const nits = Math.max(80, Math.min(480, Math.round(Number(event.currentTarget.value))));
    traySdrBrightness.nits = nits;
    pendingSdrNits = nits;
    // Leading-edge throttle, matching the main app's slider: during a
    // continuous drag the screen updates live instead of waiting for a pause.
    // A pure trailing debounce never fires while inputs arrive faster than
    // the delay (a drag emits ~16-33ms apart), which is why the tray slider
    // lagged behind the main window one.
    const since = Date.now() - sdrLastApplyTime;
    const delay = since >= SDR_APPLY_THROTTLE_MS ? 0 : SDR_APPLY_THROTTLE_MS - since;
    clearTimeout(sdrApplyTimer);
    sdrApplyTimer = setTimeout(() => {
      sdrApplyTimer = null;
      drainSdrBrightnessApply();
    }, delay);
  }

  function applyBlackLevelEvent(status) {
    if (!status) return;
    quickInteractionVersion++;
    if (typeof status.adjustment === 'number') trayBlack.adjustment = Math.max(-150, Math.min(150, Math.round(status.adjustment)));
    if (typeof status.enabled === 'boolean') trayBlack.enabled = status.enabled;
  }

  function applySdrBrightnessEvent(status) {
    const nits = Number(status?.nits);
    if (!Number.isFinite(nits) || nits < 80 || nits > 480) return;
    quickInteractionVersion++;
    traySdrBrightness = { available: true, nits };
  }

  let display = $state(null); // { width, height, refresh_hz, refresh_rates, resolutions: [{width,height}], ... }
  let favorites = $state([]);
  let skipFavoriteConfirmation = $state(false); // [{ width, height, hz }]
  // Keep the tray's resolution labels in sync with the main app's persisted
  // "Show tags (4K, QHD, UW)" preference.
  let showResolutionTags = $state(true);
  // Mirror the main app's persisted list directions. The tray keeps its own
  // copies because it runs in a separate WebView and cannot import the main
  // window's in-memory store.
  // One sort direction drives both the resolution and refresh-rate lists in
  // the main app (the refresh dropdown reuses the resolution context menu),
  // so the tray mirrors that single value for both submenus.
  let resolutionSort = $state('desc');
  // Mirror the main app's "Group by aspect ratio" dropdown preference
  // ('none' | 'aspectRatio'), kept in sync through display-menu-groupby-changed.
  let resolutionGroupBy = $state('none');
  // Which submenu (if any) is currently open: null | 'resolution' | 'refresh' | 'favorites'.
  let openSubmenu = $state(null);
  // The main app confirms destructive removals. Keep the same safeguard in
  // the tray without using a blocking browser confirm dialog.
  let pendingFavoriteRemove = $state(null);
  // Serialize favorite writes and hold the latest cross-window event until
  // the local write queue drains, so concurrent stars cannot overwrite one
  // another or lose a newer change from the main window.
  let favoritesSaveInFlight = 0;
  let favoritesSaveQueue = Promise.resolve();
  let pendingExternalFavorites = null;
  let closeSubmenuTimer = null;
  // Serialize native region updates so a quick enter/leave cannot apply an
  // older submenu shape after the current one. The OS region, not just CSS,
  // must match the visible cards because the rest of the WebView is
  // transparent but would otherwise still intercept clicks.
  let hitRegionSync = Promise.resolve();
  // The submenu is shorter than the base menu and both panes bottom-align to
  // the fixed tray canvas. Lift the submenu by the measured height
  // difference so its top edge matches the base menu's top edge (see
  // syncHitRegion).
  let submenuLift = $state(0);

  async function loadDisplayInfo() {
    try {
      display = await invoke('tray_menu_get_display_info');
      applyTrayBlackProfileForCurrentDisplay();
      resetSubmenuScroll();
      syncHitRegion();
    } catch (e) {
      console.error('tray-menu: failed to load display info', e);
    }
  }

  async function loadFavorites() {
    try {
      const cfg = await invoke('load_config');
      showResolutionTags = cfg?.display_menu_show_tags !== false;
      skipFavoriteConfirmation = cfg?.skip_favorite_confirmation === true;
      trayBlack.enabled = cfg?.black_enabled !== false;
      trayBlack.adjustment = Math.max(-150, Math.min(150, Math.round((Number(cfg?.black_lift) || 0) * 10)));
      trayBlackProfiles = cfg?.black_lift_profiles && typeof cfg.black_lift_profiles === 'object'
        ? Object.fromEntries(Object.entries(cfg.black_lift_profiles).filter(([key, value]) => !/^\d+$/.test(key) && typeof value === 'number' && Number.isFinite(value)))
        : {};
      trayLegacyBlackProfiles = cfg?.black_lift_profiles && typeof cfg.black_lift_profiles === 'object'
        ? Object.fromEntries(Object.entries(cfg.black_lift_profiles).filter(([key, value]) => /^\d+$/.test(key) && typeof value === 'number' && Number.isFinite(value)))
        : {};
      applyTrayBlackProfileForCurrentDisplay();
      resolutionSort = cfg?.display_menu_resolution_sort === 'asc' ? 'asc' : 'desc';
      resolutionGroupBy = cfg?.display_menu_resolution_group_by === 'aspectRatio' ? 'aspectRatio' : 'none';
      favorites = Array.isArray(cfg?.favorite_display_modes)
        ? cfg.favorite_display_modes
            .filter((f) => f && f.width > 0 && f.height > 0 && f.hz > 0)
            .map((f) => ({
              width: Math.round(f.width),
              height: Math.round(f.height),
              hz: Math.round(f.hz),
              fav_id: typeof f.fav_id === 'string' ? f.fav_id : '',
              monitor_id: typeof f.monitor_id === 'string' ? f.monitor_id : '',
              monitor_sig: typeof f.monitor_sig === 'string' ? f.monitor_sig : '',
              monitor_device: typeof f.monitor_device === 'string' ? f.monitor_device : '',
              monitor_name: typeof f.monitor_name === 'string' ? f.monitor_name : '',
              confirmed_monitor_id: typeof f.confirmed_monitor_id === 'string' ? f.confirmed_monitor_id : '',
            }))
        : [];
      syncHitRegion();
    } catch (e) {
      console.error('tray-menu: failed to load favorites', e);
    }
  }

  // Preserve the monitor-scoping fields (fav_id, monitor_id, monitor_sig,
  // monitor_device, monitor_name) so a tray-side add/remove never strips
  // them from the saved favorites.
  function normalizeFavorites(list) {
    return Array.isArray(list)
      ? list
          .filter((f) => f && f.width > 0 && f.height > 0 && f.hz > 0)
          .map((f) => ({
            width: Math.round(f.width),
            height: Math.round(f.height),
            hz: Math.round(f.hz),
            fav_id: typeof f.fav_id === 'string' ? f.fav_id : '',
            monitor_id: typeof f.monitor_id === 'string' ? f.monitor_id : '',
            monitor_sig: typeof f.monitor_sig === 'string' ? f.monitor_sig : '',
            monitor_device: typeof f.monitor_device === 'string' ? f.monitor_device : '',              monitor_name: typeof f.monitor_name === 'string' ? f.monitor_name : '',
              confirmed_monitor_id: typeof f.confirmed_monitor_id === 'string' ? f.confirmed_monitor_id : '',
            }))
      : [];
  }

  function applyFavoritesList(next) {
    favorites = next;
    pendingFavoriteRemove = null;
    syncHitRegion();
  }

  function setFavoritesFromEvent(list) {
    const next = normalizeFavorites(list);
    if (favoritesSaveInFlight > 0) {
      pendingExternalFavorites = next;
      return;
    }
    applyFavoritesList(next);
  }

  async function saveFavorites(next) {
    const savedFavorites = normalizeFavorites(next);
    favorites = savedFavorites;
    favoritesSaveInFlight++;
    // The inline/direct layout can change immediately at the 3→4 boundary;
    // update the native hit region without waiting for disk I/O.
    syncHitRegion();
    const operation = favoritesSaveQueue.then(async () => {
      const cfg = await invoke('load_config');
      await invoke('save_config', {
        config: { ...cfg, favorite_display_modes: savedFavorites },
      });
      await emit('favorites-changed', savedFavorites);
    });
    favoritesSaveQueue = operation.catch(() => {});
    try {
      await operation;
    } catch (e) {
      console.error('tray-menu: failed to save favorites', e);
    } finally {
      favoritesSaveInFlight--;
      if (favoritesSaveInFlight === 0 && pendingExternalFavorites) {
        const external = pendingExternalFavorites;
        pendingExternalFavorites = null;
        applyFavoritesList(external);
      }
    }
  }

  // The native window is built at a fixed full-size footprint (see
  // tray_menu_window.rs) that fits both the base menu and any submenu; only
  // the DOM here changes on hover, not the window itself.
  function cancelScheduledSubmenuClose() {
    clearTimeout(closeSubmenuTimer);
    closeSubmenuTimer = null;
  }

  function openSubmenuFor(name) {
    cancelScheduledSubmenuClose();
    openSubmenu = name;
    resetSubmenuScroll();
    syncHitRegion();
  }

  // The main app's ascending order puts the largest value at the bottom of
  // the list, while descending puts it at the top. Reapply that endpoint every
  // time a numeric submenu opens (and after its list is refreshed), so users
  // always start at the highest resolution/refresh rate rather than inheriting
  // the previous scroll position from the WebView.
  function resetSubmenuScroll() {
    const name = openSubmenu;
    if (name !== 'resolution' && name !== 'refresh') return;
    tick().then(() => requestAnimationFrame(() => {
      if (openSubmenu !== name) return;
      const submenu = document.querySelector('.tray-submenu');
      if (!submenu) return;
      const direction = resolutionSort;
      submenu.scrollTop = direction === 'asc'
        ? Math.max(0, submenu.scrollHeight - submenu.clientHeight)
        : 0;
    }));
  }

  function sortedResolutions() {
    const dir = resolutionSort === 'asc' ? 1 : -1;
    return [...(display?.resolutions || [])].sort((a, b) => (
      dir * ((a.width * a.height) - (b.width * b.height))
    ));
  }

  // ── Group-by-aspect-ratio helpers, mirroring App.svelte so the tray's
  // resolution submenu matches the main app's dropdown exactly. ────────────
  // Simplify a width/height pair to a small aspect-ratio label. Match the
  // small set of ratios actually used to market monitors first (within a 3%
  // tolerance), then fall back to a GCD-reduced fraction.
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
  // Popularity rank for common display aspect ratios, most-used first; used
  // to order the groups independently from the nearest-match list above.
  const ASPECT_RATIO_POPULARITY = ['16:9', '16:10', '21:9', '32:9', '4:3', '3:2', '5:4', '1:1'];
  function aspectRatioRank(label) {
    const i = ASPECT_RATIO_POPULARITY.indexOf(label);
    return i === -1 ? ASPECT_RATIO_POPULARITY.length : i;
  }
  function groupedResolutions() {
    const dir = resolutionSort === 'asc' ? 1 : -1;
    const groups = new Map();
    for (const r of display?.resolutions || []) {
      const label = aspectRatioLabel(r.width, r.height);
      if (!groups.has(label)) groups.set(label, []);
      groups.get(label).push(r);
    }
    const entries = [...groups.entries()].map(([label, list]) => {
      list.sort((a, b) => dir * ((a.width * a.height) - (b.width * b.height)));
      const maxPixels = Math.max(...list.map((r) => r.width * r.height));
      return { label, list, maxPixels, rank: aspectRatioRank(label) };
    });
    entries.sort((a, b) => a.rank - b.rank || b.maxPixels - a.maxPixels);
    return entries;
  }

  function sortedRefreshRates() {
    const dir = resolutionSort === 'asc' ? 1 : -1;
    return [...(display?.refresh_rates || [])].sort((a, b) => dir * (a - b));
  }

  function trayBlackProfileKey(hz) {
    // The primary's profile identity (EDID id, disambiguated for identical
    // panels) so the tray keys like the main window.
    const identity = String(display?.profile_id || display?.monitor_id || display?.device_name || '').trim() || 'unknown-display';
    if (!display?.width || !display?.height || !hz) return null;
    return `${identity}|${display.width}x${display.height}|${Math.round(Number(hz))}`;
  }

  function clampTrayBlackAdjustment(value) {
    return Math.max(-150, Math.min(150, Math.round(Number(value) || 0)));
  }

  function applyTrayBlackProfileForCurrentDisplay() {
    const key = trayBlackProfileKey(display?.refresh_hz);
    if (key && Object.prototype.hasOwnProperty.call(trayBlackProfiles, key)) {
      trayBlack.adjustment = clampTrayBlackAdjustment(Number(trayBlackProfiles[key]) * 10);
      return;
    }
    const legacyKey = display?.refresh_hz ? String(Math.round(Number(display.refresh_hz))) : null;
    if (legacyKey && Object.prototype.hasOwnProperty.call(trayLegacyBlackProfiles, legacyKey)) {
      trayBlack.adjustment = clampTrayBlackAdjustment(Number(trayLegacyBlackProfiles[legacyKey]) * 10);
    }
  }

  function formatTrayBlackLevelForRefreshRate(hz) {
    const key = trayBlackProfileKey(hz);
    const value = key && Math.round(Number(hz)) === Math.round(Number(display?.refresh_hz))
      ? trayBlack.adjustment
      : key && Object.prototype.hasOwnProperty.call(trayBlackProfiles, key)
        ? clampTrayBlackAdjustment(Number(trayBlackProfiles[key]) * 10)
        : Object.prototype.hasOwnProperty.call(trayLegacyBlackProfiles, String(Math.round(Number(hz))))
          ? clampTrayBlackAdjustment(Number(trayLegacyBlackProfiles[String(Math.round(Number(hz)))]) * 10)
          : 0;
    if (!value) return '';
    return `${value > 0 ? '+' : ''}${(value / 10).toFixed(1)}`;
  }

  function closeSubmenu() {
    cancelScheduledSubmenuClose();
    if (openSubmenu === null) return;
    openSubmenu = null;
    syncHitRegion();
  }

  // Give the pointer a short bridge while the native region catches up after
  // the submenu is shown. A real exit still closes promptly, but a transient
  // mouseleave at the pane seam cannot tear down the submenu mid-transition.
  function scheduleSubmenuClose() {
    cancelScheduledSubmenuClose();
    closeSubmenuTimer = setTimeout(() => {
      closeSubmenuTimer = null;
      closeSubmenu();
    }, 100);
  }

  function menuRect(element) {
    if (!element) return null;
    // offset* reports the settled layout box relative to the positioned
    // wrapper and deliberately ignores the card's entrance transform. Using
    // getBoundingClientRect() here would capture the transient scale/offset
    // and leave the native region misaligned after the animation finishes.
    return {
      x: element.offsetLeft,
      y: element.offsetTop,
      width: element.offsetWidth,
      height: element.offsetHeight,
    };
  }

  // Resize neither the native window nor the layout. Instead, tell Rust the
  // exact visible card rectangles and let Windows hit-test only those rounded
  // regions. This keeps the fixed taskbar anchor stable and makes the
  // transparent canvas outside the cards click through to the window below.
  function syncHitRegion() {
    // Install a conservative bridge before waiting for Svelte's DOM update.
    // Without this first pass, the submenu is visible in the browser while
    // Windows still has only the collapsed base-menu region; a fast pointer
    // crossing can blur/hide the window before the exact measurement arrives.
    const preBase = openSubmenu !== null
      ? menuRect(document.querySelector('.tray-menu'))
      : null;
    const preinstall = preBase
      ? {
          // The submenu pane is auto-sized now (max-content, up to 334px
          // outer), so this preinstalled bridge only needs a conservative
          // left region, the exact DOM measurement replaces it a tick
          // later. 244px covers the collapsed/minimum-width case; keep the
          // base menu's height since the submenu is no taller.
          x: Math.max(0, preBase.x - 244),
          y: preBase.y,
          width: 244,
          height: preBase.height,
        }
      : null;

    hitRegionSync = hitRegionSync.then(async () => {
      if (preinstall && openSubmenu !== null) {
        try {
          await invoke('tray_menu_set_expanded', {
            expanded: true,
            base: preBase,
            submenu: preinstall,
          });
        } catch (e) {
          console.error('tray-menu: failed to preinstall hit region', e);
        }
      }
      await tick();
      const base = menuRect(document.querySelector('.tray-menu'));
      if (!base) return;
      const expanded = openSubmenu !== null;
      const submenuEl = expanded
        ? document.querySelector('.tray-submenu')
        : null;
      // Bottom-aligned panes: push the shorter submenu up so both cards
      // share a top edge while the base menu stays anchored to the taskbar.
      // For the short Favorites submenu that top edge sits far above the
      // Favorites row, out of the cursor's path, align its top to the
      // anchor row instead so the pointer can cross straight from the row
      // into the card. (Resolution/Refresh Rate rows sit near the top, so
      // their existing top-edge alignment is kept.)
      const preLift = submenuEl ? menuRect(submenuEl) : null;
      let lift = preLift ? Math.max(0, base.height - preLift.height) : 0;
      if (preLift && openSubmenu === 'favorites') {
        const baseEl = document.querySelector('.tray-menu');
        const anchorEl = document.querySelector('.tray-menu .tray-item.open');
        if (baseEl && anchorEl) {
          // Viewport-coordinate difference (same frame): both elements move
          // with .tray-menu's entrance transform, so the offset cancels out
          // and stays stable even mid-animation. offsetTop alone would mix
          // the wrapper frame with .tray-menu's transform-created offset
          // parent frame.
          const anchorTop = anchorEl.getBoundingClientRect().top - baseEl.getBoundingClientRect().top;
          lift = Math.max(0, base.height - preLift.height - Math.max(0, anchorTop));
        }
      }
      submenuLift = lift;
      // Re-measure after Svelte applies the margin so the native region sent
      // to Rust matches the submenu's lifted position.
      await tick();
      const submenu = submenuEl ? menuRect(submenuEl) : null;
      try {
        await invoke('tray_menu_set_expanded', {
          expanded,
          base,
          submenu,
        });
      } catch (e) {
        console.error('tray-menu: failed to update hit region', e);
      }
    });
  }

  // Favorites are scoped to the monitor they were saved on (the main app's
  // keying: EDID monitor_id first, sig as the legacy fallback). The tray is
  // primary-scoped, so matching, starring, saving, and removing all compare
  // against the primary monitor's identity. Without this, favorites added
  // from the tray were saved unscoped and never matched the main app's
  // star/dedupe, and same-mode favorites from different monitors collided
  // as {#each} keys.
  function trayMonitorIdentity() {
    return String(display?.monitor_id || '').trim();
  }
  function favoriteIdentityOf(fav) {
    return String(fav?.monitor_id || fav?.monitor_sig || '').trim();
  }
  function favoriteMatchKey(fav) {
    return `${favoriteIdentityOf(fav)}|${favoriteKey(fav)}`;
  }
  function newTrayFavoriteId() {
    try {
      if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
        return crypto.randomUUID();
      }
    } catch { /* fall through to the local fallback */ }
    return `fav-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
  }

  function isFavoriteMode(width, height, hz) {
    const identity = trayMonitorIdentity();
    if (!identity) return false;
    const wanted = `${identity}|${width}x${height}@${hz}`;
    return favorites.some((fav) => favoriteMatchKey(fav) === wanted);
  }

  function requestFavoriteRemove(fav) {
    pendingFavoriteRemove = {
      width: fav.width,
      height: fav.height,
      hz: fav.hz,
      monitor_id: fav.monitor_id,
      monitor_sig: fav.monitor_sig,
    };
    // Inline confirms replace the row in place, so only close a Resolution/
    // Refresh Rate submenu (which has nothing to do with this favorite).
    // Leave the Favorites submenu open so its own inline confirm row stays
    // visible instead of the whole pane vanishing under the cursor.
    if (openSubmenu !== 'favorites') closeSubmenu();
    syncHitRegion();
  }

  function cancelFavoriteRemove() {
    pendingFavoriteRemove = null;
    syncHitRegion();
  }

  async function confirmFavoriteRemove() {
    if (!pendingFavoriteRemove) return;
    const key = favoriteMatchKey(pendingFavoriteRemove);
    pendingFavoriteRemove = null;
    syncHitRegion();
    await saveFavorites(favorites.filter((fav) => favoriteMatchKey(fav) !== key));
  }

  async function toggleFavoriteMode(width, height, hz) {
    if (!width || !height || !hz) return;
    const fav = { width, height, hz };
    if (isFavoriteMode(width, height, hz)) {
      requestFavoriteRemove(fav);
      return;
    }
    // Save scoped to the primary monitor (the tray's scope), so the main
    // app's star/dedupe and the tray agree on which modes are favorited. The
    // GDI device name pins the panel even when two identical monitors share
    // the EDID id.
    await saveFavorites([...favorites, {
      ...fav,
      fav_id: newTrayFavoriteId(),
      monitor_id: trayMonitorIdentity(),
      monitor_sig: '',
      monitor_device: String(display?.device_name || ''),              monitor_name: String(display?.monitor_name || ''),
              confirmed_monitor_id: '',
    }]);
  }

  async function pickResolution(res) {
    if (display && res.width === display.width && res.height === display.height) return;
    closeSubmenu();
    await invoke('tray_menu_set_resolution', { width: res.width, height: res.height });
    await invoke('tray_menu_hide');
  }

  async function pickRefreshRate(hz) {
    if (display && hz === display.refresh_hz) return;
    closeSubmenu();
    await invoke('tray_menu_set_refresh_rate', { hz });
    await invoke('tray_menu_hide');
  }

  async function pickFavorite(fav) {
    if (isCurrentFavorite(fav)) return;
    closeSubmenu();
    // Target the monitor the favorite was saved on (the backend resolves the
    // device name, falling back to the EDID id, to that display), so a
    // secondary-monitor favorite is never silently applied to the primary;
    // the backend skips it if disconnected. The device name disambiguates
    // two identical panels sharing the EDID id.
    await invoke('tray_menu_set_resolution', {
      width: fav.width,
      height: fav.height,
      hz: fav.hz,
      monitorId: fav.monitor_id || null,
      monitorDevice: fav.monitor_device || null,
      skipConfirmation: skipFavoriteConfirmation
        && Boolean(fav.confirmed_monitor_id)
        && Boolean(fav.monitor_id)
        && fav.confirmed_monitor_id === fav.monitor_id,
    });
    await invoke('tray_menu_hide');
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

  function isUltrawide(width, height) {
    return width / height > 1.9;
  }

  function isRecommendedResolution(resolution) {
    if (!display) return false;
    if (display.native_width && display.native_height) {
      return resolution.width === display.native_width && resolution.height === display.native_height;
    }
    const resolutions = Array.isArray(display.resolutions) ? display.resolutions : [];
    if (resolutions.length === 0) return false;
    const largest = resolutions.reduce((best, candidate) => (
      candidate.width * candidate.height > best.width * best.height ? candidate : best
    ));
    return resolution.width === largest.width && resolution.height === largest.height;
  }

  function favoriteKey(f) {
    return `${f.width}x${f.height}@${f.hz}`;
  }

  function favoriteLabel(fav) {
    return `${fav.width}×${fav.height} @ ${fav.hz} Hz`;
  }

  function isCurrentFavorite(fav) {
    // A favorite only counts as the active mode on the monitor it was saved
    // for; the tray compares against the primary monitor's identity.
    return display
      && fav.width === display.width
      && fav.height === display.height
      && fav.hz === display.refresh_hz
      && favoriteIdentityOf(fav) === trayMonitorIdentity();
  }

  async function restart() {
    await invoke('tray_menu_restart');
  }

  async function openMain() {
    await invoke('tray_menu_open_main');
  }

  async function quit() {
    await invoke('tray_menu_quit');
  }

  onMount(() => {
    syncHitRegion();
    loadDisplayInfo();
    loadFavorites();
    loadTaskbarDimmer();
    loadScreenDimmer();
    loadTrayElevation();
    loadHdrEnhancementMode();
    loadSdrContentBrightness();
    pollTrayHdrStatus();
    getVersion().then((v) => { if (typeof v === 'string' && v) appVersion = v; }).catch(() => {});
    const unlisten = listen('tray-menu-show', () => {
      openSubmenu = null;
      pendingFavoriteRemove = null;
      syncHitRegion();
      loadDisplayInfo();
      loadFavorites();
      loadTaskbarDimmer();
      loadScreenDimmer();
      loadTrayElevation();
      loadHdrEnhancementMode();
      loadSdrContentBrightness();
      pollTrayHdrStatus();
    });
    // Follow external HDR changes (Windows hotkey, Settings, main app switch)
    // so the tray switch and SDR row stay in sync while the popup is open.
    const hdrMq = window.matchMedia('(dynamic-range: high)');
    function onTrayHdrChange() {
      pollTrayHdrStatus();
      loadSdrContentBrightness();
      // External HDR changes can also mount/unmount the SDR row; keep the
      // native hit region in sync with the resized base menu.
      syncHitRegion();
    }
    try { hdrMq.addEventListener('change', onTrayHdrChange); } catch {}
    const unlistenTags = listen('display-menu-tags-changed', (event) => {
      showResolutionTags = event.payload !== false;
    });
    const unlistenSort = listen('display-menu-sorting-changed', (event) => {
      const prefs = event.payload || {};
      if (prefs.resolutionSort === 'asc' || prefs.resolutionSort === 'desc') {
        resolutionSort = prefs.resolutionSort;
      }
      resetSubmenuScroll();
    });
    const unlistenGroupBy = listen('display-menu-groupby-changed', (event) => {
      resolutionGroupBy = event.payload === 'aspectRatio' ? 'aspectRatio' : 'none';
      resetSubmenuScroll();
    });
    const unlistenFavorites = listen('favorites-changed', (event) => {
      setFavoritesFromEvent(event.payload);
    });
    const unlistenDimmer = listen('taskbar-dimmer-changed', (event) => {
      applyTaskbarDimmerEvent(event.payload);
    });
    const unlistenBlack = listen('black-level-changed', (event) => {
      applyBlackLevelEvent(event.payload);
    });
    const unlistenBlackMain = listen('black-level-main-changed', (event) => {
      applyBlackLevelEvent(event.payload);
    });
    const unlistenSdr = listen('sdr-content-brightness-changed', (event) => {
      applySdrBrightnessEvent(event.payload);
    });
    const unlistenSdrMain = listen('sdr-content-brightness-main-changed', (event) => {
      applySdrBrightnessEvent(event.payload);
    });
    return () => {
      unlisten.then((stop) => stop());
      unlistenTags.then((stop) => stop());
      unlistenSort.then((stop) => stop());
      unlistenGroupBy.then((stop) => stop());
      unlistenFavorites.then((stop) => stop());
      unlistenDimmer.then((stop) => stop());
      unlistenBlack.then((stop) => stop());
      unlistenBlackMain.then((stop) => stop());
      unlistenSdr.then((stop) => stop());
      unlistenSdrMain.then((stop) => stop());
      try { hdrMq.removeEventListener('change', onTrayHdrChange); } catch {}
      clearTimeout(dimmerSaveTimer);
      clearTimeout(sdrApplyTimer);
    };
  });
</script>

<div
  class="tray-menu-wrap"
  class:tray-submenu-open={openSubmenu !== null}
  role="menu"
  tabindex="-1"
  onmouseenter={cancelScheduledSubmenuClose}
  onmouseleave={scheduleSubmenuClose}
>
  <div class="tray-menu">
    <div class="tray-menu-header">
      <img
        class="tray-menu-header-icon"
        src={appIcon}
        alt=""
        role="button"
        tabindex="0"
        aria-label="Open OLED Helper"
        title="Open OLED Helper"
        onclick={openMain}
        onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && openMain()}
      />
      <div class="tray-menu-header-copy">
        <div class="tray-menu-header-title-row">
          <div
            class="tray-menu-header-title"
            role="button"
            tabindex="0"
            aria-label="Open OLED Helper"
            title="Open OLED Helper"
            onclick={openMain}
            onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && openMain()}
          >OLED HELPER</div>
          <button
            type="button"
            class="hdr-switch"
            class:hdr={trayHdrStatus === 'hdr'}
            disabled={windowsHdrBusy || trayHdrStatus === 'unknown'}
            aria-label="Windows HDR"
            title={windowsHdrError || (trayHdrStatus === 'unknown' ? 'HDR status unknown' : `Switch to ${trayHdrStatus === 'hdr' ? 'SDR' : 'HDR'}`)}
            onclick={toggleTrayHdr}
          >
            <span class="hdr-switch-opt" class:active={trayHdrStatus !== 'hdr'}>SDR</span>
            <span class="hdr-switch-opt" class:active={trayHdrStatus === 'hdr'}>HDR</span>
            <svg class="hdr-switch-arrow hdr-switch-arrow-right" viewBox="0 0 10 10" fill="none" aria-hidden="true"><path d="M3 2L7 5L3 8" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>
            <svg class="hdr-switch-arrow hdr-switch-arrow-left" viewBox="0 0 10 10" fill="none" aria-hidden="true"><path d="M7 2L3 5L7 8" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>
          </button>
        </div>
        <div class="tray-menu-header-bottom-row">
          <div class="tray-menu-header-monitor">{appVersion ? `v${appVersion}` : ''}</div>
        </div>
      </div>
    </div>

    <div
      class="tray-item"
      class:open={openSubmenu === 'resolution'}
      role="menuitem"
      tabindex="0"
      onmouseenter={() => openSubmenuFor('resolution')}
    >
      <span class="tray-item-label">Resolution</span>
      {#if display}<span class="tray-item-value">({display.width}×{display.height})</span>{/if}
      <span class="tray-item-chevron">›</span>
    </div>

    <div
      class="tray-item"
      class:open={openSubmenu === 'refresh'}
      role="menuitem"
      tabindex="0"
      onmouseenter={() => openSubmenuFor('refresh')}
    >
      <span class="tray-item-label">Refresh Rate</span>
      {#if display}<span class="tray-item-value">({display.refresh_hz} Hz)</span>{/if}
      <span class="tray-item-chevron">›</span>
    </div>

    {#if favorites.length > 0}
      <div class="tray-sep tray-favorites-sep"></div>
    {/if}

    {#if favorites.length > 3}
      <div
        class="tray-item"
        class:open={openSubmenu === 'favorites'}
        role="menuitem"
        tabindex="0"
        onmouseenter={() => openSubmenuFor('favorites')}
      >
        <span class="tray-item-label">Favorites</span>
        <span class="tray-item-chevron">›</span>
      </div>
    {:else}
      {#each favorites as fav (favoriteMatchKey(fav))}
        {@const current = isCurrentFavorite(fav)}
        {@const confirming = pendingFavoriteRemove !== null && favoriteMatchKey(pendingFavoriteRemove) === favoriteMatchKey(fav)}
        {#if confirming}
          <div class="tray-item tray-favorite-item tray-favorite-inline-confirm" role="group" aria-label={`Remove ${favoriteLabel(fav)} from favorites?`}>
            <span class="tray-item-label tray-favorite-inline-confirm-label">Remove {favoriteLabel(fav)}?</span>
            <div class="tray-favorite-inline-confirm-actions">
              <button
                type="button"
                class="tray-favorite-inline-confirm-btn tray-favorite-inline-confirm-yes"
                aria-label={`Confirm remove ${favoriteLabel(fav)}`}
                title="Remove"
                onclick={(event) => { event.stopPropagation(); confirmFavoriteRemove(); }}
              >
                <svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M3 8.5l3 3 7-7" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
              </button>
              <button
                type="button"
                class="tray-favorite-inline-confirm-btn tray-favorite-inline-confirm-no"
                aria-label={`Cancel remove ${favoriteLabel(fav)}`}
                title="Cancel"
                onclick={(event) => { event.stopPropagation(); cancelFavoriteRemove(); }}
              >
                <svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M4 4l8 8M12 4l-8 8" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
              </button>
            </div>
          </div>
        {:else}
        <div
          class="tray-item tray-favorite-item"
          class:tray-favorite-current={current}
          role="menuitem"
          tabindex={current ? -1 : 0}
          onmouseenter={closeSubmenu}
          onclick={() => !current && pickFavorite(fav)}
          onkeydown={(e) => !current && (e.key === 'Enter' || e.key === ' ') && pickFavorite(fav)}
        >
          <span class="tray-item-check">{current ? '✓' : ''}</span>
          <span class="tray-item-label">{favoriteLabel(fav)}</span>
          <button
            type="button"
            class="tray-favorite-star"
            class:filled={true}
            aria-label={`Remove ${favoriteLabel(fav)} from favorites`}
            title={`Remove ${favoriteLabel(fav)} from favorites`}
            onclick={(event) => { event.stopPropagation(); requestFavoriteRemove(fav); }}
            onkeydown={(event) => event.stopPropagation()}
          >
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
              <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill="currentColor" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
            </svg>
          </button>
        </div>
        {/if}
      {/each}
    {/if}

    <div class="tray-sep"></div>

    <div class="tray-quick-row tray-black-row" role="group" aria-label="Black Level" onmouseenter={closeSubmenu}>
      <button
        type="button"
        class="tray-quick-toggle"
        class:enabled={trayBlack.enabled}
        aria-label={trayBlack.enabled ? 'Disable Black Level' : 'Enable Black Level'}
        aria-pressed={trayBlack.enabled}
        title={trayBlack.enabled ? 'Disable Black Level' : 'Enable Black Level'}
        onclick={toggleTrayBlackLevel}
      >
        <img src={tabCrushIcon} alt="" aria-hidden="true" />
      </button>
      <div class="tray-quick-slider-wrap">          <input
            class="tray-quick-slider"
            type="range"
            min="-150"
            max="150"
            step="1"
            value={trayBlack.adjustment}
            style="--fill:{Math.max(0, Math.min(100, ((trayBlack.adjustment + 150) / 300) * 100))}%"
            disabled={!trayBlack.enabled}
            aria-label="Black Level"
            title={trayBlack.enabled ? `Black Level ${formatTrayBlackLevel()}` : 'Enable Black Level to adjust'}
            oninput={handleTrayBlackLevelInput}
            onwheel={handleTrayBlackWheel}
            onkeydown={handleTrayBlackKeydown}
            onclick={(event) => event.stopPropagation()}
          />
        <span class="tray-quick-value">{formatTrayBlackLevel()}</span>
      </div>
    </div>

    <div class="tray-dimmer-row" class:enabled={taskbarDimmer.enabled} class:unavailable={!taskbarDimmer.available} role="group" aria-label="Taskbar Dimmer" onmouseenter={closeSubmenu}>
      <button
        type="button"
        class="tray-dimmer-toggle"
        class:enabled={taskbarDimmer.enabled}
        disabled={!taskbarDimmer.available}
        aria-label={taskbarDimmer.enabled ? 'Disable Taskbar Dimmer' : 'Enable Taskbar Dimmer'}
        aria-pressed={taskbarDimmer.enabled}
        title={taskbarDimmer.enabled ? 'Disable Taskbar Dimmer' : 'Enable Taskbar Dimmer'}
        onclick={toggleTaskbarDimmer}
      >
        <img src={tabDimmerIcon} alt="" aria-hidden="true" />
      </button>
      <div class="tray-dimmer-slider-wrap">
        <input
          class="tray-dimmer-slider"
          type="range"
          min="0"
          max="100"
          step="1"
          value={Math.round(taskbarDimmer.opacity * 100)}
          style="--fill:{taskbarDimmer.opacity * 100}%"
          disabled={!taskbarDimmer.available || !taskbarDimmer.enabled}
          aria-label="Taskbar Dimmer opacity"
          title={taskbarDimmer.enabled ? `Taskbar Dimmer opacity ${Math.round(taskbarDimmer.opacity * 100)}%` : 'Enable Taskbar Dimmer to adjust opacity'}
          oninput={handleTaskbarDimmerInput}
          onclick={(event) => event.stopPropagation()}
        />
        <span class="tray-dimmer-value">{Math.round(taskbarDimmer.opacity * 100)}%</span>
      </div>
    </div>

    <div class="tray-dimmer-row" class:enabled={screenDimmer.enabled} role="group" aria-label="Screen Dimmer" onmouseenter={closeSubmenu}>
      {#if !trayElevated && screenDimmer.enabled}
        <button
          type="button"
          class="tray-screen-dimmer-warn"
          aria-label="Screen Dimmer needs administrator rights to run"
          title="The Screen Dimmer needs administrator rights for power state detection. Relaunch OLED Helper elevated to run it."
          onclick={relaunchScreenDimmerElevated}
        >⚠</button>
      {/if}
      <button
        type="button"
        class="tray-dimmer-toggle"
        class:enabled={screenDimmer.enabled}
        aria-label={screenDimmer.enabled ? 'Disable Screen Dimmer' : 'Enable Screen Dimmer'}
        aria-pressed={screenDimmer.enabled}
        title={screenDimmer.enabled ? 'Disable Screen Dimmer' : 'Enable Screen Dimmer'}
        onclick={toggleScreenDimmer}
      >
        <img src={tabScreenDimmerIcon} alt="" aria-hidden="true" />
      </button>
      <div class="tray-dimmer-slider-wrap">
        <input
          class="tray-dimmer-slider"
          type="range"
          min="5"
          max="180"
          step="1"
          value={screenDimmer.seconds}
          style="--fill:{((screenDimmer.seconds - 5) / 175) * 100}%"
          disabled={!screenDimmer.enabled}
          aria-label="Screen Dimmer timer"
          title={screenDimmer.enabled ? `Screen Dimmer timer ${screenDimmer.seconds} sec` : 'Enable Screen Dimmer to adjust the timer'}
          oninput={handleScreenDimmerInput}
          onclick={(event) => event.stopPropagation()}
        />
        <span class="tray-dimmer-value">{screenDimmer.seconds} sec</span>
      </div>
    </div>

    {#if trayHdrStatus === 'hdr'}
      <div class="tray-quick-row" class:unavailable={!traySdrBrightness.available} role="group" aria-label="Windows SDR Content Brightness" onmouseenter={closeSubmenu}>
        <span class="tray-quick-icon" title="Windows SDR Content Brightness">
          <img src={tabHdrIcon} alt="" aria-hidden="true" />
        </span>
        <div class="tray-quick-slider-wrap">
          <input
            class="tray-quick-slider"
            type="range"
            min="80"
            max="480"
            step="1"
            value={traySdrBrightness.nits ?? 80}
            style="--fill:{(((traySdrBrightness.nits ?? 80) - 80) / 400) * 100}%"
            disabled={!traySdrBrightness.available}
            aria-label="Windows SDR Content Brightness"
            title={traySdrBrightness.available ? `SDR Content Brightness ${Math.round(traySdrBrightness.nits)} nits` : 'SDR Content Brightness unavailable'}
            oninput={handleTraySdrBrightnessInput}
            onclick={(event) => event.stopPropagation()}
          />
          <span class="tray-quick-value tray-sdr-value">{traySdrBrightness.available ? `${Math.round(traySdrBrightness.nits)} nits` : '-'}</span>
        </div>
      </div>
    {/if}

    <div class="tray-sep"></div>

    <div class="tray-hdr-mode-row" role="group" aria-label="SDR to HDR mode" onmouseenter={closeSubmenu}>
      <div class="tray-hdr-mode-options">
        {#each (trayHdrIsNvidia ? ['off', 'auto', 'rtx'] : ['off', 'auto']) as mode (mode)}
          <button type="button" class="tray-hdr-mode-opt" class:active={trayHdrMode === mode}
            disabled={trayHdrModeBusy || trayHdrMode === null}
            aria-pressed={trayHdrMode === mode}
            title={trayHdrModeError || undefined}
            onclick={() => selectTrayHdrMode(mode)}>{mode === 'off' ? 'Off' : mode === 'auto' ? 'Auto HDR' : 'RTX HDR'}</button>
        {/each}
      </div>
    </div>

    <div class="tray-sep"></div>

    <div class="tray-restart-exit-row" role="group" aria-label="Restart or Exit">
      <div class="tray-item tray-restart-item" role="menuitem" tabindex="0" aria-label="Restart" title="Restart" onmouseenter={closeSubmenu} onclick={restart} onkeydown={(e) => e.key === 'Enter' && restart()}>
        <svg class="tray-item-icon" width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path d="M13.2 8a5.2 5.2 0 1 1-1.5-3.7" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>
          <path d="M13.2 2.8v2.4h-2.4" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>
        </svg>
      </div>
      <div class="tray-item tray-exit-item" role="menuitem" tabindex="0" onmouseenter={closeSubmenu} onclick={quit} onkeydown={(e) => e.key === 'Enter' && quit()}>
        <svg class="tray-item-icon" width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path d="M8 1.5v6" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>
          <path d="M4.7 3.7a6 6 0 1 0 6.6 0" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>
        </svg>
        <span class="tray-item-label">Exit</span>
      </div>
    </div>
  </div>

  {#if openSubmenu === 'resolution' && display}
    <div class="tray-submenu tray-resolution-submenu" role="menu" tabindex="-1" style="margin-bottom: {submenuLift}px">
      {#if resolutionGroupBy === 'aspectRatio'}
        {#each groupedResolutions() as group}
          <div class="tray-submenu-group-label">{group.label}</div>
          {#each group.list as res (res.width + 'x' + res.height)}
            {@const checked = res.width === display.width && res.height === display.height}
            {@const name = resolutionName(res)}
            {@const ultrawide = isUltrawide(res.width, res.height)}
            {@const recommended = isRecommendedResolution(res)}
            <div
              class="tray-item"
              class:tray-current={checked}
              role="menuitemradio"
              aria-checked={checked}
              aria-disabled={checked}
              tabindex={checked ? -1 : 0}
              onclick={() => !checked && pickResolution(res)}
              onkeydown={(e) => !checked && (e.key === 'Enter' || e.key === ' ') && pickResolution(res)}
            >
              <span class="tray-item-check">{checked ? '✓' : ''}</span>
              <span class="tray-item-label tray-resolution-label">
                <span>{res.width}×{res.height}</span>
                {#if showResolutionTags && ultrawide}<span class="tray-ultrawide-tag"><span class="tray-ultrawide-separator">·</span><svg class="tray-ultrawide-icon" viewBox="0 0 32 16" fill="none" aria-label="Ultrawide"><rect x="0.9" y="0.9" width="30.2" height="14.2" rx="2" stroke="currentColor" stroke-width="1.1"/><text x="16" y="10.8" text-anchor="middle" fill="currentColor" font-size="9" font-family="Inter, sans-serif" font-weight="700" letter-spacing="0.2">UW</text></svg></span>{/if}
                {#if showResolutionTags && name}<span class="tray-resolution-tag"> · {name}</span>{/if}
                {#if recommended}<span class="tray-resolution-native">(Native)</span>{/if}
              </span>
              <button
                type="button"
                class="tray-favorite-star"
                class:filled={isFavoriteMode(res.width, res.height, display.refresh_hz)}
                aria-label={isFavoriteMode(res.width, res.height, display.refresh_hz) ? `Remove ${res.width}×${res.height} @ ${display.refresh_hz} Hz from favorites` : `Favorite ${res.width}×${res.height} @ ${display.refresh_hz} Hz`}
                title={isFavoriteMode(res.width, res.height, display.refresh_hz) ? 'Remove from favorites' : 'Add to favorites'}
                onclick={(event) => { event.stopPropagation(); toggleFavoriteMode(res.width, res.height, display.refresh_hz); }}
                onkeydown={(event) => event.stopPropagation()}
              >
                <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                  <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill={isFavoriteMode(res.width, res.height, display.refresh_hz) ? 'currentColor' : 'none'} stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
                </svg>
              </button>
            </div>
          {/each}
        {/each}
      {:else}
        {#each sortedResolutions() as res (res.width + 'x' + res.height)}
          {@const checked = res.width === display.width && res.height === display.height}
          {@const name = resolutionName(res)}
          {@const ultrawide = isUltrawide(res.width, res.height)}
          {@const recommended = isRecommendedResolution(res)}
          <div
            class="tray-item"
            class:tray-current={checked}
            role="menuitemradio"
            aria-checked={checked}
            aria-disabled={checked}
            tabindex={checked ? -1 : 0}
            onclick={() => !checked && pickResolution(res)}
            onkeydown={(e) => !checked && (e.key === 'Enter' || e.key === ' ') && pickResolution(res)}
          >
            <span class="tray-item-check">{checked ? '✓' : ''}</span>
            <span class="tray-item-label tray-resolution-label">
              <span>{res.width}×{res.height}</span>
              {#if showResolutionTags && ultrawide}<span class="tray-ultrawide-tag"><span class="tray-ultrawide-separator">·</span><svg class="tray-ultrawide-icon" viewBox="0 0 32 16" fill="none" aria-label="Ultrawide"><rect x="0.9" y="0.9" width="30.2" height="14.2" rx="2" stroke="currentColor" stroke-width="1.1"/><text x="16" y="10.8" text-anchor="middle" fill="currentColor" font-size="9" font-family="Inter, sans-serif" font-weight="700" letter-spacing="0.2">UW</text></svg></span>{/if}
              {#if showResolutionTags && name}<span class="tray-resolution-tag"> · {name}</span>{/if}
              {#if recommended}<span class="tray-resolution-native">(Native)</span>{/if}
            </span>
            <button
              type="button"
              class="tray-favorite-star"
              class:filled={isFavoriteMode(res.width, res.height, display.refresh_hz)}
              aria-label={isFavoriteMode(res.width, res.height, display.refresh_hz) ? `Remove ${res.width}×${res.height} @ ${display.refresh_hz} Hz from favorites` : `Favorite ${res.width}×${res.height} @ ${display.refresh_hz} Hz`}
              title={isFavoriteMode(res.width, res.height, display.refresh_hz) ? 'Remove from favorites' : 'Add to favorites'}
              onclick={(event) => { event.stopPropagation(); toggleFavoriteMode(res.width, res.height, display.refresh_hz); }}
              onkeydown={(event) => event.stopPropagation()}
            >
              <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill={isFavoriteMode(res.width, res.height, display.refresh_hz) ? 'currentColor' : 'none'} stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
              </svg>
            </button>
          </div>
        {/each}
      {/if}
    </div>
  {/if}

  {#if openSubmenu === 'refresh' && display}
    <div class="tray-submenu tray-refresh-submenu" role="menu" tabindex="-1" style="margin-bottom: {submenuLift}px">
      {#each sortedRefreshRates() as hz (hz)}
        {@const checked = hz === display.refresh_hz}
        {@const blackLevel = formatTrayBlackLevelForRefreshRate(hz)}
        <div
          class="tray-item"
          class:tray-current={checked}
          role="menuitemradio"
          aria-checked={checked}
          aria-disabled={checked}
          tabindex={checked ? -1 : 0}
          onclick={() => !checked && pickRefreshRate(hz)}
          onkeydown={(e) => !checked && (e.key === 'Enter' || e.key === ' ') && pickRefreshRate(hz)}
        >
          <span class="tray-item-check">{checked ? '✓' : ''}</span>
          <span class="tray-item-label">{hz} Hz</span>
          {#if blackLevel}<span class="tray-refresh-black-level">({blackLevel})</span>{/if}
        </div>
      {/each}
    </div>
  {/if}

  {#if openSubmenu === 'favorites' && favorites.length > 3}
    <div class="tray-submenu tray-favorites-submenu" role="menu" tabindex="-1" style="margin-bottom: {submenuLift}px">
      {#each favorites as fav (favoriteMatchKey(fav))}
        {@const current = isCurrentFavorite(fav)}
        {@const confirming = pendingFavoriteRemove !== null && favoriteMatchKey(pendingFavoriteRemove) === favoriteMatchKey(fav)}
        {#if confirming}
          <div class="tray-item tray-favorite-item tray-favorite-inline-confirm" role="group" aria-label={`Remove ${favoriteLabel(fav)} from favorites?`}>
            <span class="tray-item-label tray-favorite-inline-confirm-label">Remove {favoriteLabel(fav)}?</span>
            <div class="tray-favorite-inline-confirm-actions">
              <button
                type="button"
                class="tray-favorite-inline-confirm-btn tray-favorite-inline-confirm-yes"
                aria-label={`Confirm remove ${favoriteLabel(fav)}`}
                title="Remove"
                onclick={(event) => { event.stopPropagation(); confirmFavoriteRemove(); }}
              >
                <svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M3 8.5l3 3 7-7" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
              </button>
              <button
                type="button"
                class="tray-favorite-inline-confirm-btn tray-favorite-inline-confirm-no"
                aria-label={`Cancel remove ${favoriteLabel(fav)}`}
                title="Cancel"
                onclick={(event) => { event.stopPropagation(); cancelFavoriteRemove(); }}
              >
                <svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M4 4l8 8M12 4l-8 8" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
              </button>
            </div>
          </div>
        {:else}
        <div
          class="tray-item tray-favorite-item"
          class:tray-favorite-current={current}
          role="menuitem"
          tabindex={current ? -1 : 0}
          onclick={() => !current && pickFavorite(fav)}
          onkeydown={(e) => !current && (e.key === 'Enter' || e.key === ' ') && pickFavorite(fav)}
        >
          <span class="tray-item-check">{current ? '✓' : ''}</span>
          <span class="tray-item-label">{favoriteLabel(fav)}</span>
          <button
            type="button"
            class="tray-favorite-star"
            class:filled={true}
            aria-label={`Remove ${favoriteLabel(fav)} from favorites`}
            title={`Remove ${favoriteLabel(fav)} from favorites`}
            onclick={(event) => { event.stopPropagation(); requestFavoriteRemove(fav); }}
            onkeydown={(event) => event.stopPropagation()}
          >
            <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
              <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill="currentColor" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
            </svg>
          </button>
        </div>
        {/if}
      {/each}
    </div>
  {/if}
</div>
