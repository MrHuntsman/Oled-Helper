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
  // Tick marks land on round nits values, with the BT.2400 reference (203
  // nits) included and highlighted — the exact position the Reset button
  // targets. 200 is deliberately skipped so it can't crowd 203.
  const TICKS_NITS = [100, 150, DEFAULT_NITS, 250, 300, 350, 400, 450];
  // Drag lands within this many nits of the reference white → snap to it,
  // so the alignment point (203) is easy to hit by feel.
  const SNAP_RANGE_NITS = 2;
  const sliderFraction = (nits) => (nits - MIN_NITS) / (MAX_NITS - MIN_NITS);
  const ticksBackground =
    TICKS_NITS.map((nits) => {
      const f = sliderFraction(nits).toFixed(4);
      const edge = (sign) => `calc((100% - 18px) * ${f} + 9px ${sign} 0.75px)`;
      // The 203 tick uses full white instead of the accent so it stays visible
      // in every thumb position: the accent fill sits to its left (slider
      // below 203) or covers it entirely (slider above 203 — accent-on-accent
      // was invisible). White reads against both the fill and the dark track.
      const color = nits === DEFAULT_NITS ? '#fff' : 'rgba(255,255,255,0.45)';
      return `transparent ${edge('-')}, ${color} ${edge('-')}, ${color} ${edge('+')}, transparent ${edge('+')}`;
    }).join(',\n      ');
  // Left offset for the "203 nits" caption under the track — matches the
  // thumb/tick coordinate space (thumb travel is [9px, 100% - 9px]).
  const referenceLabelLeft =
    `calc(9px + (100% - 18px) * ${sliderFraction(DEFAULT_NITS).toFixed(4)})`;
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
  // Snap to the reference white only while actually dragging with the
  // pointer, mirroring the other ticked sliders (Black Level in TabCrush,
  // opacity in TabDimmer): write the corrected value back to the input so
  // the thumb lands exactly on the tick (otherwise the native control keeps
  // the raw position and visibly fights the snapped value), and let keyboard
  // arrows bypass the snap so they never feel stuck inside the band.
  function onSliderInput(event) {
    const raw = Math.min(Number(event.currentTarget.value), MAX_NITS);
    const normalized =
      sliderDragging && Math.abs(raw - DEFAULT_NITS) <= SNAP_RANGE_NITS ? DEFAULT_NITS : raw;
    if (raw !== normalized) event.currentTarget.value = normalized;
    currentNits = normalized;
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
          class="dimmer-opacity has-ticks-sdr-white"
          type="range"
          min={MIN_NITS}
          max={MAX_NITS}
          step="1"
          value={Math.min(Number(currentNits ?? MIN_NITS), MAX_NITS)}
          style="--fill:{((Math.min(Number(currentNits ?? MIN_NITS), MAX_NITS) - MIN_NITS) / (MAX_NITS - MIN_NITS)) * 100}%;--tick-bg:linear-gradient(to right,{ticksBackground})"
          disabled={!hdrAvailable || currentNits === null}
          oninput={onSliderInput}
          onpointerdown={() => sliderDragging = true}
          onpointerup={() => sliderDragging = false}
          onpointerenter={() => sliderHovering = true}
          onpointerleave={() => sliderHovering = false} />
        <div class="slider-tooltip" class:visible={sliderDragging || sliderHovering} style="left:calc(9px + ({((Math.min(Number(currentNits ?? MIN_NITS), MAX_NITS) - MIN_NITS) / (MAX_NITS - MIN_NITS))} * (100% - 18px)))">{formatNits(currentNits)}</div>
      </div>
      <div class="dimmer-scale">
        <span>{MIN_NITS} nits</span>
        <span class="dimmer-scale-mid" style="left:{referenceLabelLeft}">{DEFAULT_NITS} nits</span>
        <span>{MAX_NITS} nits</span>
      </div>
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
