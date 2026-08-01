<script>
  import { app, pollDisplayInfo, pollHDRStatus, bumpEnvTick } from './lib/stores.svelte.js';
  import TabCrush from './lib/TabCrush.svelte';
  import TabDimmer from './lib/TabDimmer.svelte';
  import TabSystem from './lib/TabSystem.svelte';
  import TabHotkeys from './lib/TabHotkeys.svelte';
  import TabDebug from './lib/TabDebug.svelte';
  import TabSettings from './lib/TabSettings.svelte';
  import TabAbout from './lib/TabAbout.svelte';

  $effect(() => {
    document.documentElement.setAttribute('data-hdr', app.hdrStatus === 'hdr' ? 'true' : 'false');
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
    const timer = setInterval(() => { bumpEnvTick(); }, 1000);
    return () => clearInterval(timer);
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
      <img class="top-btn-icon" src="/icons/tab_settings.png" alt="" /> Settings
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
    <button class="nav-btn" class:active={app.activeTab === 'system'} onclick={() => app.activeTab = 'system'}>
      <img class="nav-icon" src="/icons/tab_system.png" alt="" /> System
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'hotkeys'} onclick={() => app.activeTab = 'hotkeys'}>
      <img class="nav-icon" src="/icons/tab_hotkeys.png" alt="" /> Hotkeys
    </button>
    <button class="nav-btn" class:active={app.activeTab === 'debug'} onclick={() => app.activeTab = 'debug'}>
      <img class="nav-icon" src="/icons/tab_debug.png" alt="" /> Debug
    </button>
  </nav>

  <main class="content">
    {#if app.activeTab === 'crush'}
      <TabCrush />
    {:else if app.activeTab === 'dimmer'}
      <TabDimmer />
    {:else if app.activeTab === 'system'}
      <TabSystem />
    {:else if app.activeTab === 'hotkeys'}
      <TabHotkeys />
    {:else if app.activeTab === 'debug'}
      <TabDebug />
    {:else if app.activeTab === 'settings'}
      <TabSettings />
    {:else if app.activeTab === 'about'}
      <TabAbout />
    {/if}
  </main>

  <div class="status-bar">
    <span>v0.1.0</span>
    <div class="status-bar-right">
      <span class="display-info">{app.displayInfo.width}×{app.displayInfo.height}{app.displayInfo.refreshRateHz ? '@' + app.displayInfo.refreshRateHz + 'Hz' : ''}</span>
      <span class="hdr-badge {app.hdrStatus}">{app.hdrStatus === 'hdr' ? 'HDR' : app.hdrStatus === 'sdr' ? 'SDR' : '--'}</span>
    </div>
  </div>
</div>
