<script>
  import { onMount, untrack } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { app, pollHDRLuminance, setSdrWhiteNits } from './stores.svelte.js';
  import SdrHdrCompare from './SdrHdrCompare.svelte';

  let currentNits = $state(null);
  let detectedNits = $state(null);
  let applying = $state(false);
  let applyQueued = $state(false);
  let applyTimer = null;
  let error = $state('');
  let sliderDragging = $state(false);
  let sliderHovering = $state(false);
  const MIN_NITS = 80;
  const MAX_NITS = 480;
  // ITU-R BT.2400 reference white for HDR — the standard diffuse white level
  // that Chromium uses when tone-mapping HDR screenshots to SDR. At this value
  // screenshots from Chromium browsers render with their intended brightness.
  const DEFAULT_NITS = 203;
  let lastHdrStatus = app.hdrStatus;
  let lastMonitorIdentity = app.displayInfo.monitorId || app.displayInfo.deviceName;
  let hdrAvailable = $derived(app.hdrStatus === 'hdr');
  let monitorName = $derived(app.displayInfo.monitorName || 'Detecting monitor...');

  function formatNits(value) {
    return value == null || !Number.isFinite(Number(value)) ? '—' : `${Math.round(Number(value))} nits`;
  }
  async function refresh() {
    error = '';
    try {
      const info = await invoke('get_sdr_white_level');
      detectedNits = info ? Number(info.nits) : null;
      // Always sync the displayed value to what the system reports, so an
      // external change (Windows Settings slider, another tool) is picked up
      // by the poll. Callers that must not clobber a fresh user edit (the
      // poll) guard via sliderDragging/applying/applyTimer before calling.
      currentNits = detectedNits;
      // Publish to the shared store so the Crush Brightness strip compensates
      // for the live SDR white level (Chromium maps the HDR canvas relative
      // to this value).
      if (detectedNits != null) setSdrWhiteNits(detectedNits);
    } catch (e) { error = String(e); }
  }
  async function apply() {
    if (!hdrAvailable || currentNits == null) return;
    if (applying) { applyQueued = true; return; }
    applying = true; applyQueued = false; error = '';
    const requestedNits = Number(currentNits);
    try {
      const result = await invoke('set_sdr_brightness_nits', { nits: requestedNits });
      if (Number(currentNits) !== requestedNits) applyQueued = true;
      if (result?.nits && Number(currentNits) === requestedNits) currentNits = Number(result.nits);
      setSdrWhiteNits(currentNits);
    } catch (e) { error = String(e); }
    finally {
      applying = false;
      if (applyQueued) { applyQueued = false; scheduleApply(); }
    }
  }
  function scheduleApply() {
    clearTimeout(applyTimer);
    applyTimer = setTimeout(() => { applyTimer = null; void apply(); }, 60);
  }
  function onSliderInput(event) {
    currentNits = Math.min(Number(event.currentTarget.value), MAX_NITS);
    scheduleApply();
  }
  async function reset() {
    clearTimeout(applyTimer);
    currentNits = DEFAULT_NITS;
    await apply();
  }

  $effect(() => {
    if (!sliderDragging) return;
    function onUp() { sliderDragging = false; }
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pointercancel', onUp);
    return () => {
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
    };
  });

  onMount(() => {
    pollHDRLuminance();
    refresh();
    // Windows does NOT broadcast a display-change message when the SDR white
    // level slider moves (it is an in-place compositor update, not a display
    // topology change), so there is no event to listen for. Poll only while
    // this tab is active — one cheap CCD read every 2 s, cleared on unmount.
    const poll = setInterval(() => {
      // applyTimer != null means a slider edit is still in its 60ms debounce
      // window — don't overwrite it with the previous CCD value before the
      // user's new value has been applied.
      if (sliderDragging || applying || applyTimer !== null) return;
      refresh();
    }, 500);
    return () => {
      clearTimeout(applyTimer);
      clearInterval(poll);
    };
  });

  $effect(() => {
    const hdr = app.hdrStatus;
    const monitor = app.displayInfo.monitorId || app.displayInfo.deviceName;
    const hdrChanged = hdr !== lastHdrStatus;
    const monitorChanged = monitor !== lastMonitorIdentity;
    if (!hdrChanged && !monitorChanged) return;

    lastHdrStatus = hdr;
    lastMonitorIdentity = monitor;
    untrack(() => {
      if (hdr !== 'hdr') { currentNits = null; return; }
      currentNits = null;
      refresh();
    });
  });
