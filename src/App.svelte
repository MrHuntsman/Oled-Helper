<script>
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { app, crush, pollDisplayInfo, pollHDRStatus, pollHDRLuminance, loadConfig, saveConfig, setDimmerSuspendedForFullscreen, applyBlackLevelProfileForRefreshRate, persistCurrentBlackLevelProfile, blackLevelDisplayForRefreshRate } from './lib/stores.svelte.js';
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

  // Custom click-only dropdown for resolution, replacing the native
  // <select> for the same reason as the Hz dropdown below (see comment
  // there). Mirrors its structure closely: aspect-ratio-grouped sections
  // instead of a flat list, but otherwise identical open/close/outside-click
  // behavior.
  let resMenuOpen = $state(false);
  let resMenuEl = $state(null);
  let resButtonEl = $state(null);

  function toggleResMenu() {
    if (app.displayModeBusy || app.displayInfo.resolutions.length === 0) return;
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
      if (resMenuEl?.contains(e.target) || resButtonEl?.contains(e.target)) return;
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
    if (app.refreshRateBusy || app.displayModeBusy || app.displayInfo.refreshRates.length === 0) return;
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
      if (hzMenuEl?.contains(e.target) || hzButtonEl?.contains(e.target)) return;
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

  // Toggling real Windows HDR (Settings > Display > HDR) via the backend.
  // Detection already exists (app.hdrStatus, driven by pollHDRStatus's
  // matchMedia check below) — this just adds the write side.
  let windowsHdrBusy = $state(false);
  let windowsHdrError = $state('');
  let refreshRateError = $state('');
  let resolutionError = $state('');

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

  // Flat list sorted largest-first, matching the plain (ungrouped) style
  // Windows' own Display Settings resolution dropdown uses — no aspect-ratio
  // sectioning. The display's native resolution comes from its EDID preferred
  // timing (queried in Rust, same data Windows uses for its "(Recommended)"
  // label); when the EDID can't be read the largest supported mode is used
  // as the closest available proxy.
  function sortedResolutions() {
    return [...app.displayInfo.resolutions].sort((a, b) => (b.width * b.height) - (a.width * a.height));
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
      || app.refreshRateBusy || (width === currentWidth && height === currentHeight)) return;

    app.displayModeBusy = true;
    app.displayInfo.width = width;
    app.displayInfo.height = height;
    resolutionError = '';
    try {
      await invoke('set_resolution', {
        width,
        height,
        preferredHz: app.displayInfo.refreshRateHz || 0,
      });
    } catch (e) {
      resolutionError = String(e);
    } finally {
      app.displayModeBusy = false;
      // Re-query the mode so the dropdown and available refresh rates reflect
      // the resolution Windows actually accepted.
      pollDisplayInfo();
    }
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
      await invoke('set_refresh_rate', { hz });
      // Apply immediately; the next display poll still confirms the actual
      // mode in case Windows selected a different mode or rejected the call.
      applyBlackLevelProfileForRefreshRate(hz);
      saveConfig();
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

  onMount(() => {
    // One-time: seed display info and HDR status. Mode changes re-poll
    // explicitly afterward — there is no periodic poll on the main thread.
    pollDisplayInfo();
    pollHDRStatus();

    let unlisten;
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
    return () => unlisten?.();
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
      <div class="resolution-wrap">
        <button type="button" class="resolution-select" class:active={resMenuOpen}
          bind:this={resButtonEl}
          aria-label="Resolution"
          aria-haspopup="listbox"
          aria-expanded={resMenuOpen}
          title={resolutionError || 'Choose resolution'}
          disabled={app.displayModeBusy || app.displayInfo.resolutions.length === 0}
          onclick={toggleResMenu}>
          {app.displayInfo.width && app.displayInfo.height ? `${app.displayInfo.width}×${app.displayInfo.height}` : '—×—'}{#if currentResolutionName()}<span class="refresh-rate-select-value"> · {currentResolutionName()}</span>{/if}
        </button>
        {#if resMenuOpen}
          <div class="hz-menu res-menu" bind:this={resMenuEl} role="listbox" aria-label="Resolution options">
            {#if app.displayInfo.resolutions.length === 0}
              <div class="hz-menu-item active">
                {app.displayInfo.width && app.displayInfo.height ? `${app.displayInfo.width}×${app.displayInfo.height}` : '—×—'}
              </div>
            {:else}
                {#each sortedResolutions() as resolution}
                  {@const name = resolutionName(resolution)}
                  {@const current = resolution.width === app.displayInfo.width && resolution.height === app.displayInfo.height}
                  {@const recommended = isRecommendedResolution(resolution)}
                  {@const ultrawide = isUltrawide(resolution.width, resolution.height)}
                  <button type="button" class="hz-menu-item" class:active={current}
                    role="option" aria-selected={current}
                    onclick={() => pickResolution(resolution.width, resolution.height)}>
                    {resolution.width}×{resolution.height}{#if ultrawide}<span class="ultrawide-tag"><span class="ultrawide-separator">·</span><svg class="ultrawide-icon" viewBox="0 0 24 16" fill="none" aria-label="Ultrawide"><rect x="0.9" y="0.9" width="22.2" height="14.2" rx="2" stroke="currentColor" stroke-width="1.1"/><text x="12" y="10.8" text-anchor="middle" fill="currentColor" font-size="8" font-family="Inter, sans-serif" font-weight="700" letter-spacing="0.2">UW</text></svg></span>{/if}{name ? ` · ${name}` : ''}{recommended ? ' (Recommended)' : ''}
                  </button>
                {/each}
            {/if}
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
          title={refreshRateError || 'Choose refresh rate'}
          disabled={app.refreshRateBusy || app.displayModeBusy || app.displayInfo.refreshRates.length === 0}
          onclick={toggleHzMenu}>
          {app.displayInfo.refreshRateHz ? `${app.displayInfo.refreshRateHz} Hz` : '— Hz'}{#if app.displayInfo.refreshRateHz && blackLevelDisplayForRefreshRate(app.displayInfo.refreshRateHz)}<span class="refresh-rate-select-value"> ({blackLevelDisplayForRefreshRate(app.displayInfo.refreshRateHz)})</span>{/if}
        </button>
        {#if hzMenuOpen}
          <div class="hz-menu" bind:this={hzMenuEl} role="listbox" aria-label="Refresh rate options">
            {#each [...app.displayInfo.refreshRates].sort((a, b) => b - a) as hz}
              <button type="button" class="hz-menu-item" class:active={hz === app.displayInfo.refreshRateHz}
                role="option" aria-selected={hz === app.displayInfo.refreshRateHz}
                onclick={() => pickHz(hz)}>
                <span>{hz} Hz</span>
                <span class="hz-profile-value">{blackLevelDisplayForRefreshRate(hz)}</span>
              </button>
            {/each}
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
</div>
