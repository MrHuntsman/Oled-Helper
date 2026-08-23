<script>
  // Inlined data URI, see App.svelte for why icons never hit the network.
  import { invoke } from '@tauri-apps/api/core';
  import tabDimmerIcon from './icons/tab_dimmer.png?inline';
  import tabScreenDimmerIcon from './icons/tab_screen_dimmer.png?inline';
  import ShieldBadge from './ShieldBadge.svelte';
  import ScreenDimmerState from './ScreenDimmerState.svelte';
  import { app, taskbarDimmer, screenDimmer, saveConfig, setScreenDimmer, setScreenDimmerEnabled, setTaskbarDimmer, restoreTaskbarDimmerDefaults, SCREEN_DIMMER_DEFAULTS, activeMonitor, monitors, dimmerMonKey, syncScreenDimmerView } from './stores.svelte.js';

  // The heading tag showing which monitor this tab's settings apply to (the
  // one selected in the top-bar monitor selector), labeled like the selector.
  const dimmerMonitorTagLabel = $derived(
    activeMonitor.mon
      ? activeMonitor.mon.friendly_name || `Monitor ${monitors.indexOf(activeMonitor.mon) + 1}`
      : ''
  );

  let { screen = false } = $props();
  let screenError = $state('');
  let screenTimeoutDragging = $state(false);
  let screenOpacityDragging = $state(false);
  let screenOpacityHovering = $state(false);
  let screenFadeInDragging = $state(false);
  let screenFadeInHovering = $state(false);
  let screenFadeOutDragging = $state(false);
  let screenFadeOutHovering = $state(false);

  function screenFmtSeconds(seconds) {
    return `${seconds} sec`;
  }

  function applyScreenDimmer(persist = false) {
    screenDimmer.seconds = Math.max(5, Math.min(180, Math.round(Number(screenDimmer.seconds) || 30)));
    screenDimmer.opacity = Math.max(0, Math.min(1, Number(screenDimmer.opacity) || 0));
    screenDimmer.fadeInDurationMs = Math.max(0, Math.min(5000, Math.round(Number(screenDimmer.fadeInDurationMs) || 0)));
    screenDimmer.fadeOutDurationMs = Math.max(0, Math.min(5000, Math.round(Number(screenDimmer.fadeOutDurationMs) || 0)));
    // Writes the active monitor's profile (the sliders edit the selected
    // monitor) and pushes it to the backend; the master toggle is global.
    setScreenDimmer(
      screenDimmer.enabled,
      screenDimmer.seconds,
      screenDimmer.opacity,
      screenDimmer.fadeInDurationMs,
      screenDimmer.fadeOutDurationMs,
      screenDimmer.fullscreenDetection,
      persist
    ).catch((e) => {
      screenError = String(e);
      console.warn('[screen-dimmer] apply failed', e);
    });
  }

  async function refreshScreenDimmer() {
    try {
      // Per-monitor status read: the poll targets the currently selected
      // monitor, whose profile the sliders edit.
      const state = await invoke('power_get_screen_dimmer_status', { monitorKey: dimmerMonKey() });
      if (!screenTimeoutDragging) {
        screenDimmer.enabled = state.enabled === true;
        screenDimmer.seconds = state.seconds;
      }
      if (!screenOpacityDragging) screenDimmer.opacity = state.opacity;
      if (!screenFadeInDragging) screenDimmer.fadeInDurationMs = state.fade_in_duration_ms;
      if (!screenFadeOutDragging) screenDimmer.fadeOutDurationMs = state.fade_out_duration_ms;
      screenDimmer.fullscreenDetection = state.fullscreen_detection === true;
      screenError = '';
    } catch (e) {
      screenError = String(e);
      console.warn('[screen-dimmer] state read failed', e);
    }
  }

  function restoreScreenDimmerDefaults() {
    // Reset the ACTIVE monitor's profile to the canonical defaults. The
    // master toggle is global, and like the Taskbar Dimmer's restore it is
    // reset too (a full restore of the tab's defaults).
    screenDimmer.enabled = SCREEN_DIMMER_DEFAULTS.enabled;
    screenDimmer.seconds = SCREEN_DIMMER_DEFAULTS.seconds;
    screenDimmer.opacity = SCREEN_DIMMER_DEFAULTS.opacity;
    screenDimmer.fadeInDurationMs = SCREEN_DIMMER_DEFAULTS.fadeInDurationMs;
    screenDimmer.fadeOutDurationMs = SCREEN_DIMMER_DEFAULTS.fadeOutDurationMs;
    screenDimmer.fadeLinked = SCREEN_DIMMER_DEFAULTS.fadeLinked;
    screenDimmer.fullscreenDetection = SCREEN_DIMMER_DEFAULTS.fullscreenDetection;
    applyScreenDimmer(true);
  }

  function toggleFullscreenDetection() {
    screenDimmer.fullscreenDetection = !screenDimmer.fullscreenDetection;
    applyScreenDimmer(true);
  }

  async function toggleScreenDimmer() {
    try {
      await setScreenDimmerEnabled(!screenDimmer.enabled, app.activeTab);
      screenError = '';
    } catch (e) {
      screenError = String(e);
      console.warn('[screen-dimmer] toggle failed', e);
    }
  }

  // The dimmer only runs in an elevated session. If the user declined the
  // startup prompt (or launched without admin), this relaunches the app
  // elevated so the dimmer actually starts; a second decline keeps the
  // enabled state and the warning banner.
  async function restartScreenDimmerAsAdmin() {
    try {
      await invoke('relaunch_elevated', {
        rtxHdrFlags: null,
        rtxHdrEnabled: null,
        hdrMode: null,
        screenDimmerEnabled: true,
        tab: 'screen-dimmer',
      });
    } catch (e) {
      screenError = String(e);
      console.warn('[screen-dimmer] restart as admin failed', e);
    }
  }

  let screenDimmerMasterAttention = $state(false);
  let screenDimmerMasterAttentionTimer = null;
  let screenDimmerMasterAttentionRaf = 0;

  function flashScreenDimmerMasterSwitch() {
    screenDimmerMasterAttention = false;
    cancelAnimationFrame(screenDimmerMasterAttentionRaf);
    screenDimmerMasterAttentionRaf = requestAnimationFrame(() => { screenDimmerMasterAttention = true; });
    clearTimeout(screenDimmerMasterAttentionTimer);
    screenDimmerMasterAttentionTimer = setTimeout(() => { screenDimmerMasterAttention = false; }, 850);
  }

  function handleDisabledScreenDimmerInteraction(e) {
    if (screenDimmer.enabled) return;
    if (e.type === 'keydown' && !['Enter', ' ', 'ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(e.key)) return;
    e.preventDefault();
    e.stopPropagation();
    flashScreenDimmerMasterSwitch();
  }

  function handleDisabledScreenDimmerWheel(e) {
    if (!screenDimmer.enabled) {
      e.preventDefault();
      e.stopPropagation();
      flashScreenDimmerMasterSwitch();
    }
  }

  $effect(() => {
    if (screenDimmer.enabled) return;
    screenTimeoutDragging = false;
    screenOpacityDragging = false;
    screenOpacityHovering = false;
    screenFadeInDragging = false;
    screenFadeInHovering = false;
    screenFadeOutDragging = false;
    screenFadeOutHovering = false;
  });

  $effect(() => () => {
    clearTimeout(screenDimmerMasterAttentionTimer);
    cancelAnimationFrame(screenDimmerMasterAttentionRaf);
  });

  function toggleScreenFadeLinked() {
    screenDimmer.fadeLinked = !screenDimmer.fadeLinked;
    if (screenDimmer.fadeLinked) {
      screenDimmer.fadeOutDurationMs = screenDimmer.fadeInDurationMs;
    }
    applyScreenDimmer(true);
  }

  $effect(() => {
    if (!screen) return;
    refreshScreenDimmer();
    const poll = setInterval(refreshScreenDimmer, 3000);
    return () => clearInterval(poll);
  });

  // When the monitor selector changes while this tab is open, reload the
  // sliders from the newly selected monitor's profile before the next poll.
  // dimmerMonKey() reads activeMonitor.mon, so this re-runs on switch.
  $effect(() => {
    if (!screen) return;
    dimmerMonKey();
    syncScreenDimmerView();
  });

  $effect(() => {
    if (!screenTimeoutDragging && !screenOpacityDragging && !screenFadeInDragging && !screenFadeOutDragging) return;
    function onUp() {
      screenTimeoutDragging = false;
      screenOpacityDragging = false;
      screenFadeInDragging = false;
      screenFadeOutDragging = false;
    }
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pointercancel', onUp);
    return () => {
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
    };
  });

  function handleScreenTimeoutInput(event) {
    screenDimmer.seconds = Number(event.currentTarget.value);
    applyScreenDimmer(false);
  }

  function handleScreenOpacityInput(event) {
    screenDimmer.opacity = Number(event.currentTarget.value) / 100;
    applyScreenDimmer(false);
  }

  function handleScreenFadeInInput(event) {
    screenDimmer.fadeInDurationMs = Number(event.currentTarget.value);
    if (screenDimmer.fadeLinked) screenDimmer.fadeOutDurationMs = screenDimmer.fadeInDurationMs;
    applyScreenDimmer(false);
  }

  function handleScreenFadeOutInput(event) {
    screenDimmer.fadeOutDurationMs = Number(event.currentTarget.value);
    if (screenDimmer.fadeLinked) screenDimmer.fadeInDurationMs = screenDimmer.fadeOutDurationMs;
    applyScreenDimmer(false);
  }

  function toggleDimmer() {
    const next = !taskbarDimmer.enabled;
    if (!next) {
      opacityDragging = false;
      opacityHovering = false;
      fadeInDragging = false;
      fadeInHovering = false;
      fadeOutDragging = false;
      fadeOutHovering = false;
    }
    setTaskbarDimmer(next);
  }

  // Disabled settings should explain themselves. A short pulse on the master
  // switch gives the user a clear target without changing any setting.
  function flashDimmerMasterSwitch() {
    dimmerMasterAttention = false;
    cancelAnimationFrame(dimmerMasterAttentionRaf);
    dimmerMasterAttentionRaf = requestAnimationFrame(() => { dimmerMasterAttention = true; });
    clearTimeout(dimmerMasterAttentionTimer);
    dimmerMasterAttentionTimer = setTimeout(() => { dimmerMasterAttention = false; }, 850);
  }

  function handleDisabledDimmerInteraction(e) {
    if (!taskbarDimmer.available || taskbarDimmer.enabled) return;
    // Do not interfere with Tab navigation. Activation and arrow keys are
    // treated as attempts to use a disabled setting and trigger the cue.
    if (e.type === 'keydown' && !['Enter', ' ', 'ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(e.key)) return;
    e.preventDefault();
    e.stopPropagation();
    flashDimmerMasterSwitch();
  }

  function handleDisabledDimmerWheel(e) {
    if (taskbarDimmer.available && !taskbarDimmer.enabled) {
      e.preventDefault();
      e.stopPropagation();
      flashDimmerMasterSwitch();
    }
  }

  let opacityDragging = $state(false);
  let opacityHovering = $state(false);
  let dimmerMasterAttention = $state(false);
  let dimmerMasterAttentionTimer = null;
  let dimmerMasterAttentionRaf = 0;
  let fadeInDragging = $state(false);
  let fadeInHovering = $state(false);
  let fadeOutDragging = $state(false);
  let fadeOutHovering = $state(false);

  $effect(() => {
    if (screen || (!opacityDragging && !fadeInDragging && !fadeOutDragging)) return;
    function onUp() { opacityDragging = false; fadeInDragging = false; fadeOutDragging = false; }
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pointercancel', onUp);
    return () => {
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
    };
  });

  $effect(() => {
    if (taskbarDimmer.enabled) return;
    // Covers external disable actions (hotkey/backend availability), not just
    // clicking this tab's master switch. Never leave a stale tooltip visible.
    opacityDragging = false;
    opacityHovering = false;
    fadeInDragging = false;
    fadeInHovering = false;
    fadeOutDragging = false;
    fadeOutHovering = false;
  });

  $effect(() => () => {
    clearTimeout(dimmerMasterAttentionTimer);
    cancelAnimationFrame(dimmerMasterAttentionRaf);
  });

  // Integer slider values map 1:1 to distinct alpha levels in the Rust
  // curve (100%→255, 99%→254, … 50%→205; below 50% the curve skips levels
  // to stay increasing), so every displayed percentage is unique, no two
  // slider positions repeat a readout. Snap to the nearest tick (every 10,
  // 10-90) when the raw value lands within a few points of one, mirroring
  // the Black Level slider's snap-to-zero (normalizeBlackLevel in
  // TabCrush.svelte). 0 and 100 aren't included since the track's own ends
  // already act as a natural stop there.
  const TICKS = [10, 20, 30, 40, 50, 60, 70, 80, 90];
  const SNAP_TOLERANCE = 1;
  function normalizeOpacityValue(value) {
    for (const tick of TICKS) {
      if (Math.abs(value - tick) <= SNAP_TOLERANCE) return tick;
    }
    return value;
  }

  function handleOpacityInput(event) {
    const raw = +event.currentTarget.value;
    // Snap to nearby ticks only while actually dragging with the pointer.
    // Keyboard arrows don't fire pointerdown, so they bypass the snap;
    // otherwise pressing an arrow from a snapped position (e.g. 50) lands on
    // 51, which is inside the tolerance band, and gets pulled straight back.
    const normalized = opacityDragging ? normalizeOpacityValue(raw) : raw;
    if (raw !== normalized) event.currentTarget.value = normalized;
    setTaskbarDimmer(taskbarDimmer.enabled, normalized / 100, taskbarDimmer.fadeInDurationMs, taskbarDimmer.fadeOutDurationMs, true, true);
  }

  function handleFadeInInput(event) {
    const duration = Math.max(0, Math.min(1000, +event.currentTarget.value));
    const fadeOut = taskbarDimmer.fadeLinked ? duration : taskbarDimmer.fadeOutDurationMs;
    setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, duration, fadeOut, false);
  }

  function commitFadeIn(event) {
    const duration = Math.max(0, Math.min(1000, +event.currentTarget.value));
    const fadeOut = taskbarDimmer.fadeLinked ? duration : taskbarDimmer.fadeOutDurationMs;
    setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, duration, fadeOut);
  }

  function handleFadeOutInput(event) {
    const duration = Math.max(0, Math.min(1000, +event.currentTarget.value));
    const fadeIn = taskbarDimmer.fadeLinked ? duration : taskbarDimmer.fadeInDurationMs;
    setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, fadeIn, duration, false);
  }

  function commitFadeOut(event) {
    const duration = Math.max(0, Math.min(1000, +event.currentTarget.value));
    const fadeIn = taskbarDimmer.fadeLinked ? duration : taskbarDimmer.fadeInDurationMs;
    setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, fadeIn, duration);
  }

  // Toggling the chain link on immediately syncs fade-out to match fade-in,
  // so the two sliders visually agree the moment they're linked rather than
  // waiting for the next drag.
  //
  // The cursor is still physically over the button right after a click, so
  // :hover immediately re-applies, but now against the OTHER state's hover
  // rule, causing a visible extra jump right after the toggle's own
  // transition plays. chainSuppressHover pins the hooks back to their plain
  // rest-state transform (see .just-toggled in app.css) until the pointer
  // actually leaves the button, no guessed timeout, driven by the real
  // pointerleave event so it can never desync from what the cursor is doing.
  let chainSuppressHover = $state(false);
  function toggleFadeLinked() {
    taskbarDimmer.fadeLinked = !taskbarDimmer.fadeLinked;
    if (taskbarDimmer.fadeLinked && taskbarDimmer.fadeOutDurationMs !== taskbarDimmer.fadeInDurationMs) {
      setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, taskbarDimmer.fadeInDurationMs, taskbarDimmer.fadeInDurationMs);
    } else {
      // Unlinking (or linking with already-equal values) doesn't change the
      // dimmer values, but the link state itself is persisted.
      saveConfig();
    }
    chainSuppressHover = true;
  }