</script>

<div class="tab-body">
  <div class="tab-heading tab-heading-2line">
    <div class="tab-heading-toggle">
      <span class="experimental-tab-icon" aria-hidden="true">⚗</span>
      <div class="tab-heading-titles">
        <div class="tab-heading-title-row">
          <h2>SDR Content Brightness</h2>
        </div>
        <span class="subtitle">Adjusts regular SDR content while Windows HDR is enabled.</span>
      </div>
    </div>
    <div class="tab-heading-spacer"></div>
    <button class="restore-defaults-btn" type="button" onclick={reset} disabled={!hdrAvailable || applying}>↻ Reset to 203 nits</button>
  </div>
  <div class="separator"></div>

  <div class="dimmer-control-group" class:disabled={!hdrAvailable || currentNits === null}>
    <div class="dimmer-control">
      <div class="dimmer-control-heading">
        <span class="settings-row-label">SDR white level</span>
        <span class="slider-val">{formatNits(currentNits)}</span>
      </div>
      <div class="slider-track-wrap dimmer-slider-track">
        <input
          class="dimmer-opacity has-ticks-dimmer"
          type="range"
          min={MIN_NITS}
          max={MAX_NITS}
          step="1"
          value={Math.min(Number(currentNits ?? MIN_NITS), MAX_NITS)}
          style="--fill:{((Math.min(Number(currentNits ?? MIN_NITS), MAX_NITS) - MIN_NITS) / (MAX_NITS - MIN_NITS)) * 100}%"
          disabled={!hdrAvailable || currentNits === null}
          oninput={onSliderInput}
          onpointerdown={() => sliderDragging = true}
          onpointerup={() => sliderDragging = false}
          onpointerenter={() => sliderHovering = true}
          onpointerleave={() => sliderHovering = false} />
        <div class="slider-tooltip" class:visible={sliderDragging || sliderHovering} style="left:calc(9px + ({((Math.min(Number(currentNits ?? MIN_NITS), MAX_NITS) - MIN_NITS) / (MAX_NITS - MIN_NITS))} * (100% - 18px)))">{formatNits(currentNits)}</div>
      </div>
      <div class="dimmer-scale"><span>{MIN_NITS} nits</span><span>{MAX_NITS} nits</span></div>
    </div>

    <div class="experimental-monitor-rows">
      <div class="experimental-monitor-row"><span>Current display</span><strong>{monitorName}</strong></div>
      <div class="experimental-monitor-row"><span>HDR peak luminance</span><strong>{formatNits(app.hdrLuminance?.maxNits)}</strong></div>
      <div class="experimental-monitor-row"><span>Current SDR white</span><strong>{formatNits(currentNits ?? detectedNits)}</strong></div>
    </div>
  </div>

  {#if hdrAvailable}
    <div class="experimental-section-note">Compare SDR white against true HDR luminance. Top row shows each level as SDR content at the current white point (clips to white above it); bottom row shows the same levels as fixed HDR luminance. Align the rows by setting the slider to 203 nits.</div>
    <SdrHdrCompare sdrWhiteNits={currentNits ?? detectedNits ?? 203} hdrActive={hdrAvailable} />
  {/if}

  {#if !hdrAvailable}
    <p class="gpu-error">Enable Windows HDR to use this control.</p>
  {/if}
  {#if error}
    <p class="gpu-error">{error}</p>
  {/if}
</div>
