// Shared keyboard capture for global hotkey fields. Combo names match the
// Rust key table in src-tauri/src/hotkeys.rs.
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
  NumLock: 'Num0',
};

// e.code keeps punctuation stable when Shift changes e.key to its shifted
// character. The Rust table contains the unshifted layout characters.
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

export function captureHotkeyCombo(event) {
  if (event.key === 'Escape' || event.key === 'Delete') {
    return { combo: 'None' };
  }
  if (['Control', 'Shift', 'Alt', 'Meta'].includes(event.key)) return null;

  const parts = [];
  if (event.metaKey) parts.push('Win');
  if (event.ctrlKey) parts.push('Ctrl');
  if (event.altKey) parts.push('Alt');
  if (event.shiftKey) parts.push('Shift');

  let name = null;
  const code = event.code || '';
  const key = event.key;
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) name = code;
  else if (/^Key[A-Z]$/.test(code)) name = code.slice(3);
  else if (/^Digit[0-9]$/.test(code)) name = code.slice(5);
  else if (/^Numpad[0-9]$/.test(code)) name = `Num${code.slice(6)}`;
  else if (code.startsWith('Numpad')) {
    const map = {
      NumpadMultiply: 'Num*',
      NumpadAdd: 'Num+',
      NumpadSubtract: 'Num-',
      NumpadDecimal: 'Num.',
      NumpadDivide: 'Num/',
    };
    name = map[code] || null;
  } else if (CODE_PUNCT[code]) name = CODE_PUNCT[code];
  else if (code === 'Space') name = 'Space';
  else if (KEY_NAMES[key]) name = KEY_NAMES[key];
  else if (typeof key === 'string' && key.length === 1) {
    name = /^[a-z]$/.test(key) ? key.toUpperCase() : key;
  }
  if (!name) return null;

  const combo = [...parts, name].join('+');
  if (parts.length === 0 && !/^F\d+$/.test(name)) {
    return { error: 'Add a modifier (Ctrl, Alt, Shift or Win) or use an F-key.' };
  }
  return { combo };
}
