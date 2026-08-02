<script>
  import { taskbarDimmer, setTaskbarDimmer, restoreTaskbarDimmerDefaults } from './stores.svelte.js';

  function toggleDimmer() {
    setTaskbarDimmer(!taskbarDimmer.enabled);
  }

  // Snap to the nearest tick (every 10, 10–90) when the raw value lands
  // within a few points of one, mirroring the Black Level slider's
  // snap-to-zero (normalizeBlackLevel in TabCrush.svelte). 0 and 100 aren't
  // included since the track's own ends already act as a natural stop there.
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
    const normalized = normalizeOpacityValue(raw);
    if (raw !== normalized) event.currentTarget.value = normalized;
    setTaskbarDimmer(taskbarDimmer.enabled, normalized / 100, taskbarDimmer.easeDurationMs, true, true);
  }

  function handleEaseDurationInput(event) {
    const duration = Math.max(0, Math.min(1000, +event.currentTarget.value));
    setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, duration, false);
  }

  function commitEaseDuration(event) {
    const duration = Math.max(0, Math.min(1000, +event.currentTarget.value));
    setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, duration);
  }
</script>

<div class="tab-body">
  <div class="tab-heading">
    <img class="tab-icon" src="/icons/tab_dimmer.png" alt="" />
    <h2>Taskbar Dimmer</h2>
    <div class="info-tip-wrap">
      <button class="info-tip-btn" aria-label="About Taskbar Dimmer" tabindex="0">?</button>
      <div class="info-tip">Darkens the Windows taskbar without blocking clicks or changing the rest of the display.</div>
    </div>
    <div class="tab-heading-spacer"></div>
    <button class="restore-defaults-btn" onclick={restoreTaskbarDimmerDefaults}>↻ Restore defaults</button>
  </div>
  <div class="separator"></div>

  <button class="settings-row dimmer-toggle-row" disabled={!taskbarDimmer.available} onclick={toggleDimmer}>
    <div class="settings-row-text">
      <span class="settings-row-label">Enable Taskbar Dimmer</span>
      <span class="settings-row-desc">Uses a click-through overlay over the taskbar.</span>
    </div>
    <div class="pill-wrap">
      <span class={`pill${taskbarDimmer.enabled ? '' : ' off'}`} aria-hidden="true"></span>
    </div>
  </button>

  <div class="dimmer-control" class:disabled={!taskbarDimmer.available || !taskbarDimmer.enabled}>
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
        oninput={handleOpacityInput} />
    </div>
    <div class="dimmer-scale"><span>Subtle</span><span>Dark</span></div>

    <div class="dimmer-control-heading dimmer-duration-heading">
      <span class="settings-row-label">Ease duration</span>
      <span class="slider-val">{taskbarDimmer.easeDurationMs === 0 ? 'OFF' : `${taskbarDimmer.easeDurationMs} ms`}</span>
    </div>
    <div class="slider-track-wrap dimmer-slider-track">
      <input
        class="dimmer-opacity has-ticks-dimmer"
        type="range"
        min="0"
        max="1000"
        step="10"
        value={taskbarDimmer.easeDurationMs}
        style="--fill:{(taskbarDimmer.easeDurationMs / 1000) * 100}%"
        disabled={!taskbarDimmer.available || !taskbarDimmer.enabled}
        oninput={handleEaseDurationInput}
        onchange={commitEaseDuration} />
    </div>
    <div class="dimmer-scale"><span>Fast</span><span>Slow</span></div>
  </div>

  {#if !taskbarDimmer.available}
    <p class="gpu-error">Windows taskbar overlay is unavailable.</p>
  {/if}
</div>
