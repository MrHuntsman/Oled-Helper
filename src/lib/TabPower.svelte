<script>
  // Power tab: display/sleep timeouts, the screen saver, and the screen
  // protector (dim the display after a configurable idle timeout). Values are
  // read live from Windows via
  // power_get_state and written through the power_set_* commands; the OS
  // (not the app config) persists the Power-tab settings. Screen Dimmer owns
  // its separate persisted settings in the dedicated tab.
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import TabPowerIcon from './TabPowerIcon.svelte';

  let state = $state({
    displayTimeout: 10,
    sleepTimeout: 30,
    screensaverEnabled: false,
    screensaverTimeout: 10,
    screensaverPath: '',
    screensavers: [],
  });
  let loaded = $state(false);
  let error = $state('');
  // Tracks which slider the user is mid-drag on, so the periodic re-read of
  // Windows settings never stomps a value they are actively changing.
  let dragging = $state({ display: false, sleep: false, screensaver: false });

  // The current path may be a third-party .scr that lives outside the
  // enumerated system dirs; surface it as its own option so the dropdown never
  // silently overwrites it with "Blank screen". The $derived must use the
  // expression form (plain function call), NOT $derived(() => {...}), that
  // form makes the derived value the function object itself.
  // First option is "Disabled" (off state, empty path); the rest are the
  // installed .scr files. The current path may be a third-party .scr that
  // lives outside the enumerated system dirs, surface it as its own option
  // right after "Disabled" so the dropdown never silently drops it.
  function computeScreensaverOptions() {
    const cur = state.screensaverPath;
    const list = [{ name: 'Disabled', path: '' }, ...state.screensavers];
    if (cur && !list.some((s) => s.path === cur)) {
      const name = cur.split(/[\\/]/).pop()?.replace(/\.scr$/i, '') || cur;
      list.splice(1, 0, { name, path: cur });
    }
    return list;
  }
  const screensaverOptions = $derived(computeScreensaverOptions());

  function fmtMinutes(minutes) {
    return minutes === 0 ? 'Never' : `${minutes} min`;
  }

  // ── Screensaver dropdown ───────────────────────────────────────────
  function selectScreensaver(path) {
    if (path === '') {
      // The "Disabled" option: turn the screen saver off.
      state.screensaverEnabled = false;
      state.screensaverPath = '';
    } else {
      state.screensaverEnabled = true;
      state.screensaverPath = path;
    }
    applyScreensaver();
  }

  function selectNativeScreensaver(event) {
    selectScreensaver(event.currentTarget.value);
  }

  function mergeState(s) {
    if (!dragging.display) state.displayTimeout = s.display_timeout_minutes;
    if (!dragging.sleep) state.sleepTimeout = s.sleep_timeout_minutes;
    state.screensaverEnabled = s.screensaver_enabled;
    if (!dragging.screensaver) {
      state.screensaverTimeout = Math.max(1, s.screensaver_timeout_minutes);
    }
    state.screensavers = s.screensavers;
    // An empty path means Windows has no .scr configured, so default to the
    // first installed one rather than leaving the dropdown empty (an empty
    // SCRNSAVE.EXE makes Windows treat the screen saver as not configured).
    state.screensaverPath = s.screensaver_path || (s.screensavers.length ? s.screensavers[0].path : '');
  }

  async function refreshState() {
    try {
      const s = await invoke('power_get_state');
      mergeState(s);
      error = '';
    } catch (e) {
      error = String(e);
      console.warn('[power] get_state failed', e);
    } finally {
      loaded = true;
    }
  }

  // Silent re-read so changes made in Windows Settings reflect here without
  // remounting the tab. Failures are ignored (they surface on the next user
  // interaction instead of flashing an error every poll).
  async function pollState() {
    try {
      const s = await invoke('power_get_state');
      mergeState(s);
    } catch (e) {
      console.warn('[power] poll failed', e);
    }
  }

  async function setDisplayTimeout() {
    try {
      await invoke('power_set_display_timeout', { minutes: state.displayTimeout });
      error = '';
    } catch (e) {
      error = String(e);
    }
  }

  async function setSleepTimeout() {
    try {
      await invoke('power_set_sleep_timeout', { minutes: state.sleepTimeout });
      error = '';
    } catch (e) {
      error = String(e);
    }
  }

  async function applyScreensaver() {
    try {
      await invoke('power_set_screensaver', {
        enabled: state.screensaverEnabled,
        timeoutMinutes: state.screensaverTimeout,
        path: state.screensaverPath,
      });
      error = '';
    } catch (e) {
      error = String(e);
    }
  }

  onMount(() => {
    refreshState();
    const poll = setInterval(pollState, 3000);
    return () => clearInterval(poll);
  });
