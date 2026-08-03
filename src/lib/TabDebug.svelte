<script>
  import { app, gpuLogEntries, gpuLogVersion, clearGpuLog, envTick, runtimeLogSettings, setVerbosePeriodicLogging } from './stores.svelte.js';

  let copied = $state(false);
  let copyFailed = $state(false);
  let logCopied = $state(false);
  let logCopyFailed = $state(false);

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

  // Depends on envTick (bumped every 1s in App.svelte) so this whole table
  // stays live without needing a tab switch/remount — navigator.*/screen.*
  // are plain globals Svelte can't track on its own.
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
      ['HDR Max Luminance', hdrLuminance],
      ['Screen Resolution', `${screen.width * devicePixelRatio}x${screen.height * devicePixelRatio}${app.displayInfo.refreshRateHz ? `@${app.displayInfo.refreshRateHz}Hz` : ''}`],
      ['Display Scaling', devicePixelRatio ? `${Math.round(devicePixelRatio * 100)}%` : '?'],
      ['Color Depth', perChannelBits ? `${perChannelBits}-bit per channel` : '?'],
    ];
  });

  // Re-derive off the version counter so this stays in sync without deep-watching
  // the whole log array (which caused a startup hang — see stores.svelte.js).
  // IMPORTANT: gpuLogEntries is a plain mutated-in-place array (not $state),
  // so we must return a NEW array reference here (spread) each time the
  // version bumps — returning the same reference back means Svelte's
  // equality check sees "no change" and skips re-rendering the template,
  // even though the array's contents did change (e.g. on clearGpuLog()).
  let logEntries = $derived.by(() => { void gpuLogVersion.n; return [...gpuLogEntries]; });

  let logEl = $state(null);
  let logAtBottom = $state(true);
  let showNewLogs = $state(false);

  function handleLogScroll() {
    if (!logEl) return;
    const distanceFromBottom = logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight;
    logAtBottom = distanceFromBottom <= 8;
    if (logAtBottom) showNewLogs = false;
  }

  function jumpToLatest() {
    if (!logEl) return;
    logEl.scrollTop = logEl.scrollHeight;
    logAtBottom = true;
    showNewLogs = false;
  }

  // Keep following new entries only while the user is already at the bottom.
  // Once they scroll upward, preserve their position and offer a jump button
  // instead of repeatedly pulling the viewport away from the lines they are
  // reading.
  $effect(() => {
    void logEntries.length;
    if (!logEl) return;
    if (logAtBottom) {
      logEl.scrollTop = logEl.scrollHeight;
      showNewLogs = false;
    } else {
      showNewLogs = true;
    }
  });

  function formatTs(ts) {
    const d = new Date(ts);
    const pad = (n, l = 2) => String(n).padStart(l, '0');
    return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
  }

  async function copyDebugInfo() {
    const lines = rows.map(([k, v]) => `${k}: ${v}`).join('\n');
    const text = `OLED Helper — Debug Info\n${'='.repeat(28)}\n${lines}`;
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

  async function copyGpuLog() {
    const lines = logEntries.map(e => `[${formatTs(e.ts)}] [${e.level.toUpperCase()}] ${e.message}`).join('\n');
    const text = `OLED Helper — Runtime Log\n${'='.repeat(28)}\n${lines || '(empty)'}`;
    try {
      await navigator.clipboard.writeText(text);
      logCopied = true;
      logCopyFailed = false;
      setTimeout(() => { logCopied = false; }, 1500);
    } catch {
      logCopyFailed = true;
      setTimeout(() => { logCopyFailed = false; }, 1500);
    }
  }
</script>

<div class="tab-body">
  <div class="tab-heading">
    <h2>Debug</h2>
    <span class="subtitle">— Runtime, GPU & HDR diagnostics</span>
    <div class="tab-heading-spacer"></div>
  </div>
  <div class="separator"></div>

  <div class="diag-section">
    <div class="diag-section-header">
      <h3>Browser / Environment</h3>
      <button class="copy-btn" class:success={copied} class:error={copyFailed} onclick={copyDebugInfo} title={copied ? 'Copied!' : copyFailed ? 'Copy failed' : 'Copy debug info'} aria-label="Copy debug info">
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

  <div class="separator"></div>

  <div class="diag-section">
    <div class="diag-section-header">
      <div class="diag-runtime-heading">
        <h3>Runtime Log</h3>
        <label class="diag-verbose-toggle">
          <input type="checkbox" checked={runtimeLogSettings.verbosePeriodic} onchange={(e) => setVerbosePeriodicLogging(e.currentTarget.checked)} />
          <span>Detailed logging</span>
        </label>
      </div>
      <div style="display:flex; gap:6px;">
        <button class="copy-btn" class:success={logCopied} class:error={logCopyFailed} onclick={copyGpuLog} title={logCopied ? 'Copied!' : logCopyFailed ? 'Copy failed' : 'Copy runtime log'} aria-label="Copy runtime log">
          {#if logCopied}
            <svg viewBox="0 0 16 16" fill="none"><path d="M3 8.5L6.5 12L13 4.5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
          {:else}
            <svg viewBox="0 0 16 16" fill="none"><rect x="5.5" y="5.5" width="8" height="8" rx="1.3" stroke="currentColor" stroke-width="1.4"/><path d="M3.5 10.5V3.8C3.5 3.36 3.86 3 4.3 3H10.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>
          {/if}
        </button>
        <button class="copy-btn" onclick={clearGpuLog} title="Clear log" aria-label="Clear log">
          <svg viewBox="0 0 16 16" fill="none"><path d="M3.5 4.5H12.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/><path d="M6 4.5V3.3C6 2.9 6.3 2.6 6.7 2.6H9.3C9.7 2.6 10 2.9 10 3.3V4.5" stroke="currentColor" stroke-width="1.4"/><path d="M4.5 4.5L5 12.4C5 12.8 5.4 13.1 5.8 13.1H10.2C10.6 13.1 11 12.8 11 12.4L11.5 4.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>
        </button>
      </div>
    </div>
    <div class="diag-log-wrap">
      <div class="diag-log" bind:this={logEl} onscroll={handleLogScroll}>
        {#if logEntries.length === 0}
          <p class="gpu-status" style="margin:0;">No log entries yet.</p>
        {:else}
          {#each logEntries as entry}
            <div class="diag-entry" class:diag-error={entry.level === 'error'} class:diag-warn={entry.level === 'warn'}>
              <span class="diag-ts">{formatTs(entry.ts)}</span>
              <span class="diag-level">{entry.level}</span>
              <span class="diag-msg">{entry.message}</span>
            </div>
          {/each}
        {/if}
      </div>
      <button class="diag-log-new" class:visible={showNewLogs} onclick={jumpToLatest} aria-label="Jump to latest runtime logs">New logs ↓</button>
    </div>
  </div>
</div>
