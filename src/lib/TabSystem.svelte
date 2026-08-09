<script>
  // Inlined data URI — see App.svelte for why icons never hit the network.
  import tabSystemIcon from './icons/tab_system.png?inline';
  import { onMount } from 'svelte';
  import { app, envTick } from './stores.svelte.js';

  let copied = $state(false);
  let copyFailed = $state(false);

  let displayType = $derived(
    app.hdrStatus === 'hdr' ? 'HDR' : app.hdrStatus === 'sdr' ? 'SDR' : 'Detecting...'
  );

  function fmtNits(v) {
    if (v == null) return null;
    if (v >= 100) return String(Math.round(v));
    if (v >= 10) return String(Math.round(v * 10) / 10);
    if (v >= 1) return String(Math.round(v * 100) / 100);
    return v.toPrecision(2);
  }

  // Peak luminance in nits comes from the monitor's EDID/driver via DXGI
  // (IDXGIOutput6::GetDesc1) — see hdr_toggle.rs. Falls back to '—' when
  // the panel doesn't report it (some drivers return 0).
  let hdrLuminance = $derived.by(() => {
    const l = app.hdrLuminance;
    if (!l) return '—';
    const max = fmtNits(l.maxNits);
    return max ? `${max} nits` : '—';
  });

  // True native resolution (EDID preferred timing) — the same data Windows
  // uses for its "(Recommended)" label. `nativeSource` says which EDID
  // location supplied it ("CEA-861" for the extension block TVs use,
  // "base EDID" for the base block, or empty when unreadable).
  let nativeResolution = $derived.by(() => {
    const d = app.displayInfo;
    if (!d.nativeWidth || !d.nativeHeight) return '—';
    const source = d.nativeSource === 'CEA-861' ? 'CEA-861 ext.' : d.nativeSource === 'base EDID' ? 'base EDID' : d.nativeSource;
    return `${d.nativeWidth}×${d.nativeHeight}${source ? ` · ${source}` : ''}`;
  });

  function runtimeLabel() {
    if (window.chrome?.webview) {
      const chromium = navigator.userAgent.match(/(?:Chrome|Edg|Chromium)\/(\d+)/)?.[1];
      return chromium ? `WebView2 (Chromium ${chromium})` : 'WebView2';
    }
    return 'Browser WebView';
  }

  // Depends on envTick (bumped on tab mount) so the table refreshes each time
  // this tab is opened. GPU rendering fields are retained after a canvas tab is
  // closed, because the renderer is intentionally tab-scoped. The labels below
  // distinguish a last successful initialization from a canvas currently
  // mounted in the active tab.
  // Grouped into logical sections so the panel reads as Browser / GPU &
  // Rendering / Display instead of one flat list of environment rows.
  let sections = $derived.by(() => {
    void envTick.n;
    const gpu = app.gpuRendering.main;
    const comparisonGpu = app.gpuRendering.comparison;
    const runtime = gpu.runtime || comparisonGpu.runtime || runtimeLabel();
    const api = gpu.api || comparisonGpu.api || (navigator.gpu ? 'WebGPU' : 'Not available');
    const hdr = app.hdrStatus === 'hdr';
    // The calibration canvases are intentionally tab-scoped so they do not
    // consume a GPU device while the app is sitting in the tray or another
    // tab. Before either canvas has been opened, report the known rendering
    // path and explain that the detailed context probe is lazy instead of
    // showing a misleading wall of "Not initialized" values.
    const mainActive = app.activeTab === 'crush';
    const comparisonActive = app.activeTab === 'windows-hdr';
    const canvas = gpu.format
      ? `${gpu.format}${gpu.colorSpace ? ` · ${gpu.colorSpace}` : ''} (last initialized${mainActive ? ', active' : ''})`
      : navigator.gpu
        ? `WebGPU canvas (${hdr ? 'rgba16float · display-p3' : 'rgba8unorm · srgb'}; initializes in Black Crush)`
        : 'WebGPU unavailable';
    // The GPU model comes from DXGI (vendor-agnostic, available at startup)
    // and is preferred over the WebGPU adapter label — WebView2 leaves the
    // adapter's device field blank on Windows, which is where the old
    // "nvidia / ? / ampere" came from.
    const gpuModel = (app.displayInfo.gpuModel || '').trim();
    const adapter = gpuModel
      ? gpuModel
      : (gpu.adapter || comparisonGpu.adapter || (navigator.gpu ? 'Queried when a calibration canvas opens' : 'Unavailable'));
    const toneMapping = gpu.toneMapping || comparisonGpu.toneMapping
      || (navigator.gpu ? (hdr ? 'Extended HDR requested when canvas initializes' : 'Standard SDR') : 'Unavailable');
    const renderingPath = gpu.hdrPath || (navigator.gpu
      ? `${runtime} → WebGPU ${hdr ? 'HDR' : 'SDR'} canvas (lazy initialization)`
      : 'Unavailable');
    const comparisonPath = comparisonGpu.format
      ? `${comparisonGpu.format} · ${comparisonGpu.toneMapping || 'standard'} (last initialized${comparisonActive ? ', active' : ''})`
      : 'Initializes when Windows HDR opens';
    return [
      {
        title: 'Browser',
        rows: [
          ['Browser / WebView', runtime],
          ['User agent', navigator.userAgent],
          ['WebGPU Support', api],
        ],
      },
      {
        title: 'GPU / Rendering',
        rows: [
          // The adapter, then how the calibration canvases are driven on it.
          ['GPU Adapter', adapter],
          ['GPU Canvas', canvas],
          ['Rendering Path', renderingPath],
          ['Tone Mapping', toneMapping],
          ['Comparison Canvas', comparisonPath],
        ],
      },
      {
        title: 'Display',
        rows: [
          // Identity and mode first: what the panel is and how it's driven.
          ['Current Display', app.displayInfo.monitorName || 'Detecting...'],
          ['Display Type', displayType],
          ['Native Resolution', nativeResolution],
          ['Screen Resolution', app.displayInfo.width && app.displayInfo.height
            ? `${app.displayInfo.width}×${app.displayInfo.height}${app.displayInfo.refreshRateHz ? ` @ ${app.displayInfo.refreshRateHz}Hz` : ''}`
            : 'Detecting...'],
          ['Display Scaling', devicePixelRatio ? `${Math.round(devicePixelRatio * 100)}%` : '?'],
          // Signal path: how the pixels travel from the GPU to the panel.
          // These fields are also exposed in the compact monitor tooltip;
          // keeping them here makes the report canonical and copyable (and
          // avoids a duplicate "Current" row — Screen Resolution already
          // includes the active resolution and refresh rate).
          ['Connector', app.displayInfo.connector || '—'],
          ['Color Format', `${app.displayInfo.colorFormat || '—'}${app.displayInfo.bitsPerChannel ? ` · ${app.displayInfo.bitsPerChannel} bpc` : ''}`],
          ['Desktop Color', app.displayInfo.desktopBits ? `${app.displayInfo.desktopBits}-bit` : '—'],
          ['Dynamic Range', app.displayInfo.dynamicRange || '—'],
          ['VRR', app.displayInfo.vrr || '—'],
          // HDR output specifics — only shown while HDR is active. Peak
          // luminance is only meaningful then; hide it in SDR.
          ...(app.hdrStatus === 'hdr' ? [['HDR Max Luminance', hdrLuminance]] : []),
          // SDR white level is used as the reference for the Brightness
          // strip's HDR cells. Chromium froze the value at WebView startup,
          // so the strip encodes against that frozen base
          // (app.sdrWhiteBaseNits), not the live slider — show the base so
          // the row matches what the strip renders.
          ...(app.hdrStatus === 'hdr' ? [['SDR White Level', `${Math.round(app.sdrWhiteBaseNits ?? app.sdrWhiteNits)} nits`]] : []),
        ],
      },
    ];
  });

  // Refresh the environment info once when this tab becomes active. TabSystem
  // is conditionally rendered (App.svelte: {#if}), so onMount runs every time
  // the user switches to this tab.
  onMount(() => {
    envTick.n++;
  });

  async function copySystemInfo() {
    const lines = sections.flatMap((section) => section.rows).map(([k, v]) => `${k}: ${v}`).join('\n');
    const text = `OLED Helper — System Info\n${'='.repeat(28)}\n${lines}`;
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
      copyFailed = false;
      setTimeout(() => { copied = false; }, 1500);
    } catch {
      copyFailed = true;
      setTimeout(() => { copyFailed = false; }, 1500);
    }
  }
