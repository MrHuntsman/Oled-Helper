<script>
  // Inlined data URIs — see App.svelte for why icons never hit the network.
  import tabCrushIcon from './icons/tab_crush.png?inline';
  import zoomIcon from './icons/zoom.png?inline';
  import { app, crush, adjustmentDisplay, blackLift, saveConfig, setHideCalibrationInterface, setBlackStripGapEnabled, setStripType, activeSquareCount, ctxMenuDismissTick, highNitsMaxValue, setHighNitsMax } from './stores.svelte.js';
  // Runtime console output is mirrored to the in-app Debug log and Rust file
  // log by main.js. Keep GPU messages on that single path so they are not
  // duplicated in either destination.
  const dlog = (msg) => { console.log('[gpu]', msg); };
  const dwarn = (msg) => { console.warn('[gpu]', msg); };
  const derr = (msg) => { console.error('[gpu]', msg); };
  import { MAX_SQUARES, ALL_PQ_CODES, ALL_SDR_CODES, GAP_FRACTION, HIGH_NITS_GAP_FRACTION, STRIPS, HIGH_SDR_RANGE, HIGH_NITS_CELLS, HIGH_NITS_COLS, HIGH_NITS_ROWS, HIGH_NITS_DEFAULT_MIN, HIGH_NITS_DEFAULT_STEP, HIGH_NITS_DEFAULT_MAX, HIGH_NITS_MAX_STOPS, highNitsGridCodes, highNitsSdrCodes, computeHdrValues, computeSdrValues, computeHighNitsValues, CELL_GRID_SHADER } from './calibration.js';
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

  // Heading title + square label helpers, driven by the active strip. The
  // High Nits strip shows a windowed nits ladder (HDR) or bright RGB range
  // (SDR); the black strip keeps the existing PQ/RGB code labels.
  function stripTitle() {
    const isHdr = app.hdrStatus === 'hdr';
    if (crush.stripType === 'highNits') {
      return isHdr ? 'HDR Brightness' : 'SDR Brightness';
    }
    return isHdr ? 'HDR Black Calibration (PQ)' : 'SDR Black Calibration (RGB)';
  }

  function stripLabels() {
    const isHdr = app.hdrStatus === 'hdr' && !forceSdrRender;
    const sq = activeSquareCount();
    if (crush.stripType === 'highNits') {
      // Fixed 3×7 grid (21 cells) in normal mode, no zoom; in fullscreen the
      // grid grows to the aspect-fit cell count (up to 40) so every slot has
      // a value. HDR shows region-stepped round nits values ending exactly
      // at the user's Max (1000-step above 5000, 500-step from 1000–5000,
      // 100-step below, finer when needed); SDR shows the bright RGB range
      // sampled over the same number of cells. Only HDR cells are true nits
      // values, so the 2000-nits outline threshold below never applies to
      // SDR.
      if (isHdr) {
        return highNitsGridCodes(crush.highNitsMax, stripGeom.cells).map((code) => ({
          code: String(code),
          unit: 'nits',
          bright: code >= 2000,
        }));
      }
      return highNitsSdrCodes(stripGeom.cells).map((code) => ({ code: String(code), unit: 'RGB', bright: false }));
    }
    const codes = isHdr ? ALL_PQ_CODES : ALL_SDR_CODES;
    return codes.slice(0, sq).map((code, i) => ({
      code: String(code),
      unit: i === 0 ? 'Black' : (isHdr ? 'PQ' : 'RGB'),
    }));
  }

  function labelCount() {
    if (crush.stripType === 'highNits') return stripGeom.cells;
    return activeSquareCount();
  }

  // Grid geometry for the current strip, matching the shader exactly
  // (cells, cols, rows). Black = one row of sq cells; high-nits = the fixed
  // 3×7 spreadsheet grid (7 columns across, 3 rows down — 21 cells) in
  // normal mode. In fullscreen the canvas fills the whole screen, so the
  // fixed 3×7 would stretch the cells into rectangles on a 16:9 (or wider)
  // panel — instead the grid is recomputed from the canvas aspect ratio to
  // keep every cell as close to a square as possible. The grid is also
  // allowed to grow past 21 cells so every slot shows a real nits value
  // (no empty black cells): the value generators accept the cell count.
  // Initialized from computeStripGeom() (hoisted below) so the very first
  // frame already matches the active strip — stripLabels() derives its
  // label count from this, and a stale 12-cell default would briefly
  // misalign the high-nits labels before the render effect corrects it.
  let stripGeom = $state(computeStripGeom());
  // Upper bound for the fullscreen high-nits grid. Searching without a cap
  // would pick absurdly dense layouts for aspect ratios that divide exactly
  // (16:9 could become 16×9 = 144 cells); 40 keeps cells comfortably large
  // and readable while still fitting any common aspect ratio within ~5% of
  // square.
  const HIGH_NITS_MAX_GRID_CELLS = 40;
  // Best-fit square-ish grid on a canvas of the given aspect ratio: search
  // every rows×cols layout whose cell count is between the fixed 21 and the
  // 40-cell cap, keep the layout whose cells come closest to 1:1 (width vs
  // height). Every slot gets a real value — the value generators accept the
  // returned cell count, so nothing renders black.
  function bestFitGrid(canvasW, canvasH) {
    if (canvasW < 1 || canvasH < 1) return { cells: HIGH_NITS_CELLS, cols: HIGH_NITS_COLS, rows: HIGH_NITS_ROWS };
    let best = null;
    let bestDiff = Infinity;
    for (let rows = 1; rows <= HIGH_NITS_MAX_GRID_CELLS; rows++) {
      for (let cols = 1; cols <= HIGH_NITS_MAX_GRID_CELLS; cols++) {
        const cells = rows * cols;
        if (cells < HIGH_NITS_CELLS || cells > HIGH_NITS_MAX_GRID_CELLS) continue;
        const cellW = canvasW / cols;
        const cellH = canvasH / rows;
        const diff = Math.abs(cellW - cellH);
        if (diff < bestDiff) { bestDiff = diff; best = { cells, cols, rows }; }
      }
    }
    return best ?? { cells: HIGH_NITS_CELLS, cols: HIGH_NITS_COLS, rows: HIGH_NITS_ROWS };
  }
  function computeStripGeom() {
    if (crush.stripType === 'highNits') {
      if (crush.fullscreen !== 'normal') {
        // Fullscreen: derive cols/rows from the live canvas aspect ratio so
        // cells stay ~square instead of stretching with the screen, growing
        // past 21 cells as needed so no slot is left empty.
        return bestFitGrid(canvas?.clientWidth || 0, canvas?.clientHeight || 0);
      }
      return { cells: HIGH_NITS_CELLS, cols: HIGH_NITS_COLS, rows: HIGH_NITS_ROWS };
    }
    const sq = activeSquareCount();
    return { cells: sq, cols: sq, rows: 1 };
  }
  function refreshStripGeom() { stripGeom = computeStripGeom(); }

  // Uniform layout: values[48] cell colors, then counts = (cells, cols,
  // rows, gap). Cells are laid out row-major (left→right, top→bottom) like a
  // spreadsheet. The black strip is a single row (rows=1, cols=sq); the
  // high-nits strip is a rows×cols grid. 48 slots give headroom for the
  // fullscreen aspect-fit grid, which can grow past the fixed 21 cells.
  const SHADER = CELL_GRID_SHADER;

  let sliderDragging = $state(false);
  let sliderHovering = $state(false);
  let blackMasterAttention = $state(false);
  let blackMasterAttentionTimer = null;
  let blackMasterAttentionRaf = 0;
  let fsSliderDragging = $state(false);
  let fsSliderHovering = $state(false);
  let mainSliderInput = $state(null);
  let fsSliderInput = $state(null);
  let zoomSliderInput = $state(null);

  $effect(() => {
    if (!sliderDragging && !fsSliderDragging) return;
    function onUp() {
      sliderDragging = false;
      fsSliderDragging = false;
      // Persist exactly once, when the drag ends (mouse button released) —
      // never mid-drag, so a paused drag can't trigger a config write while
      // the user is still adjusting.
      scheduleConfigSave();
    }
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
      ub = device.createBuffer({ size: 208, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
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
      device.lost.then((info) => {
        if (generation !== gpuGeneration) return;
        const reason = info?.message || 'GPU device was lost';
        gpuError = `GPU device lost: ${reason}`;
        derr(`GPU device lost: ${reason}`);
        releaseGpuResources();
      }).catch(() => {});
    } catch (e) {
      gpuError = 'GPU init failed: ' + (e?.message || String(e));
      forceSdrRender = true;
      derr(`UNHANDLED ERROR in initGPU: ${e?.message || String(e)}`);
      console.error('WebGPU init error:', e);
      releaseGpuResources();
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
    try {
      pipe = device.createRenderPipeline({
        layout: pipelineLayout,
        vertex: { module: shaderModule, entryPoint: 'vs' },
        fragment: { module: shaderModule, entryPoint: 'fs', targets: [{ format }] },
        primitive: { topology: 'triangle-list' },
      });
      currentFormat = format;
      dlog(`pipeline rebuilt with target format: ${format}`);
    } catch (error) {
      gpuError = `GPU pipeline rebuild failed: ${error?.message || String(error)}`;
      derr(`GPU pipeline rebuild failed: ${error?.message || String(error)}`);
      releaseGpuResources();
    }
  }

  function render() {
    if (!ready || !device || !canvas) return;
    try {
      // Keep the shader's grid geometry and the DOM label grid in sync — the
      // aspect ratio (and therefore cols/rows) can change on every resize.
      //
      // untrack: refreshStripGeom() writes the stripGeom state and render()
      // reads it below (high-nits branch). Both run inside the $effect that
      // drives rendering, so without untrack the write registers that effect
      // as a dependent of stripGeom → it re-runs → writes again (a fresh
      // object each time) → effect_update_depth_exceeded. This only hit the
      // high-nits strip because its branch actually reads stripGeom (the
      // black strip builds grid inline and never tracked it).
      untrack(() => refreshStripGeom());
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
      const sq = activeSquareCount();
      const isHdr = app.hdrStatus === 'hdr' && !forceSdrRender;
      const highNits = crush.stripType === 'highNits';
      // High Nits: render the aspect-fit grid values (21 cells in normal
      // mode, up to 40 in fullscreen — one value per slot, no empty cells),
      // laid out row-major across a spreadsheet grid.
      let vals, grid;
      if (highNits) {
        grid = untrack(() => stripGeom);
        vals = computeHighNitsValues(crush.highNitsMax, isHdr, grid.cells);
      } else {
        vals = isHdr ? HDR_VALUES : SDR_VALUES;
        grid = { cells: sq, cols: sq, rows: 1 };
      }
      const data = new Float32Array(52);
      for (let i = 0; i < 48; i++) data[i] = i < grid.cells ? vals[i] : 0;
      data[48] = grid.cells;
      data[49] = grid.cols;
      data[50] = grid.rows;
      data[51] = highNits ? HIGH_NITS_GAP_FRACTION : (crush.blackStripGapEnabled ? GAP_FRACTION : 0);
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
    void crush.squareCounts; void crush.stripType; void app.hdrStatus; void crush.blackStripGapEnabled; void crush.highNitsMin; void crush.highNitsStep; void canvas;
    // untrack the geometry write: stripGeom is a $state read by the template
    // and by render() (high-nits grid). Writing it from inside this effect —
    // whose render() call reads it — would re-schedule the effect on every
    // run, tripping effect_update_depth_exceeded. The template still updates
    // because untracked writes still notify its reactive dependents.
    untrack(() => refreshStripGeom());
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
        refreshStripGeom();
        render();
      });
    });
    resizeObs.observe(canvas);
    return () => { resizeObs?.disconnect(); if (resizeRaf) cancelAnimationFrame(resizeRaf); };
  });

  // Gamma updates are applied synchronously on every slider change — the
  // Tauri command builds the ramp and calls SetDeviceGammaRamp immediately.
  // A background safety thread re-applies every 2s to recover from external
  // gamma resets (fullscreen games, driver mode switches).
  let gammaRampDestroyed = false;
  let gammaRampLastValue = null;
  let gammaReferenceModeLast = false;

  // Send the latest value to the Rust worker. Returns immediately; the worker
  // picks up the newest value on its own loop, so even a slow invoke response
  // (or no response at all) cannot delay the display update.
  function requestGammaRamp() {
    if (gammaRampDestroyed) return;
    const value = blackLift();
    if (app.displayInfo.referenceMode) {
      // NVIDIA Reference Mode/Color Accuracy Mode makes GDI gamma-ramp
      // writes ineffective. Avoid repeatedly issuing writes while the driver
      // setting is active; the backend still owns the authoritative warning.
      return;
    }
    // Slider events frequently normalize to the same effective value (the
    // -4..4 dead zone snaps to 0, and trackpad/wheel drags repeat positions).
    // Skip redundant sends so a drag doesn't spam the IPC channel.
    if (value === gammaRampLastValue) return;
    gammaRampLastValue = value;
    invoke('apply_gamma_ramp', { blackLift: value })
      .catch((error) => console.warn(`[gamma] send value=${value} failed: ${String(error)}`));
  }

  let configSaveTimer = null;
  function scheduleConfigSave() {
    clearTimeout(configSaveTimer);
    configSaveTimer = setTimeout(() => {
      configSaveTimer = null;
      saveConfig();
    }, 150);
  }

  onDestroy(() => {
    gammaRampDestroyed = true;
    gammaRampLastValue = null;
    if (configSaveTimer) {
      clearTimeout(configSaveTimer);
      configSaveTimer = null;
      // Preserve the last slider value if the tab is destroyed before the
      // debounce window expires (for example during a rapid tab switch).
      saveConfig();
    }
    clearTimeout(blackMasterAttentionTimer);
    cancelAnimationFrame(blackMasterAttentionRaf);
  });

  $effect(() => {
    void crush.adjustment; void crush.blackEnabled; void app.displayInfo.referenceMode;
    if (app.displayInfo.referenceMode !== gammaReferenceModeLast) {
      // Allow the first write after Reference Mode is disabled. Otherwise the
      // same slider value would remain in gammaRampLastValue and no restore
      // write would be sent when the driver becomes writable again.
      gammaRampLastValue = null;
      gammaReferenceModeLast = app.displayInfo.referenceMode;
    }
    if (!crush.blackEnabled) {
      sliderDragging = false;
      fsSliderDragging = false;
      sliderHovering = false;
      fsSliderHovering = false;
    }
    requestGammaRamp();
    // No config save here: the adjustment changes on every pointer event
    // during a drag, and saving is now tied to discrete events only (drag
    // release, wheel, keyboard, toggle, restore defaults).
  });

  // The black level slider ticks are drawn at multiples of this many
  // adjustment units (see .slider-track-wrap input[type=range].has-ticks):
  // a -150..150 range divided into 6 equal steps. Grabbing the thumb or
  // clicking the track snaps to a tick ONLY when within this many units of
  // one — in RAW units. The displayed value is adjustment/10, so this 2 raw
  // is the same 0.2 the user asked for (and matches the ±2 dead zone at 0
  // in normalizeBlackLevel below). Anything farther stays where it is, so
  // the slider isn't forced onto every tick. Wheel and keyboard keep fine
  // control (see below).
  const BLACK_LEVEL_TICK_STEP = 50;
  const BLACK_LEVEL_TICK_SNAP_DIST = 2; // 0.2 on the displayed value

  function snapToBlackLevelTick(value) {
    const nearest = Math.round(value / BLACK_LEVEL_TICK_STEP) * BLACK_LEVEL_TICK_STEP;
    return Math.abs(value - nearest) <= BLACK_LEVEL_TICK_SNAP_DIST ? nearest : value;
  }

  // Lets a drag start anywhere in the slider's expanded vertical buffer zone
  // (see .slider-track-wrap-half::before), not just on the thin native input.
  // Can't hand this off to the native input via a synthetic pointerdown —
  // browsers only start native thumb-drag tracking from a trusted event, so a
  // dispatched PointerEvent starts the drag visually but doesn't continue
  // tracking subsequent moves. Instead we drive the whole drag manually here:
  // compute value from cursor X on down and on every move, write it directly
  // to crush.adjustment, and stop on pointerup/cancel. The value snaps to
  // the nearest tick and the near-zero dead zone (-2..2) snaps to 0.
  function normalizeBlackLevel(value) {
    return (value !== 0 && Math.abs(value) <= 2) ? 0 : value;
  }

  function setBlackLevel(value) {
    crush.adjustment = normalizeBlackLevel(value);
  }

  function toggleBlackEnabled() {
    crush.blackEnabled = !crush.blackEnabled;
    if (!crush.blackEnabled) {
      sliderDragging = false;
      fsSliderDragging = false;
      sliderHovering = false;
      fsSliderHovering = false;
    }
    scheduleConfigSave();
  }

  // Disabled controls stay discoverable, but an attempted interaction should
  // explain why nothing happened instead of silently doing nothing. The
  // heading pulse is deliberately short and retriggerable for repeated clicks.
  function flashBlackMasterSwitch() {
    blackMasterAttention = false;
    cancelAnimationFrame(blackMasterAttentionRaf);
    blackMasterAttentionRaf = requestAnimationFrame(() => { blackMasterAttention = true; });
    clearTimeout(blackMasterAttentionTimer);
    blackMasterAttentionTimer = setTimeout(() => { blackMasterAttention = false; }, 850);
  }

  function handleDisabledBlackInteraction(e) {
    if (crush.blackEnabled) return;
    // Preserve normal keyboard navigation; only activation/adjustment keys
    // count as an attempted interaction with the disabled section.
    if (e.type === 'keydown' && !['Enter', ' ', 'ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(e.key)) return;
    e.preventDefault();
    e.stopPropagation();
    flashBlackMasterSwitch();
  }

  function handleBlackLevelWheel(e) {
    if (!crush.blackEnabled) {
      e.preventDefault();
      e.stopPropagation();
      flashBlackMasterSwitch();
      return;
    }
    handleSliderWheel(e, mainSliderInput, setBlackLevel, 5);
  }

  function handleBlackLevelInput(e) {
    const value = +e.target.value;
    // Snap to a tick when within tolerance (native thumb drags and keyboard
    // both land here — track clicks already snapped in their pointerdown
    // handler, and this re-applies harmlessly since the value is unchanged).
    const normalized = normalizeBlackLevel(snapToBlackLevelTick(value));
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
    // The black-level slider intentionally snaps values from -2..2 (-0.2..0.2)
    // to zero. Jump across that dead zone so the first wheel tick always
    // changes it.
    if (next === current && current === 0) next = direction > 0 ? 3 : -3;
    setValue(next);
    // Wheel changes are discrete inputs (not drags), so persist them.
    if (setValue === setBlackLevel) scheduleConfigSave();
  }

  function handleTrackWrapPointerdown(e, inputEl, setDragging) {
    if (e.target === inputEl) return; // already on the real input, let it handle itself
    e.preventDefault();
    const min = +inputEl.min, max = +inputEl.max;
    const applyFromClientX = (clientX) => {
      const rect = inputEl.getBoundingClientRect();
      const frac = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
      crush.adjustment = normalizeBlackLevel(snapToBlackLevelTick(min + frac * (max - min)));
    };
    applyFromClientX(e.clientX);
    inputEl.focus();
    setDragging(true);
    function onMove(ev) { applyFromClientX(ev.clientX); }
    function onUp() {
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
      setDragging(false);
      // The drag (or click on the track) ended — persist the final value.
      // Saved directly in this handler (not in an $effect) because the
      // effect's window listener may not be registered yet for fast clicks.
      scheduleConfigSave();
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
  function handleStripKeydown(e) {
    // Only the strip itself owns this shortcut. Do not cycle fullscreen when
    // Enter/Space is used on a nested slider or help button.
    if (e.target !== e.currentTarget) return;
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      cycleFullscreen();
    }
  }

  function handleContextMenuKeydown(e) {
    const items = [...e.currentTarget.querySelectorAll('[role="menuitem"]:not(:disabled)')];
    const currentIndex = items.indexOf(document.activeElement);
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      const direction = e.key === 'ArrowDown' ? 1 : -1;
      const nextIndex = (currentIndex + direction + items.length) % items.length;
      items[nextIndex]?.focus();
    } else if (e.key === 'Home' || e.key === 'End') {
      e.preventDefault();
      (e.key === 'Home' ? items[0] : items[items.length - 1])?.focus();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      closeContextMenu();
    }
    e.stopPropagation();
  }

  function exitFullscreen() {
    crush.fullscreen = 'normal';
    if (document.fullscreenElement) document.exitFullscreen();
  }
  function setMode(mode) {
    crush.fullscreen = mode;
    closeContextMenu();
  }
  function selectStrip(type) {
    setStripType(type);
    closeContextMenu();
  }

  // Mouse wheel over the strip changes the zoom (square count). Wheeling up
  // (negative deltaY) increases zoom, down decreases — matches typical
  // scroll-to-zoom conventions (maps, image viewers). The High Nits grid is
  // fixed at 3×7, so wheel does nothing there (the range steppers below are
  // the High Nits equivalent).
  function handleStripWheel(e) {
    e.preventDefault();
    if (crush.stripType === 'highNits') return;
    const dir = e.deltaY < 0 ? 1 : -1;
    crush.squareCounts[crush.stripType] = Math.min(MAX_SQUARES, Math.max(2, activeSquareCount() + dir));
  }

  // ── High Nits Max slider ─────────────────────────────────────
  // The user only drags Max, which snaps to HIGH_NITS_MAX_STOPS. Step is
  // derived in stores.svelte.js from the Max via the region rule (1000 above
  // 5000, 500 from 1000–5000, 100 below), and the grid always ends exactly
  // at the Max (see calibration.js highNitsGridCodes).
  //
  // The slider is NON-LINEAR: it maps to the stop index (0..22), so each of
  // the 23 stops occupies an equal slice of the track regardless of its nits
  // value. That makes the visual steps match the requested stops exactly
  // (100, 200, … 1000, 1500, … 5000, 6000, … 10000 all evenly spaced) and
  // keeps every stop clickable — a linear 100→10000 scale would cram the
  // 100–1000 stops into the left 9% of the track. The readout and the strip
  // still use the true nits value.
  function highNitsStopIndex() {
    const idx = HIGH_NITS_MAX_STOPS.indexOf(highNitsMaxValue());
    return idx < 0 ? HIGH_NITS_MAX_STOPS.length - 1 : idx;
  }

  // Background for the Max slider: one tick mark per stop (evenly spaced,
  // matching the index-based thumb positions) layered over the accent fill.
  function nitsSliderBackground() {
    const n = HIGH_NITS_MAX_STOPS.length;
    const idx = highNitsStopIndex();
    const fill = (idx / (n - 1)) * 100;
    const ticks = [];
    for (let i = 0; i < n; i++) {
      const pos = (i / (n - 1)) * 100;
      ticks.push(`transparent calc(${pos}% - 0.75px), rgba(255,255,255,0.45) calc(${pos}% - 0.75px), rgba(255,255,255,0.45) calc(${pos}% + 0.75px), transparent calc(${pos}% + 0.75px)`);
    }
    return `linear-gradient(to right, ${ticks.join(',')}), linear-gradient(to right, var(--accent) 0%, var(--accent) ${fill}%, #31313f ${fill}%, #31313f 100%)`;
  }

  // Wheel over the Max slider nudges to the adjacent stop (the native range
  // input only responds to wheel when focused; driving it directly keeps the
  // behavior consistent whether hovered or focused).
  function handleNitsWheel(e) {
    if (e.deltaY === 0) return;
    e.preventDefault();
    const dir = e.deltaY < 0 ? 1 : -1;
    const idx = HIGH_NITS_MAX_STOPS.indexOf(highNitsMaxValue());
    if (idx < 0) return; // defensive: value is always a stop after snapping
    const next = Math.min(HIGH_NITS_MAX_STOPS.length - 1, Math.max(0, idx + dir));
    setHighNitsMax(HIGH_NITS_MAX_STOPS[next]);
  }

  // Arrow/Page/Home/End keys step through the slider stops instead of the
  // native 1-nits step (which would snap back to the same stop and feel
  // dead — value+1 rounds back to the current stop).
  function handleNitsSliderKeydown(e) {
    let idx = HIGH_NITS_MAX_STOPS.indexOf(highNitsMaxValue());
    if (idx < 0) {
      // Defensive: not on a stop yet — snap to the nearest one first.
      const value = highNitsMaxValue();
      let best = 0;
      let bestDist = Infinity;
      HIGH_NITS_MAX_STOPS.forEach((stop, i) => {
        const dist = Math.abs(stop - value);
        if (dist < bestDist) { bestDist = dist; best = i; }
      });
      idx = best;
    }
    const last = HIGH_NITS_MAX_STOPS.length - 1;
    let target = null;
    if (e.key === 'ArrowRight' || e.key === 'ArrowUp') target = HIGH_NITS_MAX_STOPS[Math.min(last, idx + 1)];
    else if (e.key === 'ArrowLeft' || e.key === 'ArrowDown') target = HIGH_NITS_MAX_STOPS[Math.max(0, idx - 1)];
    else if (e.key === 'PageUp') target = HIGH_NITS_MAX_STOPS[Math.min(last, idx + 5)];
    else if (e.key === 'PageDown') target = HIGH_NITS_MAX_STOPS[Math.max(0, idx - 5)];
    else if (e.key === 'Home') target = HIGH_NITS_MAX_STOPS[0];
    else if (e.key === 'End') target = HIGH_NITS_MAX_STOPS[last];
    if (target !== null) {
      e.preventDefault();
      setHighNitsMax(target);
    }
  }

  // ── Hover tooltip on the High Nits grid ───────────────────────────
  // Shows the nits/RGB value of the square under the cursor. The labels in
  // each cell are always visible, but this makes values readable in
  // fullscreen or when the interface is hidden. Cell index is computed from
  // the live grid geometry (stripGeom: fixed 7×3 normally, aspect-fit up to
  // 40 cells in fullscreen), so it's cheap per mousemove; the tooltip
  // position is only written when it actually moves ≥1px.
  let hoverCell = $state(-1);
  let hoverTip = $state({ x: 0, y: 0 });
  let lastHoverIdx = -1;
  let lastTipX = -1;
  let lastTipY = -1;
  const HOVER_TIP_H = 22;

  function handleStripHover(e) {
    if (crush.stripType !== 'highNits') {
      if (hoverCell !== -1) { hoverCell = -1; lastHoverIdx = -1; }
      return;
    }
    const rect = canvas?.getBoundingClientRect();
    if (!rect || rect.width < 1 || rect.height < 1) return;
    // Use the live grid geometry (fixed 3×7 normally, aspect-fit in
    // fullscreen) so the hovered cell always matches what's rendered.
    const cols = stripGeom.cols;
    const rows = stripGeom.rows;
    const col = Math.floor(((e.clientX - rect.left) / rect.width) * cols);
    const row = Math.floor(((e.clientY - rect.top) / rect.height) * rows);
    if (col < 0 || col >= cols || row < 0 || row >= rows) {
      if (hoverCell !== -1) { hoverCell = -1; lastHoverIdx = -1; }
      return;
    }
    const idx = row * cols + col;
    if (idx !== lastHoverIdx) { lastHoverIdx = idx; hoverCell = idx; }
    // Sit on the right side of the cursor with its top edge aligned to the
    // cursor's Y. Never flips sides — it just follows the cursor even at the
    // screen edges (only the vertical position clamps to keep it on-screen).
    const off = 12;
    const x = e.clientX + off;
    const y = Math.max(8, Math.min(e.clientY, window.innerHeight - HOVER_TIP_H - 8));
    if (Math.abs(x - lastTipX) > 1 || Math.abs(y - lastTipY) > 1) {
      lastTipX = x;
      lastTipY = y;
      hoverTip = { x, y };
    }
  }
  function handleStripHoverEnd() {
    if (hoverCell !== -1) { hoverCell = -1; lastHoverIdx = -1; }
  }
  // Value shown in the tooltip for the hovered cell (nits in HDR, RGB in
  // SDR). Only called when hoverCell >= 0 (the template guards on it). The
  // HDR value comes from the actual grid codes (region-stepped), so it stays
  // in sync with the cell's label.
  function hoverLabel() {
    const isHdr = app.hdrStatus === 'hdr' && !forceSdrRender;
    if (isHdr) return { code: String(highNitsGridCodes(crush.highNitsMax, stripGeom.cells)[hoverCell]), unit: 'nits' };
    return { code: String(highNitsSdrCodes(stripGeom.cells)[hoverCell]), unit: 'RGB' };
  }

  // ── Right-click context menu on the strip ──────────────────────────
  let ctxMenu = $state({ open: false, x: 0, y: 0 });
  let ctxMenuElement = $state(null);
  function openContextMenu(e) {
    e.preventDefault();
    // Don't leave the hover tooltip floating over the open menu.
    handleStripHoverEnd();
    const menuW = 260, menuH = 400;
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
          // Fullscreen requests require a current user activation. Retrying
          // from a timer cannot restore that activation and only creates a
          // second rejected promise, so return to a truthful non-fullscreen
          // state and require the user to initiate the action again.
          dwarn(`requestFullscreen failed: ${e?.message || String(e)}`);
          if (!document.fullscreenElement && crush.fullscreen === 'monitor') {
            crush.fullscreen = 'normal';
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

  // Another context menu (resolution / refresh rate in the status bar)
  // opened, which stopPropagation()'s the contextmenu event so this menu's
  // document listener never hears it — close via the shared dismiss tick.
  $effect(() => {
    void ctxMenuDismissTick.n;
    // Read ctxMenu.open untracked so the effect only re-runs when the tick
    // changes — otherwise opening the menu would immediately close it.
    untrack(() => { if (ctxMenu.open) closeContextMenu(); });
  });
  $effect(() => {
    if (!ctxMenu.open) return;
    // Move focus into the menu after it mounts so keyboard users can reach
    // the mode choices immediately after opening it with the mouse.
    const focusId = setTimeout(() => {
      ctxMenuElement?.querySelector('[role="menuitem"]:not(:disabled)')?.focus();
    }, 0);
    function onDocClick() { closeContextMenu(); }
    function onDocKey(e) { if (e.key === 'Escape') closeContextMenu(); }
    // Listen on capture + next tick so the opening right-click doesn't immediately close it.
    const id = setTimeout(() => {
      document.addEventListener('click', onDocClick);
      document.addEventListener('contextmenu', onDocClick);
      document.addEventListener('keydown', onDocKey);
    }, 0);
    return () => {
      clearTimeout(focusId);
      clearTimeout(id);
      document.removeEventListener('click', onDocClick);
      document.removeEventListener('contextmenu', onDocClick);
      document.removeEventListener('keydown', onDocKey);
    };
  });

  function toggleHideCalibrationInterface() {
    setHideCalibrationInterface(!crush.hideCalibrationInterface);
  }

  function toggleBlackStripGap() {
    setBlackStripGapEnabled(!crush.blackStripGapEnabled);
  }

  function restoreDefaults() {
    crush.adjustment = 0;
    crush.blackEnabled = true;
    crush.squareCounts = { black: 12, highNits: 8 };
    crush.highNitsMin = HIGH_NITS_DEFAULT_MIN;
    crush.highNitsMax = HIGH_NITS_DEFAULT_MAX;
    crush.highNitsStep = HIGH_NITS_DEFAULT_STEP;
    crush.blackStripGapEnabled = true;
    crush.stripType = 'black';
    crush.fullscreen = 'normal';
    saveConfig();
  }

  function onKeydown(e) {
    if (app.activeTab !== 'crush') return;
    if (e.key === 'Escape') { if (crush.fullscreen !== 'normal') { exitFullscreen(); return; } }
    if (!crush.blackEnabled) return;
    // crush.adjustment is stored in tenths (adjustmentDisplay() divides by 10),
    // so Left/Right move a whole point (10) and Up/Down move 0.1 (1).
    // Keyboard changes are discrete inputs (not drags), so persist them.
    if (e.key === 'ArrowRight') { crush.adjustment = Math.min(150, crush.adjustment + 10); scheduleConfigSave(); }
    if (e.key === 'ArrowLeft') { crush.adjustment = Math.max(-150, crush.adjustment - 10); scheduleConfigSave(); }
    if (e.key === 'ArrowUp') { e.preventDefault(); crush.adjustment = Math.min(150, crush.adjustment + 1); scheduleConfigSave(); }
    if (e.key === 'ArrowDown') { e.preventDefault(); crush.adjustment = Math.max(-150, crush.adjustment - 1); scheduleConfigSave(); }
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="tab-body">
  <div class="tab-heading tab-heading-2line">
    <div class="tab-heading-toggle" class:attention-pulse={blackMasterAttention} role="button" tabindex="0" onclick={toggleBlackEnabled} onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggleBlackEnabled(); } }} title="Enable Black Level" aria-label="Enable Black Level" aria-pressed={crush.blackEnabled}>
      <img class="tab-icon" src={tabCrushIcon} alt="" />
      <div class="tab-heading-titles">
        <div class="tab-heading-title-row">
          <h2>Black Crush Tweak</h2>
        </div>
        <span class="subtitle">Adjust black levels using a Reinhard gamma curve. Preserves pure black.</span>
      </div>
      <div class="tab-heading-spacer"></div>
      <span class={`pill${crush.blackEnabled ? '' : ' off'}`} aria-hidden="true"></span>
    </div>
    <button class="restore-defaults-btn" onclick={restoreDefaults}>↻ Restore Tab Defaults</button>
  </div>
  <div class="separator"></div>    <div class="dimmer-control" class:disabled={!crush.blackEnabled} onwheel={handleBlackLevelWheel}>
      {#if app.displayInfo.referenceMode}
        <p class="gamma-warning" role="alert">⚠ NVIDIA Reference Mode is enabled. <br>To use Black Crush Tweak disable “Override to reference mode” in your NVIDIA driver settings under "System → Displays → Colour".</p>
      {/if}
    {#if !crush.blackEnabled}
      <button type="button" class="disabled-section-hit-area" tabindex="-1" aria-label="Black Level disabled" onclick={handleDisabledBlackInteraction}></button>
    {/if}
    <div class="dimmer-control-heading">
      <span class="settings-row-label">Black Level</span>
      <span class="slider-val">{adjustmentDisplay()}</span>
    </div>
    <div class="slider-track-wrap dimmer-slider-track" role="presentation" onpointerdown={(e) => handleTrackWrapPointerdown(e, mainSliderInput, (v) => sliderDragging = v)}>
      <input type="range" min="-150" max="150" bind:value={crush.adjustment} bind:this={mainSliderInput}
        class="has-ticks"
        style="--fill:{((crush.adjustment + 150) / 300) * 100}%"
        oninput={handleBlackLevelInput}
        onpointerdown={() => sliderDragging = true}
        onpointerup={() => { sliderDragging = false; scheduleConfigSave(); }}
        onpointercancel={() => { sliderDragging = false; scheduleConfigSave(); }}
        onpointerenter={() => { if (crush.blackEnabled) sliderHovering = true; }}
        onpointerleave={() => sliderHovering = false}
        disabled={!crush.blackEnabled} />
      <div class="slider-tooltip" class:visible={sliderDragging || sliderHovering} style="left:calc(9px + ({((crush.adjustment + 150) / 300)} * (100% - 18px)))">{adjustmentDisplay()}</div>
    </div>
    <div class="dimmer-scale"><span>Darker</span><span>Brighter</span></div>
  </div>

  <div class="separator"></div>

  {#snippet calibrationTip()}
    {#if crush.stripType === 'highNits'}
      <span>Each cell is a different brightness, low to high. Find the highest nits you can still distinguish — above that, your display clips to white.</span>
      <span class="info-tip-note">
        <svg viewBox="0 0 16 16" fill="none"><circle cx="8" cy="8" r="6.5" stroke="currentColor" stroke-width="1.3"/><path d="M8 7.2V11.3" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><circle cx="8" cy="4.9" r="0.9" fill="currentColor"/></svg>
        <span>HDR: nits up to {highNitsMaxValue()} · SDR: RGB {HIGH_SDR_RANGE[0]}–{HIGH_SDR_RANGE[HIGH_SDR_RANGE.length - 1]}</span>
      </span>
    {:else}
      <span>Adjust black level until you can start to see the lower columns and still have a smooth gradient.</span>
      <span class="info-tip-note">
        <svg viewBox="0 0 16 16" fill="none"><circle cx="8" cy="8" r="6.5" stroke="currentColor" stroke-width="1.3"/><path d="M8 7.2V11.3" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><circle cx="8" cy="4.9" r="0.9" fill="currentColor"/></svg>
        <span><span class="info-tip-strong">{app.hdrStatus === 'hdr' ? '64' : '0'}</span> column is pure black.</span>
      </span>
    {/if}
    <div class="fs-hint-sep"></div>
    <div class="fs-controls-heading">Controls</div>
    <div class="fs-controls-block">
      {#if crush.fullscreen !== 'normal'}
        <div class="fs-hint-row">
          <span class="fs-hint-val">Exit</span>
          <span class="fs-hint-keys"><span class="fs-keycap">Esc</span></span>
        </div>
      {/if}
      <div class="fs-hint-row">
        <span class="fs-hint-val">Adjust ±1</span>
        <span class="fs-hint-keys">
          <span class="fs-keycap fs-keycap-icon"><svg viewBox="0 0 16 16" fill="none"><path d="M6 4L2.5 8L6 12" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg></span>
          <span class="fs-keycap fs-keycap-icon"><svg viewBox="0 0 16 16" fill="none"><path d="M10 4L13.5 8L10 12" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg></span>
        </span>
      </div>
      <div class="fs-hint-row">
        <span class="fs-hint-val">Adjust ±0.1</span>
        <span class="fs-hint-keys">
          <span class="fs-keycap fs-keycap-icon"><svg viewBox="0 0 16 16" fill="none"><path d="M4 6L8 2.5L12 6" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg></span>
          <span class="fs-keycap fs-keycap-icon"><svg viewBox="0 0 16 16" fill="none"><path d="M4 10L8 13.5L12 10" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg></span>
        </span>
      </div>
      <div class="fs-hint-row">
        <span class="fs-hint-val">Cycle mode</span>
        <span class="fs-hint-keys"><span class="fs-keycap fs-keycap-mouse">Double-click<svg viewBox="0 0 16 16" fill="none"><rect x="3.5" y="2.5" width="9" height="12" rx="3.5" stroke="currentColor" stroke-width="1.2"/><path d="M8 2.5V7" stroke="currentColor" stroke-width="1.2"/><path d="M8 7H3.5V6.5A3.5 3.5 0 0 1 7 3H8V7Z" fill="currentColor" fill-opacity="0.65" stroke="none"/></svg></span></span>
      </div>
      <div class="fs-hint-row">
        <span class="fs-hint-val">Menu</span>
        <span class="fs-hint-keys"><span class="fs-keycap fs-keycap-mouse">Right-click<svg viewBox="0 0 16 16" fill="none"><rect x="3.5" y="2.5" width="9" height="12" rx="3.5" stroke="currentColor" stroke-width="1.2"/><path d="M8 2.5V7" stroke="currentColor" stroke-width="1.2"/><path d="M8 7H12.5V6.5A3.5 3.5 0 0 0 9 3H8V7Z" fill="currentColor" fill-opacity="0.65" stroke="none"/></svg></span></span>
      </div>
      <div class="fs-hint-row">
        <span class="fs-hint-val">Zoom</span>
        <span class="fs-hint-keys"><span class="fs-keycap fs-keycap-mouse">Scroll<svg viewBox="0 0 16 16" fill="none"><rect x="3.5" y="2.5" width="9" height="12" rx="3.5" stroke="currentColor" stroke-width="1.2"/><path d="M8 5.5V8.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/></svg></span></span>
      </div>
    </div>
  {/snippet}

  <div class="calibration-strip-panel">
    {#if app.hdrStatus === 'unknown'}
      <p class="gpu-status">Detecting HDR status...</p>
    {:else}
      <p class="strip-heading">
        <!-- Title + Info group sizes to its content so the Info button always
             sits right next to the title text; the Black/Brightness switch is
             anchored to the heading's right edge (margin-left:auto) and never
             moves when the title length changes. -->
        <span class="strip-heading-title">
          <strong>{stripTitle()}</strong>
          <span class="info-tip-wrap">
            <button class="info-tip-btn" aria-label="About the calibration strip and controls" tabindex="0">Info</button>
            <span class="info-tip">{@render calibrationTip()}</span>
          </span>
        </span>
        <span class="strip-switch" role="group" aria-label="Calibration strip">
          {#each STRIPS as strip}
            <button type="button" class="strip-switch-btn" class:active={crush.stripType === strip.id}
              aria-pressed={crush.stripType === strip.id}
              title={strip.title}
              onclick={() => setStripType(strip.id)}>{strip.label}</button>
          {/each}
        </span>
      </p>
    {/if}

    {#if gpuError}
      <p class="gpu-error">{gpuError}</p>
    {/if}

    <div class="strip-wrap" class:monitor-fullscreen={crush.fullscreen === 'monitor'} class:inapp-fullscreen={crush.fullscreen === 'inApp'} class:high-nits={crush.stripType === 'highNits'} style={crush.fullscreen !== 'normal' ? 'position:fixed;inset:0;z-index:9999;border-radius:0;margin:0;' : ''}
      onclick={handleStripClick} onkeydown={handleStripKeydown} oncontextmenu={openContextMenu} onwheel={handleStripWheel}
      onmousemove={handleStripHover} onmouseleave={handleStripHoverEnd}
      role="button" tabindex="0" aria-label="Calibration strip. Double-click or press Enter or Space to cycle fullscreen mode.">
      <canvas bind:this={canvas}></canvas>
      {#if app.hdrStatus !== 'unknown' && !crush.hideCalibrationInterface}
        {#if crush.stripType === 'highNits'}
          {@const g = stripGeom}
          <div class="grid-labels" style="--grid-cols:{g.cols}; --grid-rows:{g.rows}">
            {#each stripLabels() as label, i}
              <div class="grid-label" class:grid-label-bright={label.bright}>
                <div>{label.code}</div>
                <div class="square-unit">{label.unit}</div>
              </div>
            {/each}
          </div>
        {:else}
          <div class="square-labels" style="--sq-count:{labelCount()}">
            {#each stripLabels() as label, i}
              <div class="square-label">
                <div>{label.code}</div>
                <div class="square-unit">{label.unit}</div>
              </div>
            {/each}
          </div>
        {/if}
      {/if}
      {#if crush.fullscreen !== 'normal' && !crush.hideCalibrationInterface && crush.stripType !== 'highNits'}
        <div class="fs-help-wrap">
          <button class="fs-help-btn" aria-label="About black calibration and strip controls" tabindex="0">Info</button>
          <div class="fs-help-tip">{@render calibrationTip()}</div>
        </div>
      {/if}
      {#if crush.fullscreen !== 'normal' && !crush.hideCalibrationInterface && crush.stripType !== 'highNits'}
        <div class="fs-black-slider" class:disabled={!crush.blackEnabled} onwheel={(e) => {
          if (!crush.blackEnabled) { e.preventDefault(); e.stopPropagation(); flashBlackMasterSwitch(); return; }
          handleSliderWheel(e, fsSliderInput, setBlackLevel, 5);
        }}>
          <div class="slider-track-wrap" role="presentation" onpointerdown={(e) => handleTrackWrapPointerdown(e, fsSliderInput, (v) => fsSliderDragging = v)}>
            <input type="range" min="-150" max="150" bind:value={crush.adjustment} bind:this={fsSliderInput}
              class="has-ticks"
              style="--fill:{((crush.adjustment + 150) / 300) * 100}%"
              oninput={handleBlackLevelInput}
              onpointerdown={() => fsSliderDragging = true}
              onpointerup={() => { fsSliderDragging = false; scheduleConfigSave(); }}
              onpointercancel={() => { fsSliderDragging = false; scheduleConfigSave(); }}
              onpointerenter={() => { if (crush.blackEnabled) fsSliderHovering = true; }}
              onpointerleave={() => fsSliderHovering = false}
              disabled={!crush.blackEnabled} />
            <div class="slider-tooltip" class:visible={fsSliderDragging || fsSliderHovering} style="left:calc(9px + ({((crush.adjustment + 150) / 300)} * (100% - 18px)))">{adjustmentDisplay()}</div>
          </div>
          <span class="slider-val">{adjustmentDisplay()}</span>
          {#if !crush.blackEnabled}
            <button type="button" class="disabled-section-hit-area" tabindex="-1" aria-label="Black Level disabled" onclick={handleDisabledBlackInteraction}></button>
          {/if}
        </div>
      {/if}
      {#if crush.stripType === 'highNits' && hoverCell >= 0}
        {@const tip = hoverLabel()}
        <div class="nits-hover-tip" role="tooltip" style="left:{hoverTip.x}px; top:{hoverTip.y}px">
          <strong>{tip.code}</strong><span>{tip.unit}</span>
        </div>
      {/if}
    </div>

    {#if crush.stripType === 'highNits'}
      <!-- Fixed 3×7 grid — the user picks only the top of the nits range   -->
      <!-- with a single Max slider; Min is fixed and the nits-per-cell step -->
      <!-- is derived silently as (max − min) / 20 (setHighNitsMax in        -->
      <!-- stores.svelte.js), not shown as its own control. The slider only -->
      <!-- rests on HIGH_NITS_MAX_STOPS. In SDR the strip shows a fixed RGB  -->
      <!-- ramp, so the nits range doesn't apply — greyed out (still         -->
      <!-- configurable once in HDR).                                       -->
      <div class="nits-range-control" class:sdr-disabled={app.hdrStatus !== 'hdr' || forceSdrRender} title={app.hdrStatus === 'hdr' && !forceSdrRender ? 'Nits range shown on the strip' : 'Nits range only applies in HDR'}>
        <!-- SDR: the ramp is a fixed RGB range, so disable the control -->
        <div class="nits-slider-group" onwheel={handleNitsWheel}>
          <span class="nits-range-label">Max</span>
          <div class="slider-track-wrap zoom-track-compact">
            <!-- The input is index-based (0..22 = one per stop): native arrow
                 keys step one stop at a time and the thumb/ticks are evenly
                 spaced. oninput maps the raw index to its stop value. -->
            <input type="range" min="0" max={HIGH_NITS_MAX_STOPS.length - 1} step="1"
              value={highNitsStopIndex()}
              oninput={(e) => setHighNitsMax(HIGH_NITS_MAX_STOPS[+e.target.value])}
              onkeydown={handleNitsSliderKeydown}
              style="background:{nitsSliderBackground()}"
              aria-label="Maximum nits"
              aria-valuetext={`${highNitsMaxValue()} nits`} />
          </div>
          <span class="nits-slider-val">{highNitsMaxValue()}</span>
        </div>
        <span class="nits-range-unit">nits · step {crush.highNitsStep}</span>
      </div>
    {:else}
      <div class="zoom-control-compact" onwheel={(e) => handleSliderWheel(e, zoomSliderInput, (value) => crush.squareCounts[crush.stripType] = value)}>
        <img class="slider-label-icon" src={zoomIcon} alt="Zoom" />
        <div class="slider-track-wrap zoom-track-compact">
          <input type="range" min="2" max={MAX_SQUARES} bind:value={crush.squareCounts[crush.stripType]} bind:this={zoomSliderInput}
            style="--fill:{((activeSquareCount() - 2) / (MAX_SQUARES - 2)) * 100}%" />
        </div>
        <span class="slider-val zoom-val-compact">{activeSquareCount()}</span>
      </div>
    {/if}
  </div>

  {#if ctxMenu.open}
    <div class="ctx-menu compact" bind:this={ctxMenuElement} style="left:{ctxMenu.x}px; top:{ctxMenu.y}px;" role="menu" aria-label="Calibration display mode" tabindex="-1" onclick={(e) => e.stopPropagation()} onkeydown={handleContextMenuKeydown} oncontextmenu={(e) => e.preventDefault()}>
      <button class="ctx-item" role="menuitem" class:active={crush.fullscreen === 'inApp'} onclick={() => setMode('inApp')}><span class="ctx-radio-dot" aria-hidden="true"></span>In-App Fullscreen</button>
      <button class="ctx-item" role="menuitem" class:active={crush.fullscreen === 'monitor'} onclick={() => setMode('monitor')}><span class="ctx-radio-dot" aria-hidden="true"></span>Monitor Fullscreen</button>
      <div class="ctx-menu-sep"></div>
      <button class="ctx-item" role="menuitem" onclick={exitFullscreen} disabled={crush.fullscreen === 'normal'}>Exit Fullscreen</button>
      <div class="ctx-menu-sep"></div>
      <button class="ctx-item" role="menuitem" class:active={crush.hideCalibrationInterface} onclick={toggleHideCalibrationInterface}><span class="ctx-checkbox" aria-hidden="true"></span>Hide interface</button>
      {#if crush.stripType === 'black'}
        <button class="ctx-item" role="menuitem" class:active={crush.blackStripGapEnabled} onclick={toggleBlackStripGap}><span class="ctx-checkbox" aria-hidden="true"></span>Gap between squares</button>
      {/if}
      <div class="ctx-menu-sep"></div>
      <div class="ctx-menu-label">Calibration strip</div>
      {#each STRIPS as strip}
        <button class="ctx-item" role="menuitem" class:active={crush.stripType === strip.id} onclick={() => selectStrip(strip.id)}><span class="ctx-radio-dot" aria-hidden="true"></span>{strip.label}</button>
      {/each}
    </div>
  {/if}
</div>
