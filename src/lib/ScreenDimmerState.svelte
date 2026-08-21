<script>
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';

  // Live screen-dimmer state readout; null until the first poll lands.
  // Shared by the Debug tab and the Screen Dimmer tab so both show the same
  // watcher-published state: whether the dim is active and, if not, what is
  // preventing it.
  let state = $state(null);

  async function refresh() {
    try {
      state = await invoke('power_get_screen_dimmer_diagnostics');
    } catch (e) {
      console.warn('[screen-dimmer] state read failed', e);
    }
  }

  onMount(() => {
    refresh();
    // The watcher emits this the moment the dimmed flag flips, so the
    // Dimming/Blocked transitions appear in lockstep with the screen instead
    // of trailing it by up to a poll interval.
    let unlisten;
    listen('screen-dimmer-state-changed', () => refresh()).then((fn) => { unlisten = fn; });
    // The one-second poll serves the idle countdown (whole seconds) and acts
    // as a safety net; the instant transitions come from the event above, so
    // this cadence is only an IPC round-trip per second. The powercfg probe
    // behind the culprit list is rate-limited to once per second in the
    // backend.
    const timer = setInterval(refresh, 1000);
    return () => {
      clearInterval(timer);
      unlisten?.();
    };
  });
</script>

<div class="diag-detection-bar">
  <span class="diag-detection-label">SCREEN DIMMER</span>
  <span class="diag-detection-sep" aria-hidden="true"></span>
  {#if state && state.dimmed}
    <span class="diag-dim-status diag-dim-on">● Dimming</span>
  {:else if !state || !state.enabled}
    <span class="diag-dim-status diag-dim-off">● Off</span>
  {:else if !state.elevated}
    <span class="diag-dim-status diag-dim-blocked">● Blocked</span>
  {:else if state.timeoutSeconds === 0}
    <span class="diag-dim-status">● Initializing…</span>
  {:else if state.reasons.length > 0}
    <!-- A blocker (fullscreen app, display power request, reference mode, ...)
         means the dim will not happen even when the timer reaches its end, so
         the countdown is pointless while it is active. The timer keeps running
         underneath; when the blocker clears, the countdown resumes. -->
    <span class="diag-dim-status diag-dim-blocked">● Blocked</span>
  {:else if state.idleSeconds >= state.timeoutSeconds}
    <span class="diag-dim-status diag-dim-blocked">● Blocked</span>
  {:else}
    <span class="diag-dim-status">● Waiting: idle {state.idleSeconds}s of {state.timeoutSeconds}s</span>
  {/if}
  {#if state && state.reasons.length > 0}
    {#each state.reasons as reason}
      <span class="diag-dim-reason">• {reason}</span>
    {/each}
  {/if}
</div>
