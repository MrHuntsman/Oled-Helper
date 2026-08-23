<script>
  // Inlined data URI, see App.svelte for why icons never hit the network.
  import tabHotkeysIcon from './icons/tab_hotkeys.png?inline';
  import { emit } from '@tauri-apps/api/event';
  import { hotkeys, setHotkey, resetHotkeys, displayFavorites, favoriteHotkeyAction, monitors } from './stores.svelte.js';
  import ConfirmModal from './ConfirmModal.svelte';

  // The action list mirrors ACTIONS in src-tauri/src/hotkeys.rs. The combo
  // strings produced by captureCombo must match the Rust parse_hotkey table.
  const ROWS = [
    { key: 'toggle_black_crush', label: 'Toggle Black Crush', desc: 'Enable or disable the black level adjustment.' },
    { key: 'decrease_black_crush', label: 'Decrease Black Level', desc: 'Lower the black level. Hold to repeat.' },
    { key: 'increase_black_crush', label: 'Increase Black Level', desc: 'Raise the black level. Hold to repeat.' },
    { key: 'hold_compare', label: 'Hold to Compare', desc: 'Temporarily disable black level while held. Restores on release.' },
    { key: 'toggle_taskbar_dimmer', label: 'Toggle Taskbar Dimmer', desc: 'Enable or disable the taskbar overlay.' },
    { key: 'toggle_screen_dimmer', label: 'Toggle Screen Dimmer', desc: 'Enable or disable the screen dimmer overlay.' },
    { key: 'toggle_hdr', label: 'Toggle HDR/SDR', desc: 'Switch Windows HDR on or off.' },
    { key: 'turn_off_display', label: 'Turn Off Display', desc: 'Instantly turn the display off.' },
    { key: 'decrease_sdr_brightness', label: 'Decrease SDR Content Brightness (−50 nits)', desc: 'Lower Windows SDR content brightness by 50 nits.' },
    { key: 'increase_sdr_brightness', label: 'Increase SDR Content Brightness (+50 nits)', desc: 'Raise Windows SDR content brightness by 50 nits.' },
  ];

  const SECTIONS = [
    { title: 'Black Crush Tweak', keys: ['toggle_black_crush', 'decrease_black_crush', 'increase_black_crush', 'hold_compare'] },
    { title: 'Taskbar Dimmer', keys: ['toggle_taskbar_dimmer'] },
    { title: 'Screen Dimmer', keys: ['toggle_screen_dimmer'] },
    { title: 'System', keys: ['toggle_hdr', 'turn_off_display'] },
    { title: 'Windows HDR', keys: ['decrease_sdr_brightness', 'increase_sdr_brightness'] },
  ];

  // Capturing state: the key of the row currently listening for a combo.
  let capturing = $state(null);
  let captureError = $state('');

  // Confirmation for the destructive "Restore Tab Defaults" action.
  let resetConfirmOpen = $state(false);

  function binding(action) {
    return hotkeys[action] || 'None';
  }

  // A favorite whose saved monitor is not currently connected (legacy
  // favorites without a monitor scope always target the primary, so they are
  // never "orphaned"). Matches by EDID monitor_id first (stable across
  // resolution changes), falling back to the sig's physical-size+DPI portion
  // so a favorite saved at 4K still resolves after the same panel switches
  // to an ultrawide mode.
  function stableSigPart(sig) {
    const parts = String(sig || '').split('|');
    return parts.length >= 3 ? `${parts[1]}|${parts[2]}` : String(sig || '');
  }
  function favoriteDisconnected(fav) {
    if (!fav.monitorId && !fav.monitorSig) return false;
    return !monitors.some((m) =>
      (fav.monitorId && m.monitor_id === fav.monitorId)
      || (fav.monitorSig && (m.sig === fav.monitorSig || stableSigPart(m.sig) === stableSigPart(fav.monitorSig))));
  }
  function favoriteOptionLabel(fav) {
    const base = `${fav.width}×${fav.height} @ ${fav.hz} Hz`;
    const mon = fav.monitorName ? ` (${fav.monitorName})` : '';
    return favoriteDisconnected(fav) ? `${base}${mon} (not connected)` : `${base}${mon}`;
  }

  async function apply(action, combo) {
    const result = await setHotkey(action, combo);
    if (!result.ok) captureError = result.error;
    else captureError = '';
    capturing = null;
  }

  // KeyboardEvent → combo string, using the same key names as the Rust table.
  // Requires at least one modifier, or an F-key, so a plain letter/digit can
  // never hijack typing globally.
  const KEY_NAMES = {
    Enter: 'Enter',
    Tab: 'Tab',
    Backspace: 'Backspace',
    Delete: 'Delete',
    Insert: 'Insert',
    Home: 'Home',
    End: 'End',
    PageUp: 'PgUp',
    PageDown: 'PgDn',
    ArrowUp: 'Up',
    ArrowDown: 'Down',
    ArrowLeft: 'Left',
    ArrowRight: 'Right',
    ',': ',',
    '.': '.',
    '/': '/',
    ';': ';',
    "'": "'",
    '[': '[',
    ']': ']',
    '\\': '\\',
    '`': '`',
    '-': '-',
    '=': '=',
    NumLock: 'Num0', // placeholder, replaced below for digits
  };

  // e.code → key name for punctuation. e.key reports the SHIFTED character
  // when Shift is held (e.g. Shift+/ gives '?'), but the Rust table only holds
  // the unshifted layout characters, so map from e.code, which is stable.
  const CODE_PUNCT = {
    Comma: ',',
    Period: '.',
    Slash: '/',
    Semicolon: ';',
    Quote: "'",
    BracketLeft: '[',
    BracketRight: ']',
    Backslash: '\\',
    Backquote: '`',
    Minus: '-',
    Equal: '=',
  };

  function captureCombo(e) {
    if (e.key === 'Escape' || e.key === 'Delete') {
      apply(capturing, 'None');
      e.preventDefault();
      return null;
    }
    if (['Control', 'Shift', 'Alt', 'Meta'].includes(e.key)) return null;

    const parts = [];
    if (e.metaKey) parts.push('Win');
    if (e.ctrlKey) parts.push('Ctrl');
    if (e.altKey) parts.push('Alt');
    if (e.shiftKey) parts.push('Shift');

    let name = null;
    const code = e.code || '';
    const key = e.key;
    if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) name = code;          // F1-F24
    else if (/^Key[A-Z]$/.test(code)) name = code.slice(3);          // A-Z
    else if (/^Digit[0-9]$/.test(code)) name = code.slice(5);        // 0-9
    else if (/^Numpad[0-9]$/.test(code)) name = `Num${code.slice(6)}`;
    else if (code.startsWith('Numpad')) {
      const map = { NumpadMultiply: 'Num*', NumpadAdd: 'Num+', NumpadSubtract: 'Num-', NumpadDecimal: 'Num.', NumpadDivide: 'Num/' };
      name = map[code] || null;
    } else if (CODE_PUNCT[code]) name = CODE_PUNCT[code];            // punctuation
    else if (code === 'Space') name = 'Space';                        // Space
    else if (KEY_NAMES[key]) name = KEY_NAMES[key];                  // Enter, Tab, arrows…
    else if (typeof key === 'string' && key.length === 1) {
      // Last resort (layout-specific keys): uppercase bare letters.
      name = /^[a-z]$/.test(key) ? key.toUpperCase() : key;
    }
    if (!name) return null;

    const combo = [...parts, name].join('+');
    // Guard: a bare letter/digit without any modifier is refused. F-keys are
    // allowed alone (they don't collide with typing).
    if (parts.length === 0 && !/^F\d+$/.test(name)) {
      captureError = 'Add a modifier (Ctrl, Alt, Shift or Win) or use an F-key.';
      return null;
    }
    apply(capturing, combo);
    e.preventDefault();
    return combo;
  }

  $effect(() => {
    if (!capturing) return;
    function onKeyDown(e) {
      if (captureCombo(e)) return;
      e.preventDefault();
      e.stopPropagation();
    }
    window.addEventListener('keydown', onKeyDown, true);
    return () => window.removeEventListener('keydown', onKeyDown, true);
  });
