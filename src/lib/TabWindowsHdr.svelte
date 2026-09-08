<script>
  import { onMount, untrack } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { emit, listen } from '@tauri-apps/api/event';
  import { app, pollHDRLuminance, setSdrWhiteNits } from './stores.svelte.js';
  import { COMPARISON_NITS } from './calibration.js';
  import tabHdrIcon from './icons/tab_hdr.png?inline';
  import SdrHdrCompare from './SdrHdrCompare.svelte';
  import VendorTag from './VendorTag.svelte';
  import { createDropdownPositionController } from './dropdown-position.js';
  import { createDropdownKeyboardHandlers } from './dropdown.js';

  let currentNits = $state(null);
  let detectedNits = $state(null);
  let applying = $state(false);
  let applyQueued = $state(false);
  let applyTimer = null;
  // While dragging, apply the brightness continuously (~20 updates/sec,
  // matching Windows' own slider) instead of only after release. A pure
  // trailing debounce never fires while inputs arrive faster than the delay
  // (a drag emits ~16-33ms apart), so the old 60ms debounce effectively
  // applied nothing until the drag ended. The throttle below applies on the
  // leading edge (as soon as the window since the last apply has elapsed)
  // and still guarantees a trailing apply so the final value always lands.
  const APPLY_THROTTLE_MS = 50;
  let lastApplyTime = 0;
  let error = $state('');
  let sliderDragging = $state(false);
  let sliderHovering = $state(false);
  const MIN_NITS = 80;
  const MAX_NITS = 480;
  // ITU-R BT.2408 reference white for HDR, the standard diffuse white level
  // that Chromium uses when tone-mapping HDR screenshots to SDR. At this value
  // screenshots from Chromium browsers render with their intended brightness.
  const DEFAULT_NITS = 203;
  // Tick marks sit on the SAME evenly-spaced grid as the comparison squares
  // below (COMPARISON_NITS), so every tick lands exactly under a square's
  // center, the slider and the ramp share one aligned scale.
  const TICKS_NITS = COMPARISON_NITS;
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
      // below 203) or covers it entirely (slider above 203, accent-on-accent
      // was invisible). White reads against both the fill and the dark track.
      const color = nits === DEFAULT_NITS ? '#fff' : 'rgba(255,255,255,0.45)';
      return `transparent ${edge('-')}, ${color} ${edge('-')}, ${color} ${edge('+')}, transparent ${edge('+')}`;
    }).join(',\n      ');
  // Left offset for the "203 nits" caption under the track, matches the
  // thumb/tick coordinate space (thumb travel is [9px, 100% - 9px]).
  const referenceLabelLeft =
    `calc(9px + (100% - 18px) * ${sliderFraction(DEFAULT_NITS).toFixed(4)})`;
  let lastHdrStatus = app.hdrStatus;
  let lastMonitorIdentity = app.displayInfo.monitorId || app.displayInfo.deviceName;
  let hdrAvailable = $derived(app.hdrStatus === 'hdr');

  function formatNits(value) {
    return value == null || !Number.isFinite(Number(value)) ? '-' : `${Math.round(Number(value))} nits`;
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
  async function apply(requestedNits) {
    if (!hdrAvailable || currentNits == null) return;
    if (applying) { applyQueued = true; return; }
    applying = true; applyQueued = false; error = '';
    lastApplyTime = Date.now();
    try {
      // Every write commits the value so the screen updates live while
      // dragging. The backend ignores the settings-change broadcast that the
      // write triggers on some systems (recent_sdr_white_write), which is
      // what used to make dragging re-enumerate the display config each step.
      const result = await invoke('set_sdr_brightness_nits', { nits: requestedNits });
      if (result?.nits && Number(currentNits) === requestedNits) currentNits = Number(result.nits);
      setSdrWhiteNits(currentNits);
      emit('sdr-content-brightness-main-changed', { nits: Number(currentNits) }).catch(() => {});
    } catch (e) { error = String(e); }
    finally {
      applying = false;
      if (applyQueued) { applyQueued = false; scheduleApply(); }
    }
  }
  function scheduleApply(nits = Number(currentNits)) {
    clearTimeout(applyTimer);
    if (sliderDragging) {
      // Throttle while dragging: apply as soon as the throttle window since
      // the last actual apply has elapsed (leading edge), so a continuous
      // drag updates the screen live instead of waiting for release.
      const since = Date.now() - lastApplyTime;
      const delay = since >= APPLY_THROTTLE_MS ? 0 : APPLY_THROTTLE_MS - since;
      applyTimer = setTimeout(() => { applyTimer = null; void apply(nits); }, delay);
    } else {
      // Keyboard arrows / programmatic changes: keep the trailing debounce.
      applyTimer = setTimeout(() => { applyTimer = null; void apply(nits); }, 60);
    }
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
    scheduleApply(normalized);
  }
  // Auto HDR is a system-wide Windows setting stored by hdr_toggle.rs.
  // The selector below deliberately treats it and NVIDIA RTX HDR as one
  // mutually-exclusive mode. null means the state has not loaded yet.
  let autoHdrEnabled = $state(null);
  let autoHdrError = $state('');
  let hdrModeBusy = $state(false);
  let hdrModeError = $state('');
  let autoHdrRefreshing = false;
  let pollTicks = 0;
  // Transient "Applied" confirmations shown after a successful mode/option
  // change. Cleared automatically after a short delay.
  let hdrModeApplied = $state(false);
  let rtxHdrFlagsApplied = $state(false);
  let hdrModeAppliedTimer = null;
  let rtxHdrFlagsAppliedTimer = null;

  const RTX_HDR_OPTIONS = [
    { value: 0x06, label: 'No Deband (Performance)' },
    { value: 0x0A, label: 'Medium Deband' },
    { value: 0x02, label: 'High Deband' },
  ];
  let rtxHdrEnabled = $state(null);
  let rtxHdrFlags = $state(null);
  let rtxHdrError = $state('');
  let rtxHdrRefreshing = false;
  let rtxHdrFlagsBusy = $state(false);
  let rtxHdrMenuOpen = $state(false);
  let rtxHdrMenuPlacement = $state('down-right');
  let rtxHdrHighlightIndex = $state(-1);
  let rtxHdrWrapEl = $state(null);
  let rtxHdrMenuEl = $state(null);
  let rtxHdrPositionController = null;
  // Auto-resolution guard for the "both modes enabled" conflict: one attempt
  // per conflict with a cooldown after a failed attempt, so a persistent
  // conflict (e.g. an external tool re-enabling Auto HDR) is not hammered
  // into a tight retry loop.
  let hdrConflictResolving = $state(false);
  // Plain (non-reactive) marker, safe because it is only ever written in the
  // same synchronous block as hdrConflictResolving (a $state write), so the
  // template and this effect always observe the updated value.
  let hdrConflictResolveAt = 0;
  const isNvidiaGpu = $derived(app.displayInfo.vendor === 'nvidia');
  const rtxHdrLabel = $derived(
    RTX_HDR_OPTIONS.find((option) => option.value === rtxHdrFlags)?.label ?? 'Unknown'
  );

  async function refreshAutoHdr() {
    if (autoHdrRefreshing) return;
    autoHdrRefreshing = true;
    try {
      autoHdrEnabled = !!await invoke('get_auto_hdr_enabled');
      autoHdrError = '';
    } catch (e) {
      autoHdrError = String(e);
    } finally {
      autoHdrRefreshing = false;
    }
  }

  async function refreshRtxHdr() {
    if (!isNvidiaGpu || rtxHdrRefreshing) return;
    rtxHdrRefreshing = true;
    try {
      const settings = await invoke('get_rtx_hdr_settings_state');
      rtxHdrEnabled = !!settings?.enabled;
      rtxHdrFlags = Number(settings?.flags ?? 0);
      rtxHdrError = '';
    } catch (e) {
      rtxHdrError = String(e);
    } finally {
      rtxHdrRefreshing = false;
    }
  }

  const hdrModeReady = $derived(
    autoHdrEnabled !== null && (!isNvidiaGpu || rtxHdrEnabled !== null)
  );
  const hdrMode = $derived(
    !hdrModeReady ? null
      : autoHdrEnabled && isNvidiaGpu && rtxHdrEnabled ? 'conflict'
      : autoHdrEnabled ? 'auto'
      : isNvidiaGpu && rtxHdrEnabled ? 'rtx'
      : 'off'
  );
  const hdrModeOptions = $derived(isNvidiaGpu
    ? [
        { value: 'off', label: 'Off', description: 'Disable all auto HDR modes.' },
        { value: 'auto', label: 'Windows Auto HDR', description: 'Use Windows auto HDR.' },
        { value: 'rtx', label: 'NVIDIA RTX HDR', description: 'Use NVIDIA RTX HDR.' },
      ]
    : [
        { value: 'off', label: 'Off', description: 'Disable Windows Auto HDR.' },
        { value: 'auto', label: 'Windows Auto HDR', description: 'Use Windows tone mapping for supported SDR games.' },
      ]);

  function currentRtxHdrOptionIndex() {
    return RTX_HDR_OPTIONS.findIndex((option) => option.value === rtxHdrFlags);
  }

  function closeRtxHdrMenu() {
    rtxHdrMenuOpen = false;
    rtxHdrHighlightIndex = -1;
    rtxHdrPositionController?.stop();
    rtxHdrPositionController = null;
  }

  function toggleRtxHdrMenu() {
    rtxHdrMenuOpen = !rtxHdrMenuOpen;
    if (rtxHdrMenuOpen) {
      rtxHdrHighlightIndex = currentRtxHdrOptionIndex();
      rtxHdrPositionController?.stop();
      rtxHdrPositionController = createDropdownPositionController({
        getTrigger: () => rtxHdrWrapEl?.querySelector('.rtx-hdr-trigger'),
        getMenu: () => rtxHdrMenuEl,
        onPlacement: (placement) => { rtxHdrMenuPlacement = placement; },
        options: { gap: 6, maxHeight: 240 },
      });
      rtxHdrPositionController.start();
    } else {
      closeRtxHdrMenu();
    }
  }

  async function selectRtxHdrFlags(value) {
    closeRtxHdrMenu();
    if (rtxHdrFlagsBusy || !isNvidiaGpu || hdrMode !== 'rtx' || rtxHdrFlags === value) return;
    rtxHdrFlagsBusy = true;
    rtxHdrError = '';
    const previous = rtxHdrFlags;
    rtxHdrFlags = value;
    try {
      // The write runs in-process when the session allows it and routes
      // through the one-time elevated helper otherwise; the app never
      // relaunches elevated for this.
      await invoke('set_rtx_hdr_state', { flags: value });
      await refreshRtxHdr();
      rtxHdrFlagsApplied = true;
      clearTimeout(rtxHdrFlagsAppliedTimer);
      rtxHdrFlagsAppliedTimer = setTimeout(() => { rtxHdrFlagsApplied = false; }, 2000);
    } catch (e) {
      rtxHdrError = String(e);
      rtxHdrFlags = previous;
      await refreshRtxHdr();
    } finally {
      rtxHdrFlagsBusy = false;
    }
  }

  const rtxHdrKeyboard = createDropdownKeyboardHandlers({
    getOptions: () => RTX_HDR_OPTIONS,
    getSelectedIndex: currentRtxHdrOptionIndex,
    getHighlightIndex: () => rtxHdrHighlightIndex,
    setHighlightIndex: (index) => { rtxHdrHighlightIndex = index; },
    isOpen: () => rtxHdrMenuOpen,
    toggle: toggleRtxHdrMenu,
    close: closeRtxHdrMenu,
    select: (index) => {
      const option = RTX_HDR_OPTIONS[index];
      if (option) selectRtxHdrFlags(option.value);
    },
    getMenu: () => rtxHdrMenuEl,
    getTrigger: () => rtxHdrWrapEl?.querySelector('.rtx-hdr-trigger'),
  });

  async function selectHdrMode(mode) {
    if (!hdrModeReady || hdrModeBusy || !hdrAvailable) return;
    if (mode === 'rtx' && !isNvidiaGpu) return;
    if (mode === hdrMode) return;

    hdrModeBusy = true;
    hdrModeError = '';
    rtxHdrError = '';
    try {
      // Keep the transition in one backend command. It disables the old path
      // before enabling the new one, and rolls back RTX if the Auto HDR write
      // fails, so both systems cannot remain enabled after a failed
      // transition. DRS writes that need elevation route through the
      // one-time helper instead of relaunching the app.
      await invoke('set_hdr_enhancement_mode', { mode });
      await refreshAutoHdr();
      await refreshRtxHdr();
      hdrModeApplied = true;
      clearTimeout(hdrModeAppliedTimer);
      hdrModeAppliedTimer = setTimeout(() => { hdrModeApplied = false; }, 2000);
    } catch (e) {
      hdrModeError = String(e);
      await refreshAutoHdr();
      await refreshRtxHdr();
    } finally {
      hdrModeBusy = false;
    }
  }

  // Close the RTX HDR options menu on outside click or Escape.
  $effect(() => {
    if (!rtxHdrMenuOpen) return;
    function onDown(event) {
      if (rtxHdrWrapEl && !rtxHdrWrapEl.contains(event.target)) {
        closeRtxHdrMenu();
      }
    }
    function onKey(event) {
      if (event.key === 'Escape') {
        closeRtxHdrMenu();
      }
    }
    // The window losing focus (minimize, backgrounded, hidden to tray)
    // must close the menu too, it would otherwise still be open, stale,
    // when the user returns to the window.
    function onBlur() {
      closeRtxHdrMenu();
    }
    window.addEventListener('pointerdown', onDown);
    window.addEventListener('keydown', onKey);
    window.addEventListener('blur', onBlur);
    return () => {
      window.removeEventListener('pointerdown', onDown);
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('blur', onBlur);
      closeRtxHdrMenu();
    };
  });

  // Load RTX HDR as soon as the GPU vendor is known (it arrives async from
  // refresh_vendor_display_info, possibly after mount).
  $effect(() => {
    if (isNvidiaGpu) refreshRtxHdr();
  });

  // Both Windows Auto HDR and RTX HDR enabled at once is a state the app
  // never creates itself (every transition disables the other system), it
  // only arises from external tools (Windows Settings + NVIDIA App/Profile
  // Inspector) and double-converts the frame. Resolve it automatically toward
  // RTX HDR: RTX is already enabled in this state, so the transition only
  // writes the Auto HDR registry flag and needs no elevation.
  $effect(() => {
    if (hdrMode !== 'conflict') {
      hdrConflictResolveAt = 0;
      return;
    }
    if (!hdrAvailable) return;
    if (hdrConflictResolving) return;
    if (Date.now() - hdrConflictResolveAt < 10_000) return;
    hdrConflictResolving = true;
    selectHdrMode('rtx').finally(() => {
      hdrConflictResolving = false;
      hdrConflictResolveAt = Date.now();
    });
  });

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
    let stopSdrBrightnessListener = null;
    listen('sdr-content-brightness-changed', (event) => {
      const nits = Number(event.payload?.nits);
      if (!Number.isFinite(nits) || nits < MIN_NITS || nits > MAX_NITS) return;
      if (sliderDragging || applying || applyTimer !== null) return;
      currentNits = nits;
      detectedNits = nits;
      setSdrWhiteNits(nits);
    }).then((stop) => { stopSdrBrightnessListener = stop; }).catch(() => {});
    // Windows does NOT broadcast a display-change message when the SDR white
    // level slider moves (it is an in-place compositor update, not a display
    // topology change), so there is no event to listen for. Poll only while
    // this tab is active, the CCD read runs on every 500 ms tick, but it is
    // cheap: the primary-HDR target lookup is cached in Rust (5 s TTL) and
    // only the single DisplayConfigGetDeviceInfo value read runs per tick.
    // Cleared on unmount.
    const poll = setInterval(() => {
      // applyTimer != null means a slider edit is still in its 60ms debounce
      // window, don't overwrite it with the previous CCD value before the
      // user's new value has been applied.
      if (sliderDragging || applying || applyTimer !== null) return;
      refresh();
      // Auto HDR state changes rarely (only via Windows Settings or here), so
      // a 2 s cadence while the tab is open keeps the pill in sync without a
      // registry read on every tick.
      if (hdrAvailable && (++pollTicks % 4) === 0) refreshAutoHdr();
      // RTX HDR can be changed externally (Profile Inspector, Control Panel).
      // Poll it every tick (500 ms) while the tab is open, each read is a
      // handful of cheap in-process driver calls with no GPU sync, and the
      // refresh guard below prevents overlapping reads.
      if (hdrAvailable && isNvidiaGpu) refreshRtxHdr();
    }, 500);
    refreshAutoHdr();
    refreshRtxHdr();
    // External tools (Profile Inspector) may change RTX HDR while this tab is
    // focused; refresh immediately when the window regains focus.
    function onFocus() {
      refresh();
      if (hdrAvailable) refreshAutoHdr();
      if (isNvidiaGpu) refreshRtxHdr();
    }
    window.addEventListener('focus', onFocus);
    return () => {
      clearTimeout(applyTimer);
      clearTimeout(hdrModeAppliedTimer);
      clearTimeout(rtxHdrFlagsAppliedTimer);
      clearInterval(poll);
      window.removeEventListener('focus', onFocus);
      stopSdrBrightnessListener?.();
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
      refreshAutoHdr();
      refreshRtxHdr();
    });
  });
