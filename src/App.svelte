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

  // Toggling real Windows HDR (Settings > Display > HDR) via the backend.
  // Detection already exists (app.hdrStatus, driven by pollHDRStatus's
  // matchMedia check below) — this just adds the write side.
  let windowsHdrBusy = $state(false);
  let windowsHdrError = $state('');

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
    const initial = setTimeout(() => { pollDisplayInfo(); pollHDRStatus(); }, 0);
    const timer = setInterval(() => { pollDisplayInfo(); pollHDRStatus(); }, 1000);
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
      <span class="display-info">{app.displayInfo.width}×{app.displayInfo.height}{app.displayInfo.refreshRateHz ? '@' + app.displayInfo.refreshRateHz + 'Hz' : ''}</span>
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
