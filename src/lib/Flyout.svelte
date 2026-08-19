<script>
  // Standalone tray "Keep display mode?" confirmation, rendered in its own
  // small always-on-top WebviewWindow (see src-tauri/src/flyout_window.rs).
  // This is intentionally the SAME markup/classes as the in-app modal in
  // App.svelte (.modal-backdrop.mode-confirm-backdrop / .modal.mode-confirm-modal)
  // so the two never visually drift; they share app.css, not a hand-copied
  // palette. Unlike the in-app modal there is no backdrop click-through
  // concern (the window IS the modal, sized to fit it exactly), so the
  // backdrop wrapper is kept only for the shared CSS classes, not for a
  // dimming overlay.
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';

  // ── Timing constants ──────────────────────────────────────────────────────
  // Single source of truth for the JS-side timers. The CSS animation
  // durations live in src/app.css as --mode-confirm-* custom properties
  // (shared with the main window's in-app modal); keep these values in sync
  // there.
  //
  //   FLYOUT_ENTRANCE_DWELL_MS      == --mode-confirm-entrance-dwell
  //   FLYOUT_VISIBLE_FALLBACK_MS    >= entrance duration + entrance dwell
  //   FLYOUT_EXIT_FAILSAFE_MS       >= --mode-confirm-exit-duration (exit
  //                                   hide is normally driven by the
  //                                   animationend event; only a failsafe)
  const FLYOUT_ENTRANCE_DWELL_MS = 300;
  const FLYOUT_VISIBLE_FALLBACK_MS = 1600; // >= entrance duration (1000ms) + dwell (300ms)
  const FLYOUT_EXIT_FAILSAFE_MS = 600;

  function diag(message) {
    // Temporary diagnostic logging while bringing up the new flyout window
    // routes through the same log_frontend bridge main.js uses, so these
    // lines land in the terminal/file log next to everything else.
    invoke('log_frontend', { level: 'info', message }).catch(() => {});
  }

  let mode = $state(null); // { width, height, refresh_hz, monitor_name, confirmation_seconds }
  let remaining = $state(0);
  // Total confirmation duration, drives the bottom timer bar's drain. Kept
  // separately from `remaining` because the bar animates over the full span
  // while `remaining` only holds the whole seconds left.
  let totalSeconds = $state(15);
  let busy = $state(false);
  let visible = $state(false); // becomes true only after Rust confirms the native window is shown
  let exiting = $state(false); // plays the exit ease before the window hides
  let timer = null;
  let shownFallbackTimer = null; // safety net: never leave the modal stuck at opacity 0
  let keepBtnEl = $state(null);
  let revertBtnEl = $state(null);

  const win = getCurrentWindow();

  // Keyboard handling, matching Windows' own display-mode confirm dialog:
  // Revert is the safe default action, so it gets initial focus and Enter /
  // Space on it (or Enter/Escape with no focused button) reverts immediately.
  // Tab is trapped between the two buttons so focus can never wander out of
  // the tiny window into the transparent webview behind it.
  function onDialogKeydown(e) {
    if (busy) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      rollback();
    } else if (e.key === 'Enter') {
      // A focused button handles Enter natively (fires its click); without
      // one, Enter falls back to the default action.
      if (e.target?.closest?.('button')) return;
      e.preventDefault();
      rollback();
    } else if (e.key === 'Tab') {
      const first = keepBtnEl;
      const last = revertBtnEl;
      if (!first || !last) return;
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    }
  }

  // Focus the default action (Revert) once the modal is actually visible on
  // screen. The window activation itself is handled natively in Rust
  // (finish_show drops SWP_NOACTIVATE); this moves focus to the button in
  // the DOM so Enter/Space and the focus ring land on the right control.
  $effect(() => {
    if (visible && mode) {
      const focusId = setTimeout(() => revertBtnEl?.focus(), 0);
      return () => clearTimeout(focusId);
    }
  });

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
  // window becomes visible (flyout-shown), it would finish before the eye
  // registers the popup, so the user would never see it. Wait a short dwell
  // after the window is on screen, then trigger the animation. Matches
  // --mode-confirm-entrance-dwell in app.css.
  function showWithEntranceDelay() {
    clearShownFallback();
    shownFallbackTimer = setTimeout(() => {
      visible = true;
      shownFallbackTimer = null;
    }, FLYOUT_ENTRANCE_DWELL_MS);
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
    totalSeconds = Math.max(1, seconds);
    timer = setInterval(() => {
      const left = Math.max(0, seconds - Math.ceil((Date.now() - started) / 1000));
      remaining = left;
      if (left <= 0) {
        clearTimer();
        if (!busy) rollback();
      }
    }, 250);
  }

  // Shared close path for Keep and Revert. The exit ease must play FIRST
  // with nothing running concurrently: confirm_mode_change / rollback both
  // re-apply the display mode via ChangeDisplaySettingsExW, and doing that
  // while the animation plays stalls the compositor and makes it stutter.
  // So the order is: play the exit animation (smooth), hide the window, and
  // only then run the mode-change invoke, its display work now happens
  // invisibly. Clicking still feels instant because the exit starts
  // immediately. If the invoke fails afterward the window is already gone;
  // the recovery watchdog still restores the old mode at its deadline.
  async function closeFlyout(action) {
    if (busy || !mode) return;
    busy = true;
    const modal = document.querySelector('.modal-backdrop .modal');
    exiting = true;
    const exitFinished = modal
      ? new Promise((resolve) => {
          let settled = false;
          const finish = () => {
            if (settled) return;
            settled = true;
            clearTimeout(failsafe);
            modal.removeEventListener('animationend', onEnd);
            modal.removeEventListener('animationcancel', onCancel);
            resolve();
          };
          // Only the exit animation ends the dialog; a leftover entrance
          // animationend (mode-confirm-rise-in) must not resolve this early.
          const onEnd = (event) => {
            if (event.animationName === 'mode-confirm-exit-in') finish();
          };
          // If the exit class is removed before the animation completes
          // (error path), the exit is cancelled rather than ended. Resolve
          // and detach there too, or repeated attempts would accumulate
          // listeners on the same modal.
          const onCancel = (event) => {
            if (event.animationName === 'mode-confirm-exit-in') finish();
          };
          modal.addEventListener('animationend', onEnd);
          modal.addEventListener('animationcancel', onCancel);
          // Safety net: never wait forever if the event is missed.
          const failsafe = setTimeout(finish, FLYOUT_EXIT_FAILSAFE_MS);
        })
      : Promise.resolve();
    try {
      // 1. Exit animation plays smoothly, nothing concurrent yet.
      await exitFinished;
      clearTimer();
      // 2. Hide the window before touching the display.
      await win.hide();
      invoke('flyout_hidden').catch(() => {});
      // 3. Now the mode change; the display re-apply happens invisibly.
      await invoke(action);
      mode = null;
      exiting = false;
    } catch (e) {
      // The window is already hidden, so there is nowhere to show the error.
      // Log it; the recovery watchdog still restores the old mode if this
      // transaction was never committed.
      diag(`[flyout] ${action} failed after hide: ${e}`);
      mode = null;
      exiting = false;
    } finally {
      busy = false;
    }
  }

  async function confirm() {
    await closeFlyout('confirm_display_mode_change');
  }

  async function rollback() {
    await closeFlyout('rollback_display_mode_change');
  }

  onMount(() => {
    let unlistenShow;
    let unlistenShown;
    let unlistenHdr;
    // Rust shows the window and immediately emits the mode payload, rather
    // than the frontend polling, mirrors the old native flyout's "show(mode)"
    // entry point but as a normal Tauri event.
    listen('flyout-show', (event) => {
      diag('[flyout] flyout-show event received');
      const payload = event.payload;
      busy = false;
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
      }, FLYOUT_VISIBLE_FALLBACK_MS);
    }).then((stop) => {
      unlistenShow = stop;
      diag('[flyout] listening for flyout-show');
      // Signal only after listen() has completed. Rust can now safely emit the
      // first payload while the window remains hidden.
      invoke('flyout_frontend_ready').catch(() => {});
    }).catch((e) => diag(`[flyout] listen(flyout-show) failed: ${e}`));
    // Fired by Rust right after the native SetWindowPos(...SWP_SHOWWINDOW)
    // call, i.e. the instant the window is actually visible on screen. Only
    // now is it safe to start the CSS entrance transition, starting it any
    // earlier (mount/mode-set) would let it finish playing while still
    // hidden behind the native show call, so the user would just see the
    // final state pop in with no visible motion.
    listen('flyout-shown', () => {
      diag('[flyout] flyout-shown event received');
      // Dwell briefly after the window becomes visible, then play the
      // entrance animation so the user actually sees it.
      showWithEntranceDelay();
      // The native window was activated in Rust (finish_show), but make sure
      // the activation sticks for the WebView so keyboard input (Enter /
      // Escape / Tab) reaches the dialog immediately.
      win.setFocus().catch(() => {});
    }).then((stop) => { unlistenShown = stop; }).catch((e) => diag(`[flyout] listen(flyout-shown) failed: ${e}`));
    // Keep the accent (--accent-hdr vs --accent-sdr) in sync with the main
    // window's HDR/SDR status. The set_flyout_hdr command is registered but
    // never actually invoked, so probe the mode directly with the same
    // matchMedia query the main app and tray menu use; the flyout-hdr event
    // listener below is kept as a harmless belt-and-braces fallback.
    let hdrMq = null;
    function syncFlyoutHdrAccent() {
      // Reuse the created query when available; fall back to a fresh probe
      // for the synchronous initial call before hdrMq is assigned.
      let hdr = false;
      try { hdr = (hdrMq ?? window.matchMedia('(dynamic-range: high)')).matches; } catch {}
      document.documentElement.setAttribute('data-hdr', hdr ? 'true' : 'false');
    }
    syncFlyoutHdrAccent();
    try {
      hdrMq = window.matchMedia('(dynamic-range: high)');
      hdrMq.addEventListener('change', syncFlyoutHdrAccent);
    } catch {}
    listen('flyout-hdr', (event) => {
      document.documentElement.setAttribute('data-hdr', event.payload ? 'true' : 'false');
    }).then((stop) => { unlistenHdr = stop; }).catch(() => {});
    return () => {
      unlistenShow?.();
      unlistenShown?.();
      unlistenHdr?.();
      try { hdrMq?.removeEventListener('change', syncFlyoutHdrAccent); } catch {}
      clearTimer();
      clearShownFallback();
    };
  });