</script>

<div class="tab-body">
  <div class="tab-heading tab-heading-2line">
    <div class="tab-heading-toggle no-hover">
      <img class="tab-icon" src={tabHdrIcon} alt="" />
      <div class="tab-heading-titles">
        <div class="tab-heading-title-row">
          <h2>Windows HDR</h2>
          <span class="tab-monitor-tag">Global</span>
        </div>
        <span class="subtitle">Change Windows HDR settings.</span>
      </div>
    </div>
    <div class="tab-heading-spacer"></div>
  </div>
  <div class="separator"></div>

  <div class="dimmer-control-group" class:disabled={!hdrAvailable || currentNits === null}>
    <div class="dimmer-control">
      <div class="dimmer-control-heading">
        <span class="hdr-control-title">
          <span class="settings-row-label">SDR Content Brightness</span>
          <span class="info-tip-wrap">
            <button class="info-tip-btn" aria-label="About SDR Content Brightness" tabindex="0">Info</button>
            <span class="info-tip">
              <span>Adjusts SDR content brightness while HDR is enabled.</span>
              {#if hdrAvailable}
                <span class="info-tip-note">
                  <svg viewBox="0 0 16 16" fill="none"><circle cx="8" cy="8" r="6.5" stroke="currentColor" stroke-width="1.3"/><path d="M8 7.2V11.3" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><circle cx="8" cy="4.9" r="0.9" fill="currentColor"/></svg>
                  <span>In Chromium browsers, HDR images are affected by SDR brightness and 203 renders them accurately.</span>
                </span>
              {/if}
            </span>
          </span>
        </span>
        <span class="slider-val">{formatNits(currentNits)}</span>
      </div>
      <!-- The track + scale block shrinks from the RIGHT by the comparison's
           label column (--sdr-label-block), so the slider's ticks and the
           comparison squares share one horizontal x-scale and align exactly.
           Only when the comparison is actually shown (HDR on); with HDR off
           the slider keeps its full width. -->
      <div class:exp-track-block={hdrAvailable}>
        <div class="slider-track-wrap dimmer-slider-track">
          <input
            class="dimmer-opacity has-ticks-sdr-white"
            type="range"
            min={MIN_NITS}
            max={MAX_NITS}
            step="1"
            aria-label="SDR Content Brightness"
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
      {#if hdrAvailable}
        <SdrHdrCompare sdrWhiteNits={currentNits ?? detectedNits ?? 203} hdrActive={hdrAvailable} />
      {/if}
    </div>
  </div>

  <div class="dimmer-control-group" class:disabled={!hdrAvailable || !hdrModeReady}>
    <div class="dimmer-control">
      <div class="dimmer-control-heading">
        <span class="hdr-control-title">
          <span class="settings-row-label">SDR to HDR inverse tone mapping</span>
          <span class="info-tip-wrap">
            <button class="info-tip-btn" aria-label="About HDR enhancement mode" tabindex="0">Info</button>
            <span class="info-tip">
              <span>SDR to HDR conversion methods.</span>
              <span class="info-tip-note">
                <svg viewBox="0 0 16 16" fill="none"><circle cx="8" cy="8" r="6.5" stroke="currentColor" stroke-width="1.3"/><path d="M8 7.2V11.3" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><circle cx="8" cy="4.9" r="0.9" fill="currentColor"/></svg>
                <span>Games may need a restart after changing the mode.</span>
              </span>
            </span>
          </span>
        </span>
      </div>
      <div class="hdr-mode-switch" role="radiogroup" aria-label="HDR enhancement mode">
        {#each hdrModeOptions as option (option.value)}
          <div
            class="hdr-mode-option-wrap"
            class:rtx-mode-active={option.value === 'rtx' && hdrMode === 'rtx' && rtxHdrFlags !== null}>
            <button
              type="button"
              class="hdr-mode-option"
              class:active={hdrMode === option.value}
              class:conflict={hdrMode === 'conflict'}
              disabled={!hdrAvailable || !hdrModeReady || hdrModeBusy}
              role="radio"
              aria-checked={hdrMode === option.value}
              onclick={() => selectHdrMode(option.value)}>
              <span class="hdr-mode-radio-dot" aria-hidden="true"></span>
              <span class="hdr-mode-option-copy">
                <span class="hdr-mode-option-title">
                  <span class="hdr-mode-option-title-text">{option.label}</span>
                  {#if option.value === 'rtx'}<VendorTag vendor="nvidia" />{/if}
                </span>
                <span class="hdr-mode-option-desc">{option.description}</span>
              </span>
            </button>
            {#if option.value === 'rtx' && hdrMode === 'rtx' && rtxHdrFlags !== null}
              <span class="rtx-hdr-inline-select" bind:this={rtxHdrWrapEl}>
                <span class="rtx-hdr-trigger-wrap">
                  {#if rtxHdrFlagsApplied && !rtxHdrFlagsBusy}
                    <span class="hdr-applied-msg rtx-hdr-applied-msg">Applied</span>
                  {/if}
                  <button
                    type="button"
                    class="rtx-hdr-trigger"
                    aria-haspopup="listbox"
                    aria-expanded={rtxHdrMenuOpen}
                    disabled={rtxHdrFlagsBusy || hdrModeBusy}
                    onclick={toggleRtxHdrMenu}
                    onkeydown={rtxHdrKeyboard.onTriggerKeydown}>
                    <span>{rtxHdrLabel}</span>
                    <span class="rtx-hdr-caret" aria-hidden="true"></span>
                  </button>
                </span>
                {#if rtxHdrMenuOpen}
                  <div
                    class="dd-menu rtx-hdr-menu dropdown-menu-position-{rtxHdrMenuPlacement}"
                    bind:this={rtxHdrMenuEl}
                    role="listbox"
                    aria-label="RTX HDR mode options"
                    tabindex="-1"
                    onkeydown={rtxHdrKeyboard.onMenuKeydown}>
                    {#each RTX_HDR_OPTIONS as rtxOption, index (rtxOption.value)}
                      <button
                        type="button"
                        class="dd-menu-item"
                        class:active={rtxHdrFlags === rtxOption.value}
                        class:keyboard-highlighted={rtxHdrHighlightIndex === index}
                        data-dropdown-option-index={index}
                        role="option"
                        aria-selected={rtxHdrFlags === rtxOption.value}
                        onfocus={() => rtxHdrKeyboard.onItemFocus(index)}
                        onkeydown={rtxHdrKeyboard.onItemKeydown}
                        onclick={() => selectRtxHdrFlags(rtxOption.value)}>
                        {rtxOption.label}
                      </button>
                    {/each}
                  </div>
                {/if}
              </span>
            {/if}
          </div>
        {/each}
      </div>
      {#if hdrModeApplied && !hdrModeBusy && hdrMode !== 'conflict'}
        <p class="hdr-applied-msg">Applied</p>
      {/if}
      {#if hdrMode === 'conflict' && !hdrModeBusy}
        <p class="gpu-error">
          {hdrConflictResolveAt === 0
            ? 'Both HDR enhancement modes are enabled. Switching to NVIDIA RTX HDR…'
            : 'Both HDR enhancement modes are enabled. Auto-switching to NVIDIA RTX HDR failed. Choose an option to resolve the conflict.'}
        </p>
      {/if}
      {#if hdrModeError}
        <p class="gpu-error">{hdrModeError}</p>
      {:else if autoHdrError}
        <p class="gpu-error">{autoHdrError}</p>
      {:else if rtxHdrError}
        <p class="gpu-error">{rtxHdrError}</p>
      {/if}
    </div>
  </div>

  {#if !hdrAvailable}
    <p class="gpu-error">Enable Windows HDR to use these controls.</p>
  {/if}
  {#if error}
    <p class="gpu-error">{error}</p>
  {/if}
</div>
