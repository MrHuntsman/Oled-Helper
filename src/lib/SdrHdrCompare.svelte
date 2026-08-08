<script>
  // SDR-vs-HDR nits-matched comparison ramp (Experimental tab).
  //
  // Two rows of gray patches on a small WebGPU canvas:
  //   • TOP row (SDR)     — the same nits levels shown the way SDR content
  //     renders at the CURRENT SDR white level. Content is scaled by the
  //     white level relative to the BT.2400 reference (203 nits), and SDR
  //     content can't get brighter than its white point, so columns above it
  //     flatten to pure white. The whole row brightens/darkens as the slider
  //     moves — exactly how Windows' SDR-vs-HDR comparison behaves.
  //   • BOTTOM row (HDR)  — the exact same nits levels as true HDR
  //     luminance, fixed and unaffected by the slider.
  // At sdrWhiteNits === HDR_REFERENCE_NITS (203) both rows render the same
  // values (the "aligned" state); above that the SDR row is visibly brighter
  // than the HDR reference, below it visibly dimmer.
  //
  // Both rows are encoded against the browser's FROZEN SDR white base
  // (app.sdrWhiteBaseNits) — Chromium/WebView2 scales the HDR canvas by that
  // startup value, so dividing by the same value makes every patch land at its
  // true nits regardless of the startup slider, matching the Brightness
  // strip's compensation in TabCrush (see patchValue).
  import { onDestroy } from 'svelte';
  import { CELL_GRID_SHADER, linearToSrgb, HDR_REFERENCE_NITS } from './calibration.js';
  import { app, setGpuRenderingInfo, refreshSdrWhiteLevel } from './stores.svelte.js';

  let { sdrWhiteNits, hdrActive } = $props();

  let canvas = $state(null);
  let device = null;
  let ctx = null;
  let ub = null;
  let bg = null;
  let pipe = null;
  let pipelineLayout = null;
  let shaderModule = null;
  let currentFormat = null;
  let ready = false;
  let forceSdrRender = false;
  let initStarted = false;
  let gpuError = $state('');
  let generation = 0;

  // Column ramp: everyday SDR levels up to the BT.2400 reference white, then
  // bright HDR steps ending at the CCD cap. 203 is centered.
  const COLUMN_NITS = [50, 100, 150, 203, 300, 400, 480];
  const COLS = COLUMN_NITS.length;
  const ROWS = 2;
  const GAP = 0.005;
  // The shader derives cell geometry from the gap (sw = (1-gf*(cols-1))/cols,
  // cw = sw+gf), so the shader's column centers sit at i*cw + sw/2, NOT at the
  // naive (i+0.5)/cols a CSS grid would give. Compute the same positions for
  // the DOM column labels so every number sits exactly under its cell.
  const CELL_SW = (1 - GAP * (COLS - 1)) / COLS;
  const CELL_CW = CELL_SW + GAP;
  function columnCenterPct(i) {
    return ((i * CELL_CW + CELL_SW / 2) * 100).toFixed(3);
  }

  const derr = (msg) => { console.error('[gpu]', msg); };

  function runtimeLabel() {
    if (window.chrome?.webview) {
      const chromium = navigator.userAgent.match(/(?:Chrome|Edg|Chromium)\/(\d+)/)?.[1];
      return chromium ? `WebView2 (Chromium ${chromium})` : 'WebView2';
    }
    return 'Browser WebView';
  }

  // Match the calibration strip's canvas path exactly: browser-default tone
  // mapping (no toneMapping dictionary — the extended-tone-mapping experiment
  // flattened the Crush strip's calibrated values). The rgba16float HDR
  // canvas preserves values above 1.0 on its own, so the 300/400/480-nit
  // patches stay distinct without it.
  function configureContext(format, colorSpace) {
    ctx.configure({ device, format, usage: GPUTextureUsage.RENDER_ATTACHMENT, colorSpace });
    setGpuRenderingInfo('comparison', {
      api: 'WebGPU',
      runtime: runtimeLabel(),
      format,
      colorSpace,
      toneMapping: 'browser default',
      hdrPath: format === 'rgba16float' ? 'HDR canvas (browser-managed)' : 'SDR canvas',
    });
    return 'browser default';
  }

  // The scRGB luminance for a patch: the HDR row is the exact nits; the SDR
  // row is that nits scaled by the CURRENT white level relative to the 203
  // mastering reference (content mastered to 203 is shown at the current
  // white point), clipped at the white level since SDR can't exceed it. Both
  // rows are then encoded against the browser's FROZEN SDR white base
  // (app.sdrWhiteBaseNits) instead of the fixed 203: Chromium/WebView2 scales
  // the HDR canvas by that frozen value, so dividing by the same value makes
  // each patch land at its true nits no matter what the startup slider was —
  // the same compensation as the Brightness strip. At live white = 203 the
  // two rows encode the same values (the aligned state).
  function patchValue(nits, row) {
    const base = app.sdrWhiteBaseNits ?? HDR_REFERENCE_NITS;
    const white = Number(sdrWhiteNits) || HDR_REFERENCE_NITS;
    const displayNits = row === 0
      ? Math.min(white * (nits / HDR_REFERENCE_NITS), white)
      : nits;
    return linearToSrgb(displayNits / base);
  }

  function releaseGpuResources() {
    generation++;
    initStarted = true;
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

  // Keep the last successful comparison diagnostics in the session store.
  // The canvas is intentionally destroyed when leaving Experimental, but
  // clearing the record would make System Info forget a renderer that really
  // did initialize successfully.
  onDestroy(releaseGpuResources);

  async function initGPU() {
    if (initStarted) return;
    initStarted = true;
    const myGeneration = generation;
    try {
      if (!navigator.gpu) { gpuError = 'WebGPU not available'; derr('FAIL: WebGPU not available'); return; }
      const adapter = await navigator.gpu.requestAdapter();
      if (!adapter || myGeneration !== generation) return;
      device = await adapter.requestDevice();
      if (myGeneration !== generation) { device.destroy(); device = null; return; }
      ctx = canvas.getContext('webgpu');

      const wantHdr = hdrActive && !forceSdrRender;
      let format = wantHdr ? 'rgba16float' : 'rgba8unorm';
      let colorSpace = wantHdr ? 'display-p3' : 'srgb';
      try {
        const toneMapping = configureContext(format, colorSpace);
        console.log(`[gpu] comparison canvas: ${format}/${colorSpace}/${toneMapping}`);
      } catch (e) {
        format = 'rgba8unorm';
        colorSpace = 'srgb';
        try {
          configureContext(format, colorSpace);
          if (wantHdr) forceSdrRender = true;
        } catch (e2) {
          gpuError = 'GPU configure failed';
          derr(`compare configure FAILED: ${e2.message}`);
          return;
        }
      }

      ub = device.createBuffer({ size: 208, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
      const bgl = device.createBindGroupLayout({ entries: [{ binding: 0, visibility: GPUShaderStage.FRAGMENT, buffer: { type: 'uniform' } }] });
      bg = device.createBindGroup({ layout: bgl, entries: [{ binding: 0, resource: { buffer: ub } }] });
      const sh = device.createShaderModule({ code: CELL_GRID_SHADER });
      shaderModule = sh;
      pipelineLayout = device.createPipelineLayout({ bindGroupLayouts: [bgl] });
      currentFormat = format;
      pipe = device.createRenderPipeline({
        layout: pipelineLayout,
        vertex: { module: sh, entryPoint: 'vs' },
        fragment: { module: sh, entryPoint: 'fs', targets: [{ format }] },
        primitive: { topology: 'triangle-list' },
      });
      ready = true;
      gpuError = '';
      render();
      device.lost.then((info) => {
        if (myGeneration !== generation) return;
        gpuError = 'GPU device lost';
        releaseGpuResources();
      }).catch(() => {});
    } catch (e) {
      gpuError = 'GPU init failed: ' + (e?.message || String(e));
      derr(`compare initGPU: ${e?.message || String(e)}`);
      releaseGpuResources();
    }
  }

  function reconfigureForFormat(wantHdr) {
    if (!device || !ctx) return;
    const format = wantHdr ? 'rgba16float' : 'rgba8unorm';
    const colorSpace = wantHdr ? 'display-p3' : 'srgb';
    if (format === currentFormat) return;
    try {
      configureContext(format, colorSpace);
      pipe = device.createRenderPipeline({
        layout: pipelineLayout,
        vertex: { module: shaderModule, entryPoint: 'vs' },
        fragment: { module: shaderModule, entryPoint: 'fs', targets: [{ format }] },
        primitive: { topology: 'triangle-list' },
      });
      currentFormat = format;
      // Entering HDR means the browser just created a fresh HDR surface and
      // re-read its frozen SDR white level — re-capture the shared base so the
      // comparison re-bases too (same hook as the Crush canvas).
      if (format === 'rgba16float') refreshSdrWhiteLevel(true);
    } catch (e) {
      derr(`compare reconfigure FAILED: ${e.message}`);
    }
  }

  function render() {
    if (!ready || !device || !canvas) return;
    try {
      const dpr = devicePixelRatio || 1;
      const w = Math.round(canvas.clientWidth * dpr);
      const h = Math.round(canvas.clientHeight * dpr);
      if (w < 1 || h < 1) return;
      if (w !== canvas.width || h !== canvas.height) { canvas.width = w; canvas.height = h; }

      const data = new Float32Array(52);
      for (let col = 0; col < COLS; col++) {
        // The HDR reference row (row 1) is computed from the frozen base, not
        // the live slider — it stays fixed while the SDR row reacts to it.
        data[col] = patchValue(COLUMN_NITS[col], 0);
        data[COLS + col] = patchValue(COLUMN_NITS[col], 1);
      }
      data[48] = COLS * ROWS; // cells
      data[49] = COLS;       // cols
      data[50] = ROWS;       // rows
      data[51] = GAP;
      device.queue.writeBuffer(ub, 0, data);

      let tex;
      try { tex = ctx.getCurrentTexture(); } catch (e) { return; }
      const enc = device.createCommandEncoder();
      const pass = enc.beginRenderPass({ colorAttachments: [{ view: tex.createView(), loadOp: 'clear', storeOp: 'store', clearValue: [0, 0, 0, 1] }] });
      pass.setPipeline(pipe); pass.setBindGroup(0, bg); pass.draw(3); pass.end();
      device.queue.submit([enc.finish()]);
      gpuError = '';
    } catch (e) {
      gpuError = 'Render error: ' + e.message;
    }
  }

  let resizeObs;
  let resizeRaf = 0;
  $effect(() => {
    if (!canvas) return;
    if (!initStarted) initGPU();
    resizeObs = new ResizeObserver(() => {
      if (resizeRaf) cancelAnimationFrame(resizeRaf);
      resizeRaf = requestAnimationFrame(() => { resizeRaf = 0; render(); });
    });
    resizeObs.observe(canvas);
    return () => { resizeObs?.disconnect(); resizeObs = null; if (resizeRaf) cancelAnimationFrame(resizeRaf); };
  });

  // Re-render whenever the slider value, the frozen base, or HDR state changes.
  $effect(() => {
    void sdrWhiteNits; void hdrActive; void app.sdrWhiteBaseNits; void canvas;
    if (ready) {
      reconfigureForFormat(hdrActive && !forceSdrRender);
      render();
    }
  });
</script>

<div class="sdr-hdr-compare">
  <div class="sdr-hdr-row-labels">
    <span>SDR white</span>
    <span>HDR ref</span>
  </div>
  <div class="sdr-hdr-canvas-wrap">
    <canvas
      bind:this={canvas}
      aria-label="SDR white level comparison ramp: top row SDR content, bottom row HDR reference, over shared nits levels"></canvas>
    <div class="sdr-hdr-column-labels">
      {#each COLUMN_NITS as nits, i}
        <span class:sdr-hdr-reference-col={nits === HDR_REFERENCE_NITS} style="left: {columnCenterPct(i)}%">{nits}</span>
      {/each}
    </div>
  </div>
  {#if gpuError}
    <p class="gpu-error">{gpuError}</p>
  {/if}
</div>

<style>
  .sdr-hdr-compare {
    display: flex;
    gap: 10px;
    margin: 18px 0;
    padding: 12px 12px 32px;
    border: 1px solid var(--divider);
    border-radius: 10px;
    background: color-mix(in srgb, var(--panel) 88%, #fff 12%);
  }
  .sdr-hdr-row-labels {
    position: relative;
    flex: 0 0 auto;
    width: 58px;
    height: 64px; /* must match the canvas height so 25%/75% hit the row centers */
  }
  .sdr-hdr-row-labels span {
    position: absolute;
    right: 0;
    transform: translateY(-50%);
    font-size: 11px;
    color: var(--text-weak);
    text-align: right;
    white-space: nowrap;
  }
  .sdr-hdr-row-labels span:first-child { top: 25%; }
  .sdr-hdr-row-labels span:last-child { top: 75%; }
  .sdr-hdr-canvas-wrap {
    position: relative;
    flex: 1 1 auto;
    min-width: 0;
  }
  .sdr-hdr-canvas-wrap canvas {
    display: block;
    width: 100%;
    height: 64px;
    border-radius: 6px;
    background: #000;
  }
  .sdr-hdr-column-labels {
    position: absolute;
    left: 0;
    right: 0;
    top: calc(100% + 6px);
    height: 0;
    font-size: 10px;
    font-variant-numeric: tabular-nums;
    color: var(--text-weak);
  }
  .sdr-hdr-column-labels span {
    position: absolute;
    transform: translateX(-50%);
    white-space: nowrap;
  }
  .sdr-hdr-column-labels span.sdr-hdr-reference-col {
    color: var(--accent);
    font-weight: 600;
  }
</style>