</script>

{#if mode}
  <div class="modal-backdrop mode-confirm-backdrop flyout-backdrop" role="presentation">
    <div class="modal mode-confirm-modal" class:flyout-visible={visible} class:flyout-exiting={exiting} role="dialog" aria-modal="true" aria-labelledby="mode-confirm-title" aria-describedby="mode-confirm-desc" tabindex="-1" onkeydown={onDialogKeydown}>
      <h2 id="mode-confirm-title" class="modal-title">Keep display mode?</h2>
      <p id="mode-confirm-desc" class="modal-desc">
        {mode.monitor_name || 'Current monitor'} is temporarily using
        <strong>{mode.width}×{mode.height} at {mode.refresh_hz} Hz</strong>.
        Confirm if the picture looks correct. It will automatically revert in {remaining}s.
      </p>
      <div class="modal-actions">
        <button bind:this={keepBtnEl} class="modal-btn" onclick={confirm} disabled={busy}>Keep this mode</button>
        <button bind:this={revertBtnEl} class="modal-btn modal-btn-cancel" onclick={rollback} disabled={busy}>Revert (<span class="modal-btn-countdown">{remaining}</span>s)</button>
      </div>
      <div class="mode-confirm-timerbar" style="animation-duration: {totalSeconds}s" role="presentation" aria-hidden="true"></div>
    </div>
  </div>
{/if}
