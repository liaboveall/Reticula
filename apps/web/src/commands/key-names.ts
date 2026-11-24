import { IS_MAC, KEYS } from "../keymap.ts";

const MAC_SIGNS: Readonly<Record<string, string>> = {
  Mod: "⌘",
  Ctrl: "⌃",
  Alt: "⌥",
  Shift: "⇧",
};

const KEY_SIGNS: Readonly<Record<string, string>> = { ArrowDown: "↓", Delete: "Del" };

/** The keys of a command the way the table has them, or an empty text. */
export function writtenKeysOf(command: string, isMac: boolean = IS_MAC): string {
  const [, , keys = "", mac = keys] = KEYS.find(([one]) => one === command) ?? [];
  return isMac ? mac : keys;
}

/** The keys of a command the way they are shown, or an empty text. */
export function keysOf(command: string, isMac: boolean = IS_MAC): string {
  const written = writtenKeysOf(command, isMac);
  const parts = written.split("+").map((part) => KEY_SIGNS[part] ?? part);
  return isMac
    ? parts.map((part) => MAC_SIGNS[part] ?? part).join("")
    : parts.map((part) => (part === "Mod" ? "Ctrl" : part)).join("+");
}

/** A name with the keys it goes by, for a tooltip. */
export function withKeys(text: string, command: string): string {
  const keys = keysOf(command);
  return keys ? `${text} (${keys})` : text;
}
