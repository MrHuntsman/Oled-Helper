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

  // Depends on envTick (bumped on tab mount) so the table refreshes each time
  // this tab is opened — navigator.*/screen.* are plain globals Svelte can't
  // track on its own.
  let rows = $derived.by(() => {
    void envTick.n;
    // screen.colorDepth/pixelDepth report combined bits across R+G+B (e.g. 30
    // for a 10-bit HDR panel), which is the browser/OS convention but not
    // what people usually mean by "10-bit display". Show per-channel bits
    // instead, since that's the number normally advertised for monitors.
    const perChannelBits = screen.colorDepth ? Math.round(screen.colorDepth / 3) : null;
    return [
      ['Browser / WebView', navigator.userAgent],
      ['WebGPU Support', navigator.gpu ? 'Available' : 'Not available'],
      ['Display Type', displayType],
      // Peak luminance is only meaningful while HDR is active; hide it in SDR.
      ...(app.hdrStatus === 'hdr' ? [['HDR Max Luminance', hdrLuminance]] : []),
      ['Current Monitor', app.displayInfo.monitorName || 'Detecting...'],
      ['Monitor Device', app.displayInfo.deviceName || '—'],
      ['Native Resolution', nativeResolution],
      ['Screen Resolution', `${screen.width * devicePixelRatio}x${screen.height * devicePixelRatio}${app.displayInfo.refreshRateHz ? `@${app.displayInfo.refreshRateHz}Hz` : ''}`],
      ['Display Scaling', devicePixelRatio ? `${Math.round(devicePixelRatio * 100)}%` : '?'],
      ['Color Depth', perChannelBits ? `${perChannelBits}-bit per channel` : '?'],
    ];
  });

  // Refresh the environment info once when this tab becomes active. TabSystem
  // is conditionally rendered (App.svelte: {#if}), so onMount runs every time
  // the user switches to this tab.
  onMount(() => {
    envTick.n++;
  });

  async function copySystemInfo() {
    const lines = rows.map(([k, v]) => `${k}: ${v}`).join('\n');
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
      <h3>Browser / Environment</h3>
      <button class="copy-btn" class:success={copied} class:error={copyFailed} onclick={copySystemInfo} title={copied ? 'Copied!' : copyFailed ? 'Copy failed' : 'Copy system info'} aria-label="Copy system info">
        {#if copied}
          <svg viewBox="0 0 16 16" fill="none"><path d="M3 8.5L6.5 12L13 4.5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
        {:else}
          <svg viewBox="0 0 16 16" fill="none"><rect x="5.5" y="5.5" width="8" height="8" rx="1.3" stroke="currentColor" stroke-width="1.4"/><path d="M3.5 10.5V3.8C3.5 3.36 3.86 3 4.3 3H10.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>
        {/if}
      </button>
    </div>
    <div class="diag-rows">
      {#each rows as [key, value]}
        <div class="diag-row"><span class="diag-key">{key}</span><span class="diag-val">{value}</span></div>
      {/each}
    </div>
  </div>
</div>
