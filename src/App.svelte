<script>
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { app, crush, pollDisplayInfo, pollHDRStatus, bumpEnvTick, loadConfig, setDimmerSuspendedForFullscreen } from './lib/stores.svelte.js';
  import { invoke } from '@tauri-apps/api/core';
  import TabCrush from './lib/TabCrush.svelte';
  import TabDimmer from './lib/TabDimmer.svelte';
  import TabHotkeys from './lib/TabHotkeys.svelte';
  import TabDebug from './lib/TabDebug.svelte';
  import TabSettings from './lib/TabSettings.svelte';
  import TabAbout from './lib/TabAbout.svelte';

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
    ['1366x768', 'HD+'],
    ['1600x900', 'HD+'],
    ['1920x1080', 'Full HD'],
    ['1920x1200', 'WUXGA'],
    ['2560x1080', 'Ultrawide Full HD'],
    ['2560x1440', 'QHD'],
    ['3440x1440', 'UWQHD'],
    ['3840x2160', '4K UHD'],
    ['5120x1440', 'DQHD'],
    ['5120x2160', '5K Ultrawide'],
    ['5120x2880', '5K UHD'],
    ['7680x4320', '8K UHD'],
  ]);

  function resolutionName(resolution) {
    return resolutionNames.get(`${resolution.width}x${resolution.height}`) || '';
  }

  function resolutionAspectRatio(resolution) {
    const ratio = resolution.width / resolution.height;
    // Treat 21:9, 32:9, and intermediate cinematic ratios (for example
    // 2560×1080 and 5120×2160) as one navigable Ultrawide group.
    if (ratio >= 2.1) return 'Ultrawide';
    const commonRatios = [
      [16 / 9, '16:9'], [16 / 10, '16:10'], [4 / 3, '4:3'], [5 / 4, '5:4'],
    ];
    return commonRatios.find(([value]) => Math.abs(ratio - value) < 0.025)?.[1] || 'Other';
  }

  function resolutionGroups() {
    const groups = new Map();
    for (const resolution of app.displayInfo.resolutions) {
      const aspect = resolutionAspectRatio(resolution);
      if (!groups.has(aspect)) groups.set(aspect, []);
      groups.get(aspect).push(resolution);
    }
    return [...groups.entries()]
      .sort(([a], [b]) => a === 'Other' ? 1 : b === 'Other' ? -1 : a.localeCompare(b, undefined, { numeric: true }))
      .map(([aspect, resolutions]) => ({
        aspect,
        resolutions: resolutions.sort((a, b) => (a.width * a.height) - (b.width * b.height)),
      }));
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

    app.refreshRateBusy = true;
    app.displayInfo.refreshRateHz = hz;
    refreshRateError = '';
    try {
      await invoke('set_refresh_rate', { hz });
    } catch (e) {
      refreshRateError = String(e);
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
    windowsHdrBusy = true;
    try {
      await invoke('set_windows_hdr_enabled', { enabled: next });
      windowsHdrError = '';
    } catch (e) {
      windowsHdrError = String(e);
    } finally {
      windowsHdrBusy = false;
      // matchMedia picks up the real change on its own via the next poll tick;
      // no separate status call needed.
    }
  }

  onMount(() => {
    let unlisten;
    listen('window-visibility', (event) => {
      windowVisible = event.payload !== false;
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

  // Poll display info and HDR status every 1s.
  // NOTE: pollDisplayInfo() must NOT be called synchronously as the first
  // statement of this effect body — it writes app.displayInfo ($state), and
  // now also kicks off an async invoke().then() that writes it again. Calling
  // it synchronously inside the effect's own tracked scope is the exact
  // write-inside-effect pattern that trips Svelte's effect_update_depth_exceeded
  // loop guard (see the gpuLogVersion footgun comment in stores.svelte.js).
  // Deferring the first call with setTimeout(...,0) keeps it out of the
  // effect's synchronous execution while still firing immediately on mount.
  $effect(() => {
    if (!windowVisible) return;
    const pollDisplay = () => { if (!hzMenuOpen && !resMenuOpen) pollDisplayInfo(); };
    const initial = setTimeout(() => { pollDisplay(); pollHDRStatus(); }, 0);
    const timer = setInterval(() => { pollDisplay(); pollHDRStatus(); }, 1000);
    return () => { clearTimeout(initial); clearInterval(timer); };
  });

  // Separate timer for envTick (drives live-refresh of non-reactive globals
  // like navigator.*/screen.* in the Debug tab). Kept in its own effect,
  // deliberately NOT combined with the poll effect above — stacking multiple
  // $state writes (pollDisplayInfo's object reassignment + pollHDRStatus +
  // bumpEnvTick) inside one setInterval callback tripped Svelte's
  // effect_update_depth_exceeded loop guard, even though none of them read
  // back their own state. See stores.svelte.js footgun comment near envTick.
  $effect(() => {
    if (!windowVisible) return;
    const timer = setInterval(() => { bumpEnvTick(); }, 1000);
    return () => clearInterval(timer);
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
      <img class="app-icon" src="/icons/app-icon.png" alt="OLED Helper" />
      <div class="title-block">
        <span class="title">OLED HELPER</span>
        <span class="version">v0.1.0</span>
      </div>
    </div>
    <div class="spacer"></div>
    <button class="top-btn" class:active={app.activeTab === 'settings'} onclick={() => app.activeTab = 'settings'}>
      <img class="top-btn-icon" src="/icons/tab_system.png" alt="" /> Settings
    </button>
    <button class="top-btn" class:active={app.activeTab === 'about'} onclick={() => app.activeTab = 'about'}>
      <img class="top-btn-icon" src="/icons/tab_about.png" alt="" /> About
    </button>
  </div>

  <nav class="sidebar">
    <button class="nav-btn" class:active={app.activeTab === 'crush'} onclick={() => app.activeTab = 'crush'}>
      <img class="nav-icon" src="/icons/tab_crush.png" alt="" /> Black Crush Tweak
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'dimmer'} onclick={() => app.activeTab = 'dimmer'}>
      <img class="nav-icon" src="/icons/tab_dimmer.png" alt="" /> Taskbar Dimmer
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'hotkeys'} onclick={() => app.activeTab = 'hotkeys'}>
      <img class="nav-icon" src="/icons/tab_hotkeys.png" alt="" /> Hotkeys
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
          {app.displayInfo.width && app.displayInfo.height ? `${app.displayInfo.width}×${app.displayInfo.height}` : '—×—'}
        </button>
        {#if resMenuOpen}
          <div class="hz-menu res-menu" bind:this={resMenuEl} role="listbox" aria-label="Resolution options">
            {#if app.displayInfo.resolutions.length === 0}
              <div class="hz-menu-item active">
                {app.displayInfo.width && app.displayInfo.height ? `${app.displayInfo.width}×${app.displayInfo.height}` : '—×—'}
              </div>
            {:else}
              {#each resolutionGroups() as group}
                <div class="hz-menu-group-label">{group.aspect === 'Ultrawide' ? 'Ultrawide (21:9 / 32:9)' : `${group.aspect} resolutions`}</div>
                {#each group.resolutions as resolution}
                  {@const name = resolutionName(resolution)}
                  {@const current = resolution.width === app.displayInfo.width && resolution.height === app.displayInfo.height}
                  <button type="button" class="hz-menu-item" class:active={current}
                    role="option" aria-selected={current}
                    onclick={() => pickResolution(resolution.width, resolution.height)}>
                    {resolution.width}×{resolution.height}{name ? ` · ${name}` : ''}
                  </button>
                {/each}
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
          {app.displayInfo.refreshRateHz ? `${app.displayInfo.refreshRateHz} Hz` : '— Hz'}
        </button>
        {#if hzMenuOpen}
          <div class="hz-menu" bind:this={hzMenuEl} role="listbox" aria-label="Refresh rate options">
            {#each app.displayInfo.refreshRates as hz}
              <button type="button" class="hz-menu-item" class:active={hz === app.displayInfo.refreshRateHz}
                role="option" aria-selected={hz === app.displayInfo.refreshRateHz}
                onclick={() => pickHz(hz)}>
                {hz} Hz
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
        </button>
        <div class="hdr-switch-tooltip">
          {windowsHdrError || (app.hdrStatus === 'unknown' ? 'HDR status unknown' : `Switch to ${app.hdrStatus === 'hdr' ? 'SDR' : 'HDR'}`)}
        </div>
      </div>
    </div>
  </div>
</div>
