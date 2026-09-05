<script>
  import { onDestroy } from 'svelte';
  import { gpuLogVersion, gpuLogSnapshot, clearGpuLog, runtimeLogSettings, setVerbosePeriodicLogging, setDimmerVerboseLogging, dimmerDetections, setDimmerDetection, debugSettings, setContextMenuEnabled } from './stores.svelte.js';
  import { invoke } from '@tauri-apps/api/core';
  import tabDebugIcon from './icons/tab_debug.png?inline';
  import TabHeading from './TabHeading.svelte';
  import ScreenDimmerState from './ScreenDimmerState.svelte';

  let logCopied = $state(false);
  let logCopyFailed = $state(false);
  let configFolderError = $state('');
  let noticeReset = $state(false);
  let gamepadTestEnabled = $state(false);
  let gamepadActive = $state(false);
  let gamepadPollTimer = null;

  async function openConfigFolder() {
    configFolderError = '';
    try {
      await invoke('open_config_folder');
    } catch (error) {
      configFolderError = String(error);
      setTimeout(() => { configFolderError = ''; }, 2500);
    }
  }

  // Live-tests the backend XInput/DirectInput detector: while enabled, polls
  // the current controller state so the user can move a gamepad and watch the
  // readout flip between idle and active.
  async function pollGamepad() {
    if (!gamepadTestEnabled) return;
    try {
      gamepadActive = await invoke('gamepad_input_active');
    } catch (e) {
      console.warn('[debug] gamepad detection poll failed', e);
      gamepadActive = false;
    }
    gamepadPollTimer = setTimeout(pollGamepad, 250);
  }

  function toggleGamepadTest(enabled) {
    gamepadTestEnabled = enabled;
    if (gamepadPollTimer) {
      clearTimeout(gamepadPollTimer);
      gamepadPollTimer = null;
    }
    gamepadActive = false;
    if (enabled) pollGamepad();
  }


  onDestroy(() => {
    if (gamepadPollTimer) clearTimeout(gamepadPollTimer);
  });

  // Re-derive off the version counter so this stays in sync without deep-watching
  // the whole log array (which caused a startup hang, see stores.svelte.js).
  // The store returns a fresh chronological snapshot from its ring buffer.
  let logEntries = $derived.by(() => { void gpuLogVersion.n; return gpuLogSnapshot(); });

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

  // Re-enable the one-time "still running in the tray" balloon so it shows
  // again on the next close-to-tray (debug/testing convenience).
  async function resetTrayNotice() {
    try {
      await invoke('reset_tray_notice');
      noticeReset = true;
      setTimeout(() => { noticeReset = false; }, 1500);
    } catch (e) {
      console.warn('[debug] reset tray notice failed', e);
    }
  }


  async function copyGpuLog() {
    const lines = logEntries.map(e => `[${formatTs(e.ts)}] [${e.level.toUpperCase()}] ${e.message}`).join('\n');
    const text = `OLED Helper: Runtime Log\n${'='.repeat(28)}\n${lines || '(empty)'}`;
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
  <TabHeading icon={tabDebugIcon} title="Debug" subtitle="Runtime & GPU diagnostics." />
  <div class="separator"></div>

  <div class="diag-section">
    <div class="diag-section-header">
      <div class="diag-runtime-heading">
        <h3>Runtime Log</h3>
        <label class="diag-verbose-toggle">
          <input type="checkbox" checked={runtimeLogSettings.verbosePeriodic} onchange={(e) => setVerbosePeriodicLogging(e.currentTarget.checked)} />
          <span>Detailed logging</span>
        </label>
        <label class="diag-verbose-toggle">
          <input type="checkbox" checked={runtimeLogSettings.dimmerVerbose} onchange={(e) => setDimmerVerboseLogging(e.currentTarget.checked)} />
          <span>Taskbar dimmer verbose</span>
        </label>
        <label class="diag-verbose-toggle">
          <input type="checkbox" checked={debugSettings.contextMenuEnabled} onchange={(e) => setContextMenuEnabled(e.currentTarget.checked)} />
          <span>Allow right-click menu</span>
        </label>
      </div>
      <div class="diag-header-actions">
        <button class="copy-btn copy-btn-label" class:success={logCopied} class:error={logCopyFailed} onclick={copyGpuLog} title={logCopied ? 'Copied!' : logCopyFailed ? 'Copy failed' : 'Copy log text'} aria-label="Copy log text">
          <svg viewBox="0 0 16 16" fill="none"><rect x="5.5" y="5.5" width="8" height="8" rx="1.3" stroke="currentColor" stroke-width="1.4"/><path d="M3.5 10.5V3.8C3.5 3.36 3.86 3 4.3 3H10.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>
          <span>{logCopied ? 'Copied!' : 'Copy log'}</span>
        </button>
        <button class="copy-btn" onclick={clearGpuLog} title="Clear log" aria-label="Clear log">
          <svg viewBox="0 0 16 16" fill="none"><path d="M3.5 4.5H12.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/><path d="M6 4.5V3.3C6 2.9 6.3 2.6 6.7 2.6H9.3C9.7 2.6 10 2.9 10 3.3V4.5" stroke="currentColor" stroke-width="1.4"/><path d="M4.5 4.5L5 12.4C5 12.8 5.4 13.1 5.8 13.1H10.2C10.6 13.1 11 12.8 11 12.4L11.5 4.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>
        </button>
        <button class="copy-btn config-folder-btn" class:error={configFolderError} onclick={openConfigFolder} title={configFolderError || 'Open config folder'} aria-label="Open config folder">
          <svg viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M2.5 4.5C2.5 3.95 2.95 3.5 3.5 3.5H6.3L7.5 4.8H12.5C13.05 4.8 13.5 5.25 13.5 5.8V11.5C13.5 12.05 13.05 12.5 12.5 12.5H3.5C2.95 12.5 2.5 12.05 2.5 11.5V4.5Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><path d="M2.8 6H13.2" stroke="currentColor" stroke-width="1.3"/></svg>
        </button>
        <button class="copy-btn" class:success={noticeReset} onclick={resetTrayNotice} title={noticeReset ? 'Tray notice reset!' : 'Reset tray notice: show the tray balloon again on the next close-to-tray'} aria-label="Reset tray notice">
          {#if noticeReset}
            <svg viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M3 8.5L6.5 12L13 4.5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
          {:else}
            <svg viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M15.3 3v4h-4" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/><path d="M13.7 10A6 6 0 1 1 12.3 3.8L15.3 7" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>
          {/if}
        </button>
      </div>
    </div>

    <ScreenDimmerState />

    <div class="diag-detection-bar">
      <span class="diag-detection-label">DETECTION TOGGLES</span>
      <span class="diag-detection-sep" aria-hidden="true"></span>
      <label class="diag-detection-toggle">
        <input type="checkbox" checked={dimmerDetections.fullscreen} onchange={(e) => setDimmerDetection('fullscreen', e.currentTarget.checked)} />
        <span>Fullscreen</span>
      </label>
      <label class="diag-detection-toggle">
        <input type="checkbox" checked={dimmerDetections.taskbarCache} onchange={(e) => setDimmerDetection('taskbarCache', e.currentTarget.checked)} />
        <span>Taskbar cache</span>
      </label>
      <label class="diag-detection-toggle">
        <input type="checkbox" checked={dimmerDetections.zorder} onchange={(e) => setDimmerDetection('zorder', e.currentTarget.checked)} />
        <span>Z-order repair</span>
      </label>
      <label class="diag-detection-toggle">
        <input type="checkbox" checked={dimmerDetections.cursor} onchange={(e) => setDimmerDetection('cursor', e.currentTarget.checked)} />
        <span>Cursor polling</span>
      </label>
      <label class="diag-detection-toggle">
        <input type="checkbox" checked={dimmerDetections.mouseHook} onchange={(e) => setDimmerDetection('mouseHook', e.currentTarget.checked)} />
        <span>Mouse hook</span>
      </label>
      <label class="diag-detection-toggle">
        <input type="checkbox" checked={gamepadTestEnabled} onchange={(e) => toggleGamepadTest(e.currentTarget.checked)} />
        <span>Gamepad test</span>
      </label>
      {#if gamepadTestEnabled}
        <span class="diag-gamepad-status" class:diag-gamepad-active={gamepadActive}>{gamepadActive ? 'ACTIVE' : 'IDLE'}</span>
      {/if}
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
