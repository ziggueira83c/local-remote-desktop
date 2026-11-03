// KeyboardEvent.code (tecla física) → USB HID usage (página 0x07).
// Enviar a tecla física faz o layout do computador controlado (ABNT2, US...) decidir o caractere.

const MAP: Record<string, number> = {
  Enter: 0x28, Escape: 0x29, Backspace: 0x2a, Tab: 0x2b, Space: 0x2c,
  Minus: 0x2d, Equal: 0x2e, BracketLeft: 0x2f, BracketRight: 0x30, Backslash: 0x31,
  IntlHash: 0x32, Semicolon: 0x33, Quote: 0x34, Backquote: 0x35, Comma: 0x36,
  Period: 0x37, Slash: 0x38, CapsLock: 0x39,
  F1: 0x3a, F2: 0x3b, F3: 0x3c, F4: 0x3d, F5: 0x3e, F6: 0x3f,
  F7: 0x40, F8: 0x41, F9: 0x42, F10: 0x43, F11: 0x44, F12: 0x45,
  PrintScreen: 0x46, ScrollLock: 0x47, Pause: 0x48,
  Insert: 0x49, Home: 0x4a, PageUp: 0x4b, Delete: 0x4c, End: 0x4d, PageDown: 0x4e,
  ArrowRight: 0x4f, ArrowLeft: 0x50, ArrowDown: 0x51, ArrowUp: 0x52,
  NumLock: 0x53, NumpadDivide: 0x54, NumpadMultiply: 0x55, NumpadSubtract: 0x56,
  NumpadAdd: 0x57, NumpadEnter: 0x58,
  Numpad1: 0x59, Numpad2: 0x5a, Numpad3: 0x5b, Numpad4: 0x5c, Numpad5: 0x5d,
  Numpad6: 0x5e, Numpad7: 0x5f, Numpad8: 0x60, Numpad9: 0x61, Numpad0: 0x62,
  NumpadDecimal: 0x63, IntlBackslash: 0x64, ContextMenu: 0x65, NumpadEqual: 0x67,
  NumpadComma: 0x85, IntlRo: 0x87,
  ControlLeft: 0xe0, ShiftLeft: 0xe1, AltLeft: 0xe2, MetaLeft: 0xe3,
  ControlRight: 0xe4, ShiftRight: 0xe5, AltRight: 0xe6, MetaRight: 0xe7,
  // Nomes antigos usados por alguns WebViews
  OSLeft: 0xe3, OSRight: 0xe7,
};

for (let i = 0; i < 26; i++) {
  MAP["Key" + String.fromCharCode(65 + i)] = 0x04 + i;
}
for (let i = 1; i <= 9; i++) {
  MAP["Digit" + i] = 0x1e + (i - 1);
}
MAP["Digit0"] = 0x27;

const SWAP: Record<number, number> = {
  0xe0: 0xe3, // Ctrl esq. → Cmd esq.
  0xe3: 0xe0,
  0xe4: 0xe7,
  0xe7: 0xe4,
};

/** Converte `KeyboardEvent.code` em HID. Retorna `null` para teclas desconhecidas. */
export function codeToHid(code: string, swapCtrlMeta: boolean): number | null {
  const hid = MAP[code];
  if (hid === undefined) return null;
  return swapCtrlMeta && SWAP[hid] !== undefined ? SWAP[hid] : hid;
}
