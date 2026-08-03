<script>
  import { taskbarDimmer, setTaskbarDimmer, restoreTaskbarDimmerDefaults } from './stores.svelte.js';

  function toggleDimmer() {
    setTaskbarDimmer(!taskbarDimmer.enabled);
  }

  let fadeLinked = $state(true);
  let opacityDragging = $state(false);
  let opacityHovering = $state(false);
  let fadeInDragging = $state(false);
  let fadeInHovering = $state(false);
  let fadeOutDragging = $state(false);
  let fadeOutHovering = $state(false);

  $effect(() => {
    if (!opacityDragging && !fadeInDragging && !fadeOutDragging) return;
    function onUp() { opacityDragging = false; fadeInDragging = false; fadeOutDragging = false; }
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pointercancel', onUp);
    return () => {
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
    };
  });

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
    setTaskbarDimmer(taskbarDimmer.enabled, normalized / 100, taskbarDimmer.fadeInDurationMs, taskbarDimmer.fadeOutDurationMs, true, true);
  }

  function handleFadeInInput(event) {
    const duration = Math.max(0, Math.min(1000, +event.currentTarget.value));
    const fadeOut = fadeLinked ? duration : taskbarDimmer.fadeOutDurationMs;
    setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, duration, fadeOut, false);
  }

  function commitFadeIn(event) {
    const duration = Math.max(0, Math.min(1000, +event.currentTarget.value));
    const fadeOut = fadeLinked ? duration : taskbarDimmer.fadeOutDurationMs;
    setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, duration, fadeOut);
  }

  function handleFadeOutInput(event) {
    const duration = Math.max(0, Math.min(1000, +event.currentTarget.value));
    const fadeIn = fadeLinked ? duration : taskbarDimmer.fadeInDurationMs;
    setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, fadeIn, duration, false);
  }

  function commitFadeOut(event) {
    const duration = Math.max(0, Math.min(1000, +event.currentTarget.value));
    const fadeIn = fadeLinked ? duration : taskbarDimmer.fadeInDurationMs;
    setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, fadeIn, duration);
  }

  // Toggling the chain link on immediately syncs fade-out to match fade-in,
  // so the two sliders visually agree the moment they're linked rather than
  // waiting for the next drag.
  //
  // The cursor is still physically over the button right after a click, so
  // :hover immediately re-applies — but now against the OTHER state's hover
  // rule, causing a visible extra jump right after the toggle's own
  // transition plays. chainSuppressHover pins the hooks back to their plain
  // rest-state transform (see .just-toggled in app.css) until the pointer
  // actually leaves the button — no guessed timeout, driven by the real
  // pointerleave event so it can never desync from what the cursor is doing.
  let chainSuppressHover = $state(false);
  function toggleFadeLinked() {
    fadeLinked = !fadeLinked;
    if (fadeLinked && taskbarDimmer.fadeOutDurationMs !== taskbarDimmer.fadeInDurationMs) {
      setTaskbarDimmer(taskbarDimmer.enabled, taskbarDimmer.opacity, taskbarDimmer.fadeInDurationMs, taskbarDimmer.fadeInDurationMs);
    }
    chainSuppressHover = true;
  }
</script>

<div class="tab-body">
  <div class="tab-heading tab-heading-2line">
    <div class="tab-heading-toggle" class:disabled={!taskbarDimmer.available} role="button" tabindex="0" onclick={() => { if (taskbarDimmer.available) toggleDimmer(); }} onkeydown={(e) => { if ((e.key === 'Enter' || e.key === ' ') && taskbarDimmer.available) { e.preventDefault(); toggleDimmer(); } }} title="Enable Taskbar Dimmer" aria-label="Enable Taskbar Dimmer" aria-pressed={taskbarDimmer.enabled}>
      <img class="tab-icon" src="/icons/tab_dimmer.png" alt="" />
      <div class="tab-heading-titles">
        <div class="tab-heading-title-row">
          <h2>Taskbar Dimmer</h2>
          <div class="info-tip-wrap" role="presentation" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
            <button class="info-tip-btn" aria-label="About Taskbar Dimmer" tabindex="0">Info</button>
            <div class="info-tip">Darkens the Windows taskbar without blocking clicks or changing the rest of the display.</div>
          </div>
        </div>
        <span class="subtitle">Uses a click-through overlay over the taskbar.</span>
      </div>
      <div class="tab-heading-spacer"></div>
      <span class={`pill${taskbarDimmer.enabled ? '' : ' off'}`} aria-hidden="true"></span>
    </div>
    <button class="restore-defaults-btn" onclick={restoreTaskbarDimmerDefaults}>↻ Restore defaults</button>
  </div>
  <div class="separator"></div>

  <div class="dimmer-control-group" class:disabled={!taskbarDimmer.available || !taskbarDimmer.enabled}>
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
        <button class="dimmer-chain-btn" class:linked={fadeLinked} class:suppress-hover={chainSuppressHover} onclick={toggleFadeLinked} onpointerleave={() => chainSuppressHover = false} title={fadeLinked ? 'Unlink fade durations' : 'Link fade durations'} aria-label={fadeLinked ? 'Unlink fade durations' : 'Link fade durations'} aria-pressed={fadeLinked}>
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