</script>

<div class="tab-body">
  <div class="tab-heading tab-heading-2line">
    <div class="tab-heading-toggle">
      <img class="tab-icon" src={tabHotkeysIcon} alt="" />
      <div class="tab-heading-titles">
        <div class="tab-heading-title-row">
          <h2>Hotkeys</h2>
          <span class="tab-monitor-tag">Global</span>
        </div>
        <span class="subtitle">Global shortcuts for the app's main features.</span>
      </div>
    </div>
    <div class="tab-heading-spacer"></div>
    <button class="restore-defaults-btn" onclick={() => resetConfirmOpen = true}>↻ Restore Tab Defaults</button>
  </div>
  <div class="separator"></div>

  <p class="hotkey-hint hotkey-hint-top">Use key modifiers: Win, Ctrl, Alt or Shift. (F1-F24) keys can be used without a modifier.</p>

  {#each SECTIONS as section}
    <div class="hotkey-section">
      <span class="settings-row-label hotkey-section-label">{section.title}</span>
      {#each section.keys as key}
        {@const row = ROWS.find((r) => r.key === key)}
        <div class="hotkey-row">
          <div class="hotkey-row-text">
            <span class="settings-row-label">{row.label}</span>
            <span class="settings-row-desc">{row.desc}</span>
          </div>
          <div class="hotkey-row-controls">
            <button
              type="button"
              class="hotkey-pill"
              class:capturing={capturing === key}
              class:bound={binding(key) !== 'None'}
              onclick={() => { captureError = ''; capturing = capturing === key ? null : key; }}
              aria-label={`Set hotkey for ${row.label}`}>
              {capturing === key ? 'Press keys… (Esc to clear)' : binding(key)}
            </button>
            <button
              type="button"
              class="hotkey-clear"
              disabled={binding(key) === 'None'}
              onclick={() => apply(key, 'None')}
              title="Clear this hotkey"
              aria-label={`Clear hotkey for ${row.label}`}>×</button>
          </div>
        </div>
      {/each}
    </div>
  {/each}

  <div class="hotkey-section">
    <span class="settings-row-label hotkey-section-label">Favorite Resolutions</span>
    {#if displayFavorites.length === 0}
      <p class="hotkey-hint">Save a favorite from the status bar first, then bind a hotkey to it.</p>
    {:else}
      {#each displayFavorites as fav (fav.favId)}
        {@const favAction = favoriteHotkeyAction(fav.favId)}
        {@const favLabel = favoriteOptionLabel(fav)}
        <div class="hotkey-row hotkey-row-favorite">
          <div class="hotkey-row-text hotkey-row-favorite-text">
            <span class="settings-row-label" title={favLabel}>{favLabel}</span>
            <button
              type="button"
              class="hotkey-fav-remove"
              title={`Remove favorite ${favLabel}`}
              aria-label={`Remove favorite ${favLabel}`}
              onclick={() => emit('favorite-remove-request', fav)}>
              <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                <path d="M8 1.7l1.9 3.9 4.4.6-3.2 3.1.8 4.4L8 11.6 4.1 13.7l.8-4.4L1.7 6.2l4.4-.6z" fill="currentColor" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>
              </svg>
            </button>
          </div>
          <div class="hotkey-row-controls">
            <button
              type="button"
              class="hotkey-pill"
              class:capturing={capturing === favAction}
              class:bound={binding(favAction) !== 'None'}
              onclick={() => { captureError = ''; capturing = capturing === favAction ? null : favAction; }}
              aria-label={`Set hotkey for ${favLabel}`}>
              {capturing === favAction ? 'Press keys… (Esc to clear)' : binding(favAction)}
            </button>
            <button
              type="button"
              class="hotkey-clear"
              disabled={binding(favAction) === 'None'}
              onclick={() => apply(favAction, 'None')}
              title="Clear this hotkey"
              aria-label={`Clear hotkey for ${favLabel}`}>×</button>
          </div>
        </div>
      {/each}
    {/if}
  </div>

  {#if captureError}
    <p class="hotkey-error" role="status">{captureError}</p>
  {/if}

  {#if resetConfirmOpen}
    <ConfirmModal
      title="Restore hotkey defaults?"
      desc="This clears every hotkey combo back to its default."
      confirmLabel="Restore"
      onConfirm={resetHotkeys}
      onClose={() => resetConfirmOpen = false} />
  {/if}
</div>