</script>

<div class="tab-body">
  <div class="tab-heading">
    <img class="tab-icon" src={tabSystemIcon} alt="" />
    <h2>System Info</h2>
    <span class="subtitle">— Browser & display environment</span>
    <div class="tab-heading-spacer"></div>
  </div>
  <div class="separator"></div>

  <div class="diag-section">
    <div class="diag-section-header">
      <h3>System Info</h3>
      <button class="copy-btn" class:success={copied} class:error={copyFailed} onclick={copySystemInfo} title={copied ? 'Copied!' : copyFailed ? 'Copy failed' : 'Copy system info'} aria-label="Copy system info">
        {#if copied}
          <svg viewBox="0 0 16 16" fill="none"><path d="M3 8.5L6.5 12L13 4.5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
        {:else}
          <svg viewBox="0 0 16 16" fill="none"><rect x="5.5" y="5.5" width="8" height="8" rx="1.3" stroke="currentColor" stroke-width="1.4"/><path d="M3.5 10.5V3.8C3.5 3.36 3.86 3 4.3 3H10.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>
        {/if}
      </button>
    </div>
    {#each sections as section}
      <h4 class="diag-subheading">{section.title}</h4>
      <div class="diag-rows">
        {#each section.rows as [key, value]}
          <div class="diag-row"><span class="diag-key">{key}</span><span class="diag-val">{value}</span></div>
        {/each}
      </div>
    {/each}
  </div>
</div>
