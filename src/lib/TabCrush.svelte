<script>
  import { app, crush, adjustmentDisplay, blackLift, saveConfig } from './stores.svelte.js';
  // Runtime console output is mirrored to the in-app Debug log and Rust file
  // log by main.js. Keep GPU messages on that single path so they are not
  // duplicated in either destination.
  const dlog = (msg) => { console.log('[gpu]', msg); };
  const dwarn = (msg) => { console.warn('[gpu]', msg); };
  const derr = (msg) => { console.error('[gpu]', msg); };
  import { MAX_SQUARES, ALL_PQ_CODES, ALL_SDR_CODES, GAP_FRACTION, computeHdrValues, computeSdrValues } from './calibration.js';
  import { invoke } from '@tauri-apps/api/core';
  import { onDestroy, untrack } from 'svelte';

  let canvas = $state(null);
  let device = $state(null);
  let ctx = $state(null);
  let ub = $state(null);
  let bg = $state(null);
  let pipe = $state(null);
  let ready = $state(false);
  let gpuError = $state('');
  let pipelineLayout = null;
  let shaderModule = null;
  let currentFormat = null;
  // If the GPU rejects the HDR (rgba16float) format, we render in SDR locally
  // without touching app.hdrStatus — that's the shared source of truth also
  // written by App.svelte's poll timer, and writing to it here caused an
  // infinite render ↔ fallback ↔ poll loop (Svelte's max update depth error).
  let forceSdrRender = $state(false);

  const HDR_VALUES = computeHdrValues();
  const SDR_VALUES = computeSdrValues();

  const SHADER = `struct Cb{values:array<vec4<f32>,6>,counts:vec4<f32>}@group(0)@binding(0)var<uniform>cb:Cb;
struct VO{@builtin(position)p:vec4f,@location(0)uv:vec2f}
@vertex fn vs(@builtin(vertex_index)i:u32)->VO{
  let pos=array(vec2f(-1,1),vec2f(3,1),vec2f(-1,-3));
  let p=pos[i];
  return VO(vec4f(p,0,1),p*vec2f(.5,-.5)+vec2f(.5,.5));
}
@fragment fn fs(vin:VO)->@location(0)vec4f{
  let sq=i32(cb.counts.x);let gf=cb.counts.w;
  let sw=(1-gf*f32(sq-1))/f32(sq);let cw=sw+gf;
  let col=i32(floor(vin.uv.x/cw));let xf=fract(vin.uv.x/cw)*cw;
  if(xf>sw||col>=sq){return vec4f(0);}
  let g=col/4;return vec4f(cb.values[g][col%4]);
}`;

  let sliderDragging = $state(false);
  let sliderHovering = $state(false);
  let fsSliderDragging = $state(false);
  let fsSliderHovering = $state(false);
  let mainSliderInput = $state(null);
  let fsSliderInput = $state(null);
  let zoomSliderInput = $state(null);

  $effect(() => {
    if (!sliderDragging && !fsSliderDragging) return;
    function onUp() { sliderDragging = false; fsSliderDragging = false; }
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pointercancel', onUp);
    return () => {
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
    };
  });

  let initStarted = false;
  let gpuGeneration = 0;

  function releaseGpuResources() {
    gpuGeneration++;
    initStarted = true;

    if (resizeRaf) {
      cancelAnimationFrame(resizeRaf);
      resizeRaf = 0;
    }
    resizeObs?.disconnect();
    resizeObs = null;
    device?.destroy();
    device = null;
    ctx = null;
    ub = null;
    bg = null;
    pipe = null;
    pipelineLayout = null;
    shaderModule = null;
    currentFormat = null;
    ready = false;
  }

  onDestroy(releaseGpuResources);

  async function initGPU() {
    if (initStarted) { dwarn('initGPU called again while already running/ran — skipping'); return; }
    initStarted = true;
    const generation = gpuGeneration;
    try {
      dlog(`navigator.gpu present: ${!!navigator.gpu}`);
      if (!navigator.gpu) { gpuError = 'WebGPU not available'; derr('FAIL: WebGPU not available'); return; }

      // Browser env info
      dlog(`userAgent: ${navigator.userAgent}`);
      try {
        const mq = window.matchMedia('(dynamic-range: high)');
        dlog(`matchMedia (dynamic-range: high) = ${mq.matches}`);
      } catch (e) { dwarn(`matchMedia threw: ${e.message}`); }

      dlog('requesting GPU adapter...');
      const adapter = await navigator.gpu.requestAdapter();
      if (generation !== gpuGeneration) return;
      if (!adapter) { gpuError = 'No GPU adapter'; derr('FAIL: No GPU adapter returned'); return; }
      dlog('GPU adapter acquired');

      // Adapter info (may not exist in older WebView2)
      try {
        if (adapter.info) {
          dlog(`adapter.info: vendor=${adapter.info.vendor || '?'} device=${adapter.info.device || '?'} arch=${adapter.info.architecture || '?'}`);
        } else if (adapter.requestAdapterInfo) {
          const info = await adapter.requestAdapterInfo();
          dlog(`requestAdapterInfo: vendor=${info.vendor || '?'} device=${info.device || '?'} arch=${info.architecture || '?'}`);
        } else {
          dwarn('adapter.info / requestAdapterInfo not available (older WebView2)');
        }
      } catch (e) { dwarn(`adapter info query failed: ${e.message}`); }

      // Feature set
      try {
        const feat = adapter.features;
        if (feat) {
          const names = [];
          for (const f of feat) { if (names.length < 20) names.push(f); }
          dlog(`adapter features (${feat.size} total): ${names.join(', ')}`);
        }
      } catch (e) { dwarn(`adapter.features threw: ${e.message}`); }

      dlog('requesting device...');
      device = await adapter.requestDevice();
      if (generation !== gpuGeneration) {
        device.destroy();
        device = null;
        return;
      }
      dlog('GPU device created');

      ctx = canvas.getContext('webgpu');
      dlog('webgpu context obtained');

      // HDR status already set by pollHDRStatus (matchMedia) in App.svelte.
      // Use it to pick the best format; fall back if the GPU rejects it.
      const wantHdr = app.hdrStatus === 'hdr' && !forceSdrRender;
      let format = wantHdr ? 'rgba16float' : 'rgba8unorm';
      let colorSpace = wantHdr ? 'display-p3' : 'srgb';
      dlog(`configure: format=${format} colorSpace=${colorSpace} (hdrStatus=${app.hdrStatus})`);
      try {
        ctx.configure({ device, format, usage: GPUTextureUsage.RENDER_ATTACHMENT, colorSpace });
        dlog('configure SUCCESS');
      } catch (e) {
        dwarn(`configure FAILED: ${e.message}`);
        // Fall back to safe SDR format regardless of what matchMedia said
        format = 'rgba8unorm';
        colorSpace = 'srgb';
        dlog(`configure fallback: format=${format} colorSpace=${colorSpace}`);
        try {
          ctx.configure({ device, format, usage: GPUTextureUsage.RENDER_ATTACHMENT, colorSpace });
          dlog('fallback configure SUCCESS');
          if (wantHdr) { dwarn('matchMedia said HDR but GPU rejected rgba16float — using SDR locally'); forceSdrRender = true; }
        } catch (e2) {
          derr(`fallback configure FAILED: ${e2.message}`);
          throw e2;
        }
      }

      // After configure, try to read back the actual texture format
      try {
        const tex = ctx.getCurrentTexture();
        dlog(`getCurrentTexture format: ${tex.format}`);
      } catch (e) { dwarn(`getCurrentTexture threw: ${e.message}`); }

      dlog('creating uniform buffer + bind group...');
      ub = device.createBuffer({ size: 112, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
      const bgl = device.createBindGroupLayout({ entries: [{ binding: 0, visibility: GPUShaderStage.FRAGMENT, buffer: { type: 'uniform' } }] });
      bg = device.createBindGroup({ layout: bgl, entries: [{ binding: 0, resource: { buffer: ub } }] });
      const sh = device.createShaderModule({ code: SHADER });
      pipelineLayout = device.createPipelineLayout({ bindGroupLayouts: [bgl] });
      shaderModule = sh;
      currentFormat = format;
      pipe = device.createRenderPipeline({
        layout: pipelineLayout,
        vertex: { module: sh, entryPoint: 'vs' },
        fragment: { module: sh, entryPoint: 'fs', targets: [{ format }] },
        primitive: { topology: 'triangle-list' },
      });
      dlog(`pipeline created with target format: ${format}`);
      if (generation !== gpuGeneration) {
        releaseGpuResources();
        return;
      }
      ready = true;
      gpuError = '';
      dlog('GPU init complete — calling first render');
      render();
    } catch (e) {
      gpuError = 'GPU init failed: ' + e.message;
      forceSdrRender = true;
      derr(`UNHANDLED ERROR in initGPU: ${e.message}`);
      console.error('WebGPU init error:', e);
    }
  }

  // Reconfigures the canvas context and rebuilds the render pipeline for the
  // given format/colorSpace. Needed because the format picked at initGPU()
  // time was previously baked in forever — switching HDR↔SDR after launch
  // (e.g. toggling Windows HDR while the app is open) kept rendering into a
  // stale 8-bit SDR pipeline, silently clipping/crushing HDR values above 1.0
  // (the higher PQ code columns) instead of actually reconfiguring for HDR.
  function reconfigureForFormat(wantHdr) {
    if (!device || !ctx) return;
    let format = wantHdr ? 'rgba16float' : 'rgba8unorm';
    let colorSpace = wantHdr ? 'display-p3' : 'srgb';
    if (format === currentFormat) return;
    dlog(`reconfiguring: format=${format} colorSpace=${colorSpace} (hdrStatus=${app.hdrStatus})`);
    try {
      ctx.configure({ device, format, usage: GPUTextureUsage.RENDER_ATTACHMENT, colorSpace });
    } catch (e) {
      dwarn(`reconfigure FAILED: ${e.message} — falling back to SDR`);
      format = 'rgba8unorm';
      colorSpace = 'srgb';
      try {
        ctx.configure({ device, format, usage: GPUTextureUsage.RENDER_ATTACHMENT, colorSpace });
        if (wantHdr) forceSdrRender = true;
      } catch (e2) {
        derr(`reconfigure fallback FAILED: ${e2.message}`);
        return;
      }
    }
    pipe = device.createRenderPipeline({
      layout: pipelineLayout,
      vertex: { module: shaderModule, entryPoint: 'vs' },
      fragment: { module: shaderModule, entryPoint: 'fs', targets: [{ format }] },
      primitive: { topology: 'triangle-list' },
    });
    currentFormat = format;
    dlog(`pipeline rebuilt with target format: ${format}`);
  }

  function render() {
    if (!ready || !device || !canvas) return;
    try {
      const dpr = devicePixelRatio || 1;
      const clientWidth = canvas.clientWidth;
      const clientHeight = canvas.clientHeight;
      // The strip can briefly be zero-sized while its flex layout is settling
      // or while the tab is being switched. Never ask WebGPU for a 0x0 texture.
      if (clientWidth < 1 || clientHeight < 1) return;
      const w = Math.round(clientWidth * dpr);
      const h = Math.round(clientHeight * dpr);
      if (w < 1 || h < 1) return;
      if (w !== canvas.width || h !== canvas.height) { canvas.width = w; canvas.height = h; }
      const sq = crush.squareCount;
      const isHdr = app.hdrStatus === 'hdr' && !forceSdrRender;
      const vals = isHdr ? HDR_VALUES : SDR_VALUES;
      const data = new Float32Array(28);
      for (let i = 0; i < 24; i++) data[i] = i < sq ? vals[i] : 0;
      data[24] = sq; data[25] = sq; data[26] = 1; data[27] = GAP_FRACTION;
      device.queue.writeBuffer(ub, 0, data);

      let tex;
      try {
        tex = ctx.getCurrentTexture();
      } catch (e) {
        // Surface lost or unavailable — silently skip this frame
        return;
      }

      const enc = device.createCommandEncoder();
      const pass = enc.beginRenderPass({ colorAttachments: [{ view: tex.createView(), loadOp: 'clear', storeOp: 'store', clearValue: [0, 0, 0, 1] }] });
      pass.setPipeline(pipe); pass.setBindGroup(0, bg); pass.draw(3); pass.end();
      device.queue.submit([enc.finish()]);
      gpuError = '';
    } catch (e) {
      gpuError = 'Render error: ' + e.message;
    }
  }

  $effect(() => {
    console.log(`[gpu] init effect fired — canvas=${!!canvas} initStarted=${initStarted}`);
    if (canvas && !initStarted) initGPU();
  });
  $effect(() => {
    void crush.squareCount; void app.hdrStatus;
    if (ready) {
      reconfigureForFormat(app.hdrStatus === 'hdr' && !forceSdrRender);
      render();
    }
  });

  let resizeObs;
  let resizeRaf = 0;
  $effect(() => {
    if (!canvas) return;
    // Coalesce rapid-fire ResizeObserver callbacks into a single rAF-scheduled
    // render. Calling render() synchronously inside the observer callback let
    // canvas.width/height writes (which themselves trigger a resize) cascade
    // into the same microtask Svelte's effect scheduler watches, tripping
    // effect_update_depth_exceeded.
    resizeObs = new ResizeObserver(() => {
      if (resizeRaf) cancelAnimationFrame(resizeRaf);
      resizeRaf = requestAnimationFrame(() => {
        resizeRaf = 0;
        render();
      });
    });
    resizeObs.observe(canvas);
    return () => { resizeObs?.disconnect(); if (resizeRaf) cancelAnimationFrame(resizeRaf); };
  });

  $effect(() => {
    void crush.adjustment; void crush.blackEnabled;
    invoke('apply_gamma_ramp', { blackLift: blackLift() }).catch(() => {});
    saveConfig();
  });

  // Lets a drag start anywhere in the slider's expanded vertical buffer zone
  // (see .slider-track-wrap-half::before), not just on the thin native input.
  // Can't hand this off to the native input via a synthetic pointerdown —
  // browsers only start native thumb-drag tracking from a trusted event, so a
  // dispatched PointerEvent starts the drag visually but doesn't continue
  // tracking subsequent moves. Instead we drive the whole drag manually here:
  // compute value from cursor X on down and on every move, write it directly
  // to crush.adjustment, and stop on pointerup/cancel.
  function normalizeBlackLevel(value) {
    return (value !== 0 && Math.abs(value) <= 4) ? 0 : value;
  }

  function setBlackLevel(value) {
    crush.adjustment = normalizeBlackLevel(value);
  }

  function handleBlackLevelInput(e) {
    const value = +e.target.value;
    const normalized = normalizeBlackLevel(value);
    crush.adjustment = normalized;
    if (value !== normalized) e.target.value = normalized;
  }

  // Change a range by one native step for each wheel event while the pointer
  // is anywhere over its row. Use the sign only so mouse wheels and trackpads
  // both produce predictable one-step adjustments, then clamp to the range.
  function handleSliderWheel(e, inputEl, setValue, stepOverride = null) {
    if (!inputEl || inputEl.disabled || e.deltaY === 0) return;
    e.preventDefault();
    e.stopPropagation();
    const min = +inputEl.min;
    const max = +inputEl.max;
    const step = stepOverride ?? +(inputEl.step || 1);
    const current = +inputEl.value;
    const direction = e.deltaY < 0 ? 1 : -1;
    const rawNext = Math.min(max, Math.max(min, current + direction * step));
    let next = setValue === setBlackLevel ? normalizeBlackLevel(rawNext) : rawNext;
    // The black-level slider intentionally snaps values from -4..4 to zero.
    // Jump across that dead zone so the first wheel tick always changes it.
    if (next === current && current === 0) next = direction > 0 ? 5 : -5;
    setValue(next);
  }

  function handleTrackWrapPointerdown(e, inputEl) {
    if (e.target === inputEl) return; // already on the real input, let it handle itself
    e.preventDefault();
    const min = +inputEl.min, max = +inputEl.max;
    const applyFromClientX = (clientX) => {
      const rect = inputEl.getBoundingClientRect();
      const frac = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
      const raw = Math.round(min + frac * (max - min));
      crush.adjustment = (raw !== 0 && Math.abs(raw) <= 4) ? 0 : raw;
    };
    applyFromClientX(e.clientX);
    inputEl.focus();
    sliderDragging = true;
    function onMove(ev) { applyFromClientX(ev.clientX); }
    function onUp() {
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
    }
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pointercancel', onUp);
  }

  function cycleFullscreen() {
    if (crush.fullscreen === 'normal') crush.fullscreen = 'inApp';
    else if (crush.fullscreen === 'inApp') crush.fullscreen = 'monitor';
    else crush.fullscreen = 'normal';
  }
  // Native dblclick is unreliable here: entering fullscreen mid-gesture
  // repositions/resizes .strip-wrap (relative -> fixed) under the cursor,
  // which can make the browser fail to associate the next click as part of
  // a new double-click for a period. Track click timestamps manually instead
  // so double-click detection survives the layout change between clicks.
  let lastClickTime = 0;
  function handleStripClick(e) {
    const now = Date.now();
    if (now - lastClickTime < 400) {
      lastClickTime = 0;
      cycleFullscreen();
    } else {
      lastClickTime = now;
    }
  }
  function exitFullscreen() {
    crush.fullscreen = 'normal';
    if (document.fullscreenElement) document.exitFullscreen();
  }
  function setMode(mode) {
    crush.fullscreen = mode;
    closeContextMenu();
  }

  // Mouse wheel over the strip changes the zoom (square count). Wheeling up
  // (negative deltaY) increases zoom, down decreases — matches typical
  // scroll-to-zoom conventions (maps, image viewers).
  function handleStripWheel(e) {
    e.preventDefault();
    const dir = e.deltaY < 0 ? 1 : -1;
    crush.squareCount = Math.min(MAX_SQUARES, Math.max(2, crush.squareCount + dir));
  }

  // ── Right-click context menu on the strip ──────────────────────────
  let ctxMenu = $state({ open: false, x: 0, y: 0 });
  function openContextMenu(e) {
    e.preventDefault();
    const menuW = 260, menuH = 210;
    const x = Math.min(e.clientX, window.innerWidth - menuW - 8);
    const y = Math.min(e.clientY, window.innerHeight - menuH - 8);
    ctxMenu = { open: true, x, y };
  }
  function closeContextMenu() {
    ctxMenu = { ...ctxMenu, open: false };
  }
  let lastFullscreenMode = crush.fullscreen;
  $effect(() => {
    const mode = crush.fullscreen;
    if (mode !== lastFullscreenMode) {
      lastFullscreenMode = mode;
      untrack(() => { if (ctxMenu.open) closeContextMenu(); });
    }
  });
  $effect(() => {
    if (crush.fullscreen === 'monitor') {
      if (!document.fullscreenElement) {
        document.documentElement.requestFullscreen().catch((e) => {
          // Browser rejected the request (e.g. rapid repeated calls without a
          // fresh user gesture, or a request already in flight). If we're
          // still supposed to be in monitor mode, retry shortly — otherwise
          // rapid clicking can leave crush.fullscreen==='monitor' while the
          // real fullscreenElement never gets set, permanently desyncing state.
          dwarn(`requestFullscreen failed: ${e.message}`);
          if (crush.fullscreen === 'monitor' && !document.fullscreenElement) {
            setTimeout(() => {
              if (crush.fullscreen === 'monitor' && !document.fullscreenElement) {
                document.documentElement.requestFullscreen().catch(() => {});
              }
            }, 150);
          }
        });
      }
    } else if (document.fullscreenElement) {
      document.exitFullscreen();
    }
  });
  $effect(() => {
    function onFSChange() {
      if (!document.fullscreenElement && crush.fullscreen === 'monitor') crush.fullscreen = 'normal';
    }
    document.addEventListener('fullscreenchange', onFSChange);
    return () => document.removeEventListener('fullscreenchange', onFSChange);
  });

  $effect(() => {
    if (!ctxMenu.open) return;
    function onDocClick() { closeContextMenu(); }
    function onDocKey(e) { if (e.key === 'Escape') closeContextMenu(); }
    // Listen on capture + next tick so the opening right-click doesn't immediately close it.
    const id = setTimeout(() => {
      document.addEventListener('click', onDocClick);
      document.addEventListener('contextmenu', onDocClick);
      document.addEventListener('keydown', onDocKey);
    }, 0);
    return () => {
      clearTimeout(id);
      document.removeEventListener('click', onDocClick);
      document.removeEventListener('contextmenu', onDocClick);
      document.removeEventListener('keydown', onDocKey);
    };
  });

  function restoreDefaults() {
    crush.adjustment = 0;
    crush.blackEnabled = true;
    crush.squareCount = 12;
    crush.fullscreen = 'normal';
    saveConfig();
  }

  function onKeydown(e) {
    if (app.activeTab !== 'crush') return;
    if (e.key === 'Escape') { if (crush.fullscreen !== 'normal') { exitFullscreen(); return; } }
    if (!crush.blackEnabled) return;
    // crush.adjustment is stored in tenths (adjustmentDisplay() divides by 10),
    // so Left/Right move a whole point (10) and Up/Down move 0.1 (1).
    if (e.key === 'ArrowRight') { crush.adjustment = Math.min(150, crush.adjustment + 10); }
    if (e.key === 'ArrowLeft') { crush.adjustment = Math.max(-150, crush.adjustment - 10); }
    if (e.key === 'ArrowUp') { e.preventDefault(); crush.adjustment = Math.min(150, crush.adjustment + 1); }
    if (e.key === 'ArrowDown') { e.preventDefault(); crush.adjustment = Math.max(-150, crush.adjustment - 1); }
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="tab-body">
  <div class="tab-heading">
<img class="tab-icon" src="/icons/tab_crush.png" alt="" />
    <h2>Black Crush Tweak</h2>
    <div class="tab-heading-spacer"></div>
    <button class="restore-defaults-btn" onclick={restoreDefaults}>↻ Restore defaults</button>
  </div>
  <div class="separator"></div>

  <div class="settings-row dimmer-toggle-row" role="button" tabindex="0" onclick={() => crush.blackEnabled = !crush.blackEnabled} onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); crush.blackEnabled = !crush.blackEnabled; } }}>
    <div class="settings-row-text">
      <span class="settings-row-label">Enable Black Level</span>
      <span class="settings-row-desc">Adjust black levels using a Reinhard gamma curve. Preserves pure black.</span>
    </div>
    <div class="pill-wrap">
      <span class={`pill${crush.blackEnabled ? '' : ' off'}`} aria-hidden="true"></span>
    </div>
  </div>

  <div class="dimmer-control" class:disabled={!crush.blackEnabled} onwheel={(e) => handleSliderWheel(e, mainSliderInput, setBlackLevel, 5)}>
    <div class="dimmer-control-heading">
      <span class="settings-row-label">Black Level</span>
      <span class="slider-val">{adjustmentDisplay()}</span>
    </div>
    <div class="slider-track-wrap dimmer-slider-track" onpointerdown={(e) => handleTrackWrapPointerdown(e, mainSliderInput)}>
      <input type="range" min="-150" max="150" bind:value={crush.adjustment} bind:this={mainSliderInput}
        class="has-ticks"
        style="--fill:{((crush.adjustment + 150) / 300) * 100}%"
        oninput={handleBlackLevelInput}
        onpointerdown={() => sliderDragging = true}
        onpointerup={() => sliderDragging = false}
        onpointerenter={() => sliderHovering = true}
        onpointerleave={() => sliderHovering = false}
        disabled={!crush.blackEnabled} />
      <div class="slider-tooltip" class:visible={sliderDragging || sliderHovering} style="left:calc(9px + ({((crush.adjustment + 150) / 300)} * (100% - 18px)))">{adjustmentDisplay()}</div>
    </div>
    <div class="dimmer-scale"><span>Darker</span><span>Brighter</span></div>
  </div>

  <div class="separator"></div>

  {#snippet calibrationTip()}
    <span>Adjust black level until you can start to see the lower columns and still have a smooth gradient.</span>
    <span class="info-tip-note">
      <svg viewBox="0 0 16 16" fill="none"><circle cx="8" cy="8" r="6.5" stroke="currentColor" stroke-width="1.3"/><path d="M8 7.2V11.3" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><circle cx="8" cy="4.9" r="0.9" fill="currentColor"/></svg>
      <span><span class="info-tip-strong">{app.hdrStatus === 'hdr' ? '64' : '0'}</span> column is pure black.</span>
    </span>
    <div class="fs-hint-sep"></div>
    <div class="fs-hint-title">Controls</div>
    {#if crush.fullscreen !== 'normal'}
      <div class="fs-hint-row"><span class="fs-hint-key">Esc</span><span class="fs-hint-val">Exit</span></div>
    {/if}
    <div class="fs-hint-row"><span class="fs-hint-key">Left / Right</span><span class="fs-hint-val">±1</span></div>
    <div class="fs-hint-row"><span class="fs-hint-key">Up / Down</span><span class="fs-hint-val">±0.1</span></div>
    <div class="fs-hint-sep"></div>
    <div class="fs-hint-row"><span class="fs-hint-key">Double-click</span><span class="fs-hint-val">Cycle mode</span></div>
    <div class="fs-hint-row"><span class="fs-hint-key">Right-click</span><span class="fs-hint-val">Menu</span></div>
    <div class="fs-hint-row"><span class="fs-hint-key">Scroll</span><span class="fs-hint-val">Zoom</span></div>
  {/snippet}

  <div class="calibration-strip-panel">
    {#if app.hdrStatus === 'unknown'}
      <p class="gpu-status">Detecting HDR status...</p>
    {:else}
      <p class="strip-heading">
        <strong>{app.hdrStatus === 'hdr' ? 'HDR Black Calibration (PQ)' : 'SDR Black Calibration (RGB)'}</strong>
        <span class="info-tip-wrap">
          <button class="info-tip-btn" aria-label="About black calibration and strip controls" tabindex="0">Info</button>
          <span class="info-tip">{@render calibrationTip()}</span>
        </span>
      </p>
    {/if}

    {#if gpuError}
      <p class="gpu-error">{gpuError}</p>
    {/if}

    <div class="strip-wrap" class:monitor-fullscreen={crush.fullscreen === 'monitor'} class:inapp-fullscreen={crush.fullscreen === 'inApp'} style={crush.fullscreen !== 'normal' ? 'position:fixed;inset:0;z-index:9999;border-radius:0;margin:0;' : ''}
      onclick={handleStripClick} oncontextmenu={openContextMenu} onwheel={handleStripWheel} role="button" tabindex="0">
      <canvas bind:this={canvas}></canvas>
      {#if app.hdrStatus !== 'unknown'}
        <div class="square-labels" style="--sq-count:{crush.squareCount}">
          {#each Array(crush.squareCount) as _, i}
            {@const code = app.hdrStatus === 'hdr'
              ? (i === 0 ? '64' : String(ALL_PQ_CODES[i]))
              : (i === 0 ? '0' : String(ALL_SDR_CODES[i]))}
            {@const unit = i === 0 ? 'Black' : (app.hdrStatus === 'hdr' ? 'PQ' : 'RGB')}
            <div class="square-label">
              <div>{code}</div>
              <div class="square-unit">{unit}</div>
            </div>
          {/each}
        </div>
      {/if}
      {#if crush.fullscreen !== 'normal'}
        <div class="fs-help-wrap">
          <button class="fs-help-btn" aria-label="About black calibration and strip controls" tabindex="0">Info</button>
          <div class="fs-help-tip">{@render calibrationTip()}</div>
        </div>
      {/if}
      {#if crush.fullscreen !== 'normal'}
        <div class="fs-black-slider" onwheel={(e) => handleSliderWheel(e, fsSliderInput, setBlackLevel, 5)}>
          <div class="slider-track-wrap">
            <input type="range" min="-150" max="150" bind:value={crush.adjustment} bind:this={fsSliderInput}
              class="has-ticks"
              style="--fill:{((crush.adjustment + 150) / 300) * 100}%"
              oninput={handleBlackLevelInput}
              onpointerdown={() => fsSliderDragging = true}
              onpointerup={() => fsSliderDragging = false}
              onpointerenter={() => fsSliderHovering = true}
              onpointerleave={() => fsSliderHovering = false}
              disabled={!crush.blackEnabled} />
            <div class="slider-tooltip" class:visible={fsSliderDragging || fsSliderHovering} style="left:calc(9px + ({((crush.adjustment + 150) / 300)} * (100% - 18px)))">{adjustmentDisplay()}</div>
          </div>
          <span class="slider-val">{adjustmentDisplay()}</span>
        </div>
      {/if}
    </div>

    <div class="zoom-control-compact" onwheel={(e) => handleSliderWheel(e, zoomSliderInput, (value) => crush.squareCount = value)}>
      <img class="slider-label-icon" src="/icons/zoom.png" alt="Zoom" />
      <div class="slider-track-wrap zoom-track-compact">
        <input type="range" min="2" max={MAX_SQUARES} bind:value={crush.squareCount} bind:this={zoomSliderInput}
          style="--fill:{((crush.squareCount - 2) / (MAX_SQUARES - 2)) * 100}%" />
      </div>
      <span class="slider-val zoom-val-compact">{crush.squareCount}</span>
    </div>
  </div>

  {#if ctxMenu.open}
    <div class="ctx-menu" style="left:{ctxMenu.x}px; top:{ctxMenu.y}px;" onclick={(e) => e.stopPropagation()} oncontextmenu={(e) => e.preventDefault()}>
      <div class="ctx-menu-label">
        Calibration Display Mode
      </div>
      <button class="ctx-item" class:active={crush.fullscreen === 'inApp'} onclick={() => setMode('inApp')}>In-App Fullscreen</button>
      <button class="ctx-item" class:active={crush.fullscreen === 'monitor'} onclick={() => setMode('monitor')}>Monitor Fullscreen</button>
      <div class="ctx-menu-sep"></div>
      <button class="ctx-item" onclick={exitFullscreen} disabled={crush.fullscreen === 'normal'}>Exit Fullscreen</button>
    </div>
  {/if}
</div>
