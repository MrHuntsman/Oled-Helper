<script>
  // Standalone tray "Keep display mode?" confirmation, rendered in its own
  // small always-on-top WebviewWindow (see src-tauri/src/flyout_window.rs).
  // This is intentionally the SAME markup/classes as the in-app modal in
  // App.svelte (.modal-backdrop.mode-confirm-backdrop / .modal.mode-confirm-modal)
  // so the two never visually drift — they share app.css, not a hand-copied
  // palette. Unlike the in-app modal there is no backdrop click-through
  // concern (the window IS the modal, sized to fit it exactly), so the
  // backdrop wrapper is kept only for the shared CSS classes, not for a
  // dimming overlay.
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';

  function diag(message) {
    // Temporary diagnostic logging while bringing up the new flyout window —
    // routes through the same log_frontend bridge main.js uses, so these
    // lines land in the terminal/file log next to everything else.
    invoke('log_frontend', { level: 'info', message }).catch(() => {});
  }

  let mode = $state(null); // { width, height, refresh_hz, monitor_name, confirmation_seconds }
  let remaining = $state(0);
  let busy = $state(false);
  let error = $state('');
  let visible = $state(false); // becomes true only after Rust confirms the native window is shown
  let timer = null;
  let shownFallbackTimer = null; // safety net: never leave the modal stuck at opacity 0

  const win = getCurrentWindow();

  function clearTimer() {
    if (timer) clearInterval(timer);
    timer = null;
  }

  function clearShownFallback() {
    if (shownFallbackTimer) {
      clearTimeout(shownFallbackTimer);
      shownFallbackTimer = null;
    }
  }

  // The entrance animation must not start at the exact moment the native
  // window becomes visible (flyout-shown) — it would finish in 0.22s before
  // the eye registers the popup, so the user would never see it. Wait a short
  // dwell after the window is on screen, then trigger the animation.
  function showWithEntranceDelay() {
    clearShownFallback();
    shownFallbackTimer = setTimeout(() => {
      visible = true;
      shownFallbackTimer = null;
    }, 300);
  }

  function notifyLayoutReady() {
    // Let Svelte commit the mode payload and let the browser calculate the
    // modal's final layout before Rust performs its last hidden native
    // placement pass.
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        invoke('flyout_layout_ready').catch(() => {});
      });
    });
  }

  function startCountdown(seconds, startedAtMs) {
    clearTimer();
    const started = startedAtMs || Date.now();
    remaining = seconds;
    timer = setInterval(() => {
      const left = Math.max(0, seconds - Math.ceil((Date.now() - started) / 1000));
      remaining = left;
      if (left <= 0) {
        clearTimer();
        if (!busy) rollback();
      }
    }, 250);
  }

  async function confirm() {
    if (busy || !mode) return;
    busy = true;
    error = '';
    try {
      await invoke('confirm_display_mode_change');
      clearTimer();
      await win.hide();
      invoke('flyout_hidden').catch(() => {});
      mode = null;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function rollback() {
    if (busy || !mode) return;
    busy = true;
    error = '';
    try {
      await invoke('rollback_display_mode_change');
      clearTimer();
      await win.hide();
      invoke('flyout_hidden').catch(() => {});
      mode = null;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  onMount(() => {
    let unlistenShow;
    let unlistenShown;
    let unlistenHdr;
    // Rust shows the window and immediately emits the mode payload, rather
    // than the frontend polling — mirrors the old native flyout's "show(mode)"
    // entry point but as a normal Tauri event.
    listen('flyout-show', (event) => {
      diag('[flyout] flyout-show event received');
      const payload = event.payload;
      busy = false;
      error = '';
      visible = false;
      mode = payload;
      startCountdown(Number(payload?.confirmation_seconds) || 15);
      notifyLayoutReady();
      // Safety net: if flyout-shown is ever dropped (IPC hiccup, listener
      // timing), force visibility after a generous timeout so the dialog can
      // never remain blank.
      clearShownFallback();
      shownFallbackTimer = setTimeout(() => {
        visible = true;
        shownFallbackTimer = null;
      }, 1200);
    }).then((stop) => {
      unlistenShow = stop;
      diag('[flyout] listening for flyout-show');
      // Signal only after listen() has completed. Rust can now safely emit the
      // first payload while the window remains hidden.
      invoke('flyout_frontend_ready').catch(() => {});
    }).catch((e) => diag(`[flyout] listen(flyout-show) failed: ${e}`));
    // Fired by Rust right after the native SetWindowPos(...SWP_SHOWWINDOW)
    // call, i.e. the instant the window is actually visible on screen. Only
    // now is it safe to start the CSS entrance transition — starting it any
    // earlier (mount/mode-set) would let it finish playing while still
    // hidden behind the native show call, so the user would just see the
    // final state pop in with no visible motion.
    listen('flyout-shown', () => {
      diag('[flyout] flyout-shown event received');
      // Dwell briefly after the window becomes visible, then play the
      // entrance animation so the user actually sees it.
      showWithEntranceDelay();
    }).then((stop) => { unlistenShown = stop; }).catch((e) => diag(`[flyout] listen(flyout-shown) failed: ${e}`));
    // Keep the accent (--accent-hdr vs --accent-sdr) in sync with the main
    // window's HDR/SDR status, same as the old set_flyout_hdr command did.
    listen('flyout-hdr', (event) => {
      document.documentElement.setAttribute('data-hdr', event.payload ? 'true' : 'false');
    }).then((stop) => { unlistenHdr = stop; }).catch(() => {});
    return () => {
      unlistenShow?.();
      unlistenShown?.();
      unlistenHdr?.();
      clearTimer();
      clearShownFallback();
    };
  });
</script>

{#if mode}
  <div class="modal-backdrop mode-confirm-backdrop flyout-backdrop" role="presentation">
    <div class="modal mode-confirm-modal" class:flyout-visible={visible} role="dialog" aria-modal="true" aria-labelledby="mode-confirm-title" tabindex="-1">
      <h2 id="mode-confirm-title" class="modal-title">Keep display mode?</h2>
      <p class="modal-desc">
        {mode.monitor_name || 'Current monitor'} is temporarily using
        <strong>{mode.width}×{mode.height} at {mode.refresh_hz} Hz</strong>.
        Confirm if the picture looks correct. It will automatically revert in {remaining}s.
      </p>
      {#if error}<p class="modal-error">Rollback failed: {error}</p>{/if}
      <div class="modal-actions">
        <button class="modal-btn" onclick={confirm} disabled={busy}>Keep this mode</button>
        <button class="modal-btn modal-btn-cancel" onclick={rollback} disabled={busy}>Revert ({remaining}s)</button>
      </div>
    </div>
  </div>
{/if}

<style>
  /* The flyout window is sized to fit the modal exactly and is itself
     transparent (see flyout.html), so the full-viewport dimmed backdrop the
     in-app version uses would just paint a translucent black rectangle the
     size of the tiny window. Drop the dimming/centering and let the modal
     fill the window instead. */
  .flyout-backdrop {
    position: static;
    inset: auto;
    z-index: auto;
    background: transparent;
    display: block;
    animation: none;
  }
  .flyout-backdrop .modal {
    width: 380px;
    /* Hold the entrance animation's "from" state — no animation plays until
       .flyout-visible is added (see the flyout-shown listener in <script>),
       which happens only after the native window is already visible. Without
       this hold, the animation would run to completion while the window is
       still hidden behind SetWindowPos and the user would only ever see the
       final frame pop in with no motion. */
    opacity: 0;
    transform: translateY(10px);
    animation: none;
  }
  .flyout-backdrop .modal.flyout-visible {
    opacity: 1;
    transform: none;
    /* Dedicated flyout entrance: slides up from below and fades in, like it
       is rising out of the taskbar. Cannot reuse the in-app modal-in
       keyframe (app.css) for the upward slide, because that keyframe's `to`
       state is only the element's computed style — it defines just a `from`
       — and the shared keyframe is intentionally generic. `to`/`from` both
       explicit here, so the animation end state is authoritative and the
       explicit end-state declarations above are redundant but harmless. */
    animation: flyout-rise-in 0.32s cubic-bezier(0.22, 1, 0.36, 1) forwards;
  }
  @keyframes flyout-rise-in {
    from {
      opacity: 0;
      transform: translateY(14px);
    }
    to {
      opacity: 1;
      transform: none;
    }
  }
</style>