</script>

{#if screen}
  <div class="tab-body">
    <div class="tab-heading tab-heading-2line">
      <div class="tab-heading-toggle" class:attention-pulse={screenDimmerMasterAttention} role="button" tabindex="0" onclick={toggleScreenDimmer} onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggleScreenDimmer(); } }} title="Enable Screen Dimmer" aria-label="Enable Screen Dimmer" aria-pressed={screenDimmer.enabled}>
        <img class="tab-icon" src={tabScreenDimmerIcon} alt="" />
        <div class="tab-heading-titles">
          <div class="tab-heading-title-row">
            <h2>Screen Dimmer</h2>
            {#if dimmerMonitorTagLabel}
              <span class="tab-monitor-tag">{dimmerMonitorTagLabel}</span>
            {/if}
          </div>
          <span class="subtitle">Dims the display after inactivity to reduce static OLED content.</span>
        </div>
        <div class="tab-heading-spacer"></div>
        <ShieldBadge size={24} text="Admin required." />
        <span class={`pill${screenDimmer.enabled ? '' : ' off'}`} aria-hidden="true"></span>
      </div>
      <button class="restore-defaults-btn" onclick={restoreScreenDimmerDefaults}>↻ Restore Tab Defaults</button>
    </div>
    <div class="separator"></div>

    <ScreenDimmerState />

    {#if screenError}
      <p class="power-error" role="alert">⚠ {screenError}</p>
    {/if}

    {#if app.isElevated === false && screenDimmer.enabled}
      <div class="dimmer-admin-warning" role="alert">
        <span class="dimmer-admin-warning-icon" aria-hidden="true">⚠</span>
        <div class="dimmer-admin-warning-text">
          <span class="dimmer-admin-warning-title">Administrator rights required</span>
          <span class="dimmer-admin-warning-desc">The Screen Dimmer needs administrator rights for power state detection (pausing while video is playing, etc). Restart OLED Helper as administrator to enable it.</span>
        </div>
        <button type="button" class="dimmer-admin-restart-btn" onclick={restartScreenDimmerAsAdmin}>Restart as administrator</button>
      </div>
    {/if}

    <div class="dimmer-control-group" class:disabled={!screenDimmer.enabled}>
      {#if !screenDimmer.enabled}
        <button type="button" class="disabled-section-hit-area" tabindex="-1" aria-label="Screen Dimmer controls disabled" onclick={handleDisabledScreenDimmerInteraction} onwheel={handleDisabledScreenDimmerWheel}></button>
      {/if}
      <div class="dimmer-control">
        <div class="dimmer-control-heading">
          <span class="settings-row-label">Dim display after</span>
          <span class="slider-val">{screenFmtSeconds(screenDimmer.seconds)}</span>
        </div>
        <div class="slider-track-wrap dimmer-slider-track">
          <input class="dimmer-opacity" type="range" min="5" max="180" step="1"
            value={screenDimmer.seconds} style="--fill:{((screenDimmer.seconds - 5) / 175) * 100}%"
            disabled={!screenDimmer.enabled}
            oninput={handleScreenTimeoutInput} onchange={() => { applyScreenDimmer(true); }}
            onpointerdown={() => screenTimeoutDragging = true} onpointerup={() => screenTimeoutDragging = false} />
        </div>
        <div class="dimmer-scale"><span>5 sec</span><span>180 sec</span></div>
      </div>

    <div class="dimmer-control">
      <div class="dimmer-control-heading">
        <span class="settings-row-label">Dim strength</span>
        <span class="slider-val">{Math.round(screenDimmer.opacity * 100)}%</span>
      </div>
      <div class="slider-track-wrap dimmer-slider-track">
        <input class="dimmer-opacity has-ticks-dimmer" type="range" min="0" max="100" step="1"
          value={Math.round(screenDimmer.opacity * 100)} style="--fill:{screenDimmer.opacity * 100}%"
          disabled={!screenDimmer.enabled}
          oninput={handleScreenOpacityInput} onchange={() => { applyScreenDimmer(true); }}
          onpointerdown={() => screenOpacityDragging = true} onpointerup={() => screenOpacityDragging = false}
          onpointerenter={() => screenOpacityHovering = true} onpointerleave={() => screenOpacityHovering = false} />
        <div class="slider-tooltip" class:visible={screenOpacityDragging || screenOpacityHovering} style="left:calc(9px + ({screenDimmer.opacity} * (100% - 18px)))">{Math.round(screenDimmer.opacity * 100)}%</div>
      </div>
      <div class="dimmer-scale"><span>Subtle</span><span>Dark</span></div>
    </div>

    <div class="dimmer-fade-row">
      <div class="dimmer-control dimmer-fade-col">
        <div class="dimmer-control-heading">
          <span class="settings-row-label">Fade in duration</span>
          <span class="slider-val">{screenDimmer.fadeInDurationMs === 0 ? 'OFF' : `${screenDimmer.fadeInDurationMs} ms`}</span>
        </div>
        <div class="slider-track-wrap dimmer-slider-track">
          <input class="dimmer-opacity has-ticks-dimmer-fade" type="range" min="0" max="5000" step="50"
            value={screenDimmer.fadeInDurationMs} style="--fill:{(screenDimmer.fadeInDurationMs / 5000) * 100}%"
            disabled={!screenDimmer.enabled}
            oninput={handleScreenFadeInInput} onchange={() => { applyScreenDimmer(true); }}
            onpointerdown={() => screenFadeInDragging = true} onpointerup={() => screenFadeInDragging = false}
            onpointerenter={() => screenFadeInHovering = true} onpointerleave={() => screenFadeInHovering = false} />
          <div class="slider-tooltip" class:visible={screenFadeInDragging || screenFadeInHovering} style="left:calc(9px + ({screenDimmer.fadeInDurationMs / 5000} * (100% - 18px)))">{screenDimmer.fadeInDurationMs === 0 ? 'OFF' : `${screenDimmer.fadeInDurationMs} ms`}</div>
        </div>
        <div class="dimmer-scale"><span>Fast</span><span>Slow</span></div>
      </div>

      <div class="dimmer-chain-btn-wrap">
        <button class="dimmer-chain-btn" class:linked={screenDimmer.fadeLinked} disabled={!screenDimmer.enabled} onclick={toggleScreenFadeLinked} title={screenDimmer.fadeLinked ? 'Unlink fade durations' : 'Link fade durations'} aria-label={screenDimmer.fadeLinked ? 'Unlink fade durations' : 'Link fade durations'} aria-pressed={screenDimmer.fadeLinked}>
          <svg viewBox="0 0 24 24" fill="none">
            <path class="chain-bar" d="M10 14L14 10" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>
            <path class="chain-hook chain-hook-a" d="M11 6.5L12.5 5C14.4 3.1 17.5 3.1 19.4 5C21.3 6.9 21.3 10 19.4 11.9L17.9 13.4" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>
            <path class="chain-hook chain-hook-b" d="M13 17.5L11.5 19C9.6 20.9 6.5 20.9 4.6 19C2.7 17.1 2.7 14 4.6 12.1L6.1 10.6" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>
          </svg>
        </button>
      </div>

      <div class="dimmer-control dimmer-fade-col">
        <div class="dimmer-control-heading">
          <span class="settings-row-label">Fade out duration</span>
          <span class="slider-val">{screenDimmer.fadeOutDurationMs === 0 ? 'OFF' : `${screenDimmer.fadeOutDurationMs} ms`}</span>
        </div>
        <div class="slider-track-wrap dimmer-slider-track">
          <input class="dimmer-opacity has-ticks-dimmer-fade" type="range" min="0" max="5000" step="50"
            value={screenDimmer.fadeOutDurationMs} style="--fill:{(screenDimmer.fadeOutDurationMs / 5000) * 100}%"
            disabled={!screenDimmer.enabled}
            oninput={handleScreenFadeOutInput} onchange={() => { applyScreenDimmer(true); }}
            onpointerdown={() => screenFadeOutDragging = true} onpointerup={() => screenFadeOutDragging = false}
            onpointerenter={() => screenFadeOutHovering = true} onpointerleave={() => screenFadeOutHovering = false} />
          <div class="slider-tooltip" class:visible={screenFadeOutDragging || screenFadeOutHovering} style="left:calc(9px + ({screenDimmer.fadeOutDurationMs / 5000} * (100% - 18px)))">{screenDimmer.fadeOutDurationMs === 0 ? 'OFF' : `${screenDimmer.fadeOutDurationMs} ms`}</div>
        </div>
        <div class="dimmer-scale"><span>Fast</span><span>Slow</span></div>
      </div>
    </div>

    <div class="dimmer-control">
      <label class="dimmer-check-toggle">
        <input type="checkbox" checked={screenDimmer.fullscreenDetection} onchange={toggleFullscreenDetection} disabled={!screenDimmer.enabled} />
        <span class="dimmer-check-text">
          <span class="settings-row-label">Pause during fullscreen apps</span>
          <span class="settings-row-desc dimmer-section-desc">Skips dimming while a fullscreen app or game is active on a monitor.</span>
        </span>
      </label>
    </div>
    </div>
  </div>
{:else}
  <div class="tab-body">
  <div class="tab-heading tab-heading-2line">
    <div class="tab-heading-toggle" class:disabled={!taskbarDimmer.available} class:attention-pulse={dimmerMasterAttention} role="button" tabindex="0" onclick={() => { if (taskbarDimmer.available) toggleDimmer(); }} onkeydown={(e) => { if ((e.key === 'Enter' || e.key === ' ') && taskbarDimmer.available) { e.preventDefault(); toggleDimmer(); } }} title="Enable Taskbar Dimmer" aria-label="Enable Taskbar Dimmer" aria-pressed={taskbarDimmer.enabled}>
      <img class="tab-icon" src={tabDimmerIcon} alt="" />
      <div class="tab-heading-titles">
        <div class="tab-heading-title-row">
          <h2>Taskbar Dimmer</h2>
          {#if dimmerMonitorTagLabel}
            <span class="tab-monitor-tag">{dimmerMonitorTagLabel}</span>
          {/if}
        </div>
        <span class="subtitle">Adds a dark overlay over the selected monitor's taskbar.</span>
      </div>
      <div class="tab-heading-spacer"></div>
      <span class={`pill${taskbarDimmer.enabled ? '' : ' off'}`} aria-hidden="true"></span>
    </div>
    <button class="restore-defaults-btn" onclick={restoreTaskbarDimmerDefaults}>↻ Restore Tab Defaults</button>
  </div>
  <div class="separator"></div>

  <div class="dimmer-control-group" class:disabled={!taskbarDimmer.available || !taskbarDimmer.enabled}>
    {#if !taskbarDimmer.available || !taskbarDimmer.enabled}
      <button type="button" class="disabled-section-hit-area" tabindex="-1" aria-label="Taskbar Dimmer controls disabled" onclick={handleDisabledDimmerInteraction} onwheel={handleDisabledDimmerWheel}></button>
    {/if}
    <div class="dimmer-control">
      <div class="dimmer-control-heading">
        <span class="settings-row-label">Overlay opacity</span>
        <span class="slider-val">{Math.round(taskbarDimmer.opacity * 100)}%</span>
      </div>
      <div class="slider-track-wrap dimmer-slider-track">
        <input
          class="dimmer-opacity has-ticks-dimmer"
          type="range"
          min="0"
          max="100"
          step="1"
          value={Math.round(taskbarDimmer.opacity * 100)}
          style="--fill:{taskbarDimmer.opacity * 100}%"
          disabled={!taskbarDimmer.available || !taskbarDimmer.enabled}
          oninput={handleOpacityInput}
          onpointerdown={() => opacityDragging = true}
          onpointerup={() => opacityDragging = false}
          onpointerenter={() => opacityHovering = true}
          onpointerleave={() => opacityHovering = false} />
        <div class="slider-tooltip" class:visible={opacityDragging || opacityHovering} style="left:calc(9px + ({taskbarDimmer.opacity} * (100% - 18px)))">{Math.round(taskbarDimmer.opacity * 100)}%</div>
      </div>
      <div class="dimmer-scale"><span>Subtle</span><span>Dark</span></div>
    </div>

    <div class="dimmer-fade-row">
      <div class="dimmer-control dimmer-fade-col">
        <div class="dimmer-control-heading">
          <span class="settings-row-label">Fade in duration</span>
          <span class="slider-val">{taskbarDimmer.fadeInDurationMs === 0 ? 'OFF' : `${taskbarDimmer.fadeInDurationMs} ms`}</span>
        </div>
        <div class="slider-track-wrap dimmer-slider-track">
          <input
            class="dimmer-opacity has-ticks-dimmer-fade"
            type="range"
            min="0"
            max="1000"
            step="10"
            value={taskbarDimmer.fadeInDurationMs}
            style="--fill:{(taskbarDimmer.fadeInDurationMs / 1000) * 100}%"
            disabled={!taskbarDimmer.available || !taskbarDimmer.enabled}
            oninput={handleFadeInInput}
            onchange={commitFadeIn}
            onpointerdown={() => fadeInDragging = true}
            onpointerup={() => fadeInDragging = false}
            onpointerenter={() => fadeInHovering = true}
            onpointerleave={() => fadeInHovering = false} />
          <div class="slider-tooltip" class:visible={fadeInDragging || fadeInHovering} style="left:calc(9px + ({taskbarDimmer.fadeInDurationMs / 1000} * (100% - 18px)))">{taskbarDimmer.fadeInDurationMs === 0 ? 'OFF' : `${taskbarDimmer.fadeInDurationMs} ms`}</div>
        </div>
        <div class="dimmer-scale"><span>Fast</span><span>Slow</span></div>
      </div>

      <div class="dimmer-chain-btn-wrap">
        <button class="dimmer-chain-btn" class:linked={taskbarDimmer.fadeLinked} disabled={!taskbarDimmer.available || !taskbarDimmer.enabled} class:suppress-hover={chainSuppressHover} onclick={toggleFadeLinked} onpointerleave={() => chainSuppressHover = false} title={taskbarDimmer.fadeLinked ? 'Unlink fade durations' : 'Link fade durations'} aria-label={taskbarDimmer.fadeLinked ? 'Unlink fade durations' : 'Link fade durations'} aria-pressed={taskbarDimmer.fadeLinked}>
          <svg viewBox="0 0 24 24" fill="none">
            <path class="chain-bar" d="M10 14L14 10" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>
            <path class="chain-hook chain-hook-a" d="M11 6.5L12.5 5C14.4 3.1 17.5 3.1 19.4 5C21.3 6.9 21.3 10 19.4 11.9L17.9 13.4" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>
            <path class="chain-hook chain-hook-b" d="M13 17.5L11.5 19C9.6 20.9 6.5 20.9 4.6 19C2.7 17.1 2.7 14 4.6 12.1L6.1 10.6" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>
          </svg>
        </button>
      </div>

      <div class="dimmer-control dimmer-fade-col">
        <div class="dimmer-control-heading">
          <span class="settings-row-label">Fade out duration</span>
          <span class="slider-val">{taskbarDimmer.fadeOutDurationMs === 0 ? 'OFF' : `${taskbarDimmer.fadeOutDurationMs} ms`}</span>
        </div>
        <div class="slider-track-wrap dimmer-slider-track">
          <input
            class="dimmer-opacity has-ticks-dimmer-fade"
            type="range"
            min="0"
            max="1000"
            step="10"
            value={taskbarDimmer.fadeOutDurationMs}
            style="--fill:{(taskbarDimmer.fadeOutDurationMs / 1000) * 100}%"
            disabled={!taskbarDimmer.available || !taskbarDimmer.enabled}
            oninput={handleFadeOutInput}
            onchange={commitFadeOut}
            onpointerdown={() => fadeOutDragging = true}
            onpointerup={() => fadeOutDragging = false}
            onpointerenter={() => fadeOutHovering = true}
            onpointerleave={() => fadeOutHovering = false} />
          <div class="slider-tooltip" class:visible={fadeOutDragging || fadeOutHovering} style="left:calc(9px + ({taskbarDimmer.fadeOutDurationMs / 1000} * (100% - 18px)))">{taskbarDimmer.fadeOutDurationMs === 0 ? 'OFF' : `${taskbarDimmer.fadeOutDurationMs} ms`}</div>
        </div>
        <div class="dimmer-scale"><span>Fast</span><span>Slow</span></div>
      </div>
    </div>
  </div>

  {#if !taskbarDimmer.available}
    <p class="gpu-error">Windows taskbar overlay is unavailable.</p>
  {/if}
  </div>
{/if}