</script>

<div class="tab-body">
  <div class="tab-heading tab-heading-2line">
    <div class="tab-heading-toggle no-hover">
      <TabPowerIcon class="tab-icon" />
      <div class="tab-heading-titles">
        <div class="tab-heading-title-row">
          <h2>Power Saving</h2>
          <span class="tab-monitor-tag">Global</span>
        </div>
        <span class="subtitle">Display &amp; screensaver power saving.</span>
      </div>
    </div>
  </div>
  <div class="separator"></div>

  {#if error}
    <p class="power-error" role="alert">⚠ {error}</p>
  {/if}

  {#if !loaded}
    <p class="power-loading">Reading Windows power settings…</p>
  {:else}
  <div class="dimmer-control">
    <div class="settings-section-heading">Display</div>
    <div class="dimmer-control-heading">
      <span class="settings-row-label">Turn off display after</span>
      <span class="slider-val">{fmtMinutes(state.displayTimeout)}</span>
    </div>
    <div class="slider-track-wrap dimmer-slider-track" role="presentation">
      <input type="range" min="0" max="60" step="1" bind:value={state.displayTimeout}
        style="--fill:{(state.displayTimeout / 60) * 100}%"
        onchange={setDisplayTimeout}
        onpointerdown={() => dragging.display = true}
        onpointerup={() => dragging.display = false}
        onpointercancel={() => dragging.display = false}
        aria-label="Turn off display after (minutes)" />
    </div>
    <div class="dimmer-scale"><span>Never</span><span>60 min</span></div>

    <div class="power-row-gap"></div>

    <div class="dimmer-control-heading">
      <span class="settings-row-label">Sleep after</span>
      <span class="slider-val">{fmtMinutes(state.sleepTimeout)}</span>
    </div>
    <div class="slider-track-wrap dimmer-slider-track" role="presentation">
      <input type="range" min="0" max="120" step="1" bind:value={state.sleepTimeout}
        style="--fill:{(state.sleepTimeout / 120) * 100}%"
        onchange={setSleepTimeout}
        onpointerdown={() => dragging.sleep = true}
        onpointerup={() => dragging.sleep = false}
        onpointercancel={() => dragging.sleep = false}
        aria-label="Sleep after (minutes)" />
    </div>
    <div class="dimmer-scale"><span>Never</span><span>120 min</span></div>

  </div>

  <div class="separator"></div>

  <div class="dimmer-control">
    <div class="settings-section-heading">Screensaver</div>

    <div class="power-screensaver-top-row">
      <select
        class="power-select-native"
        aria-label="Screensaver"
        value={state.screensaverEnabled ? state.screensaverPath : ''}
        onchange={selectNativeScreensaver}>
        {#each screensaverOptions as ss (ss.path)}
          <option value={ss.path}>{ss.name}</option>
        {/each}
      </select>
    </div>

    <div class="power-row-gap"></div>

    <div class="dimmer-control-heading">
      <span class="settings-row-label">Start after</span>
      <span class="slider-val">{state.screensaverTimeout} min</span>
    </div>
    <div class="slider-track-wrap dimmer-slider-track" role="presentation">
      <input type="range" min="1" max="60" step="1" bind:value={state.screensaverTimeout}
        style="--fill:{((state.screensaverTimeout - 1) / 59) * 100}%"
        onchange={applyScreensaver}
        onpointerdown={() => dragging.screensaver = true}
        onpointerup={() => dragging.screensaver = false}
        onpointercancel={() => dragging.screensaver = false}
        disabled={!state.screensaverEnabled}
        aria-label="Screensaver start after (minutes)" />
    </div>
    <div class="dimmer-scale"><span>1 min</span><span>60 min</span></div>
  </div>
  {/if}
</div>
