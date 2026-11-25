import { IS_MAC } from "@reticula/grid";

/**
 * The keys of the editor's commands, and where each of them applies. One
 * listener looks them up, whatever has the focus: what has a use of its own for
 * a key has prevented its default by then.
 */

/**
 * - `anywhere`: everywhere but in a dialog;
 * - `page`: not where text is typed either;
 * - `grid`: only in the grid, while no cell is being edited;
 * - `shown`: handled by the grid itself, and listed here to be shown.
 */
type Reach = "anywhere" | "page" | "grid" | "shown";

/** What has the focus, as far as the keys are concerned. */
export type FocusScope = "modal" | "input" | "editing" | "grid" | "page";

/**
 * Ctrl is Mod on Windows and Linux, and Cmd is on macOS. Other keys there are
 * in the fourth place, for those that the system or the browser has taken.
 */
export const KEYS: readonly (readonly [
  command: string,
  reach: Reach,
  keys: string,
  mac?: string,
])[] = [
  ["nextRegion", "anywhere", "F6"],
  ["previousRegion", "anywhere", "Shift+F6"],
  ["palette", "page", "Mod+/"],
  ["goTo", "page", "Mod+G"],
  ["find", "page", "Mod+F"],
  ["replace", "page", "Mod+H", "Mod+Shift+H"],
  ["undo", "page", "Mod+Z"],
  ["redo", "page", "Mod+Y", "Mod+Shift+Z"],
  ["redo", "page", "Mod+Shift+Z"],
  ["bold", "page", "Mod+B"],
  ["italic", "page", "Mod+I"],
  ["strikethrough", "page", "Mod+5", "Mod+Shift+X"],
  ["nextSheet", "page", "Alt+PageDown"],
  ["previousSheet", "page", "Alt+PageUp"],
  ["contextMenu", "grid", "Shift+F10"],
  ["contextMenu", "grid", "ContextMenu"],
  ["selectRows", "grid", "Shift+Space"],
  ["selectCols", "grid", "Mod+Space", "Ctrl+Space"],
  ["fillDown", "grid", "Mod+D"],
  ["fillRight", "grid", "Mod+R"],
  ["filterColumn", "grid", "Alt+ArrowDown"],
  ["selectAll", "shown", "Mod+A"],
  ["cut", "shown", "Mod+X"],
  ["copy", "shown", "Mod+C"],
  ["paste", "shown", "Mod+V"],
  ["clearContents", "shown", "Delete"],
];

export interface KeyPress {
  readonly key: string;
  readonly code: string;
  readonly keyCode: number;
  readonly ctrlKey: boolean;
  readonly metaKey: boolean;
  readonly altKey: boolean;
  readonly shiftKey: boolean;
  readonly isComposing: boolean;
  readonly defaultPrevented: boolean;
  getModifierState(key: string): boolean;
}

/** What is pressed, written the way the table does, or `null` if it is typing. */
function pressed(event: KeyPress, isMac: boolean): string | null {
  if (
    event.defaultPrevented ||
    event.isComposing ||
    event.keyCode === 229 ||
    event.getModifierState("AltGraph")
  ) {
    return null;
  }
  // Other scripts have their letters on the keys of the Latin ones.
  const latin = /^[^ -~]$/.test(event.key) ? /^Key(.)$/.exec(event.code)?.[1] : undefined;
  const key = event.key === " " ? "Space" : (latin ?? event.key);
  const letter = key.length === 1 ? key.toUpperCase() : key;
  // With which of the digits and signs Shift is held depends on the keyboard.
  const shift = event.shiftKey && !/^[^A-Z]$/.test(letter);
  const [mod, other] = isMac ? [event.metaKey, event.ctrlKey] : [event.ctrlKey, event.metaKey];
  return (
    (mod ? "Mod+" : "") +
    (other ? (isMac ? "Ctrl+" : "Meta+") : "") +
    (event.altKey ? "Alt+" : "") +
    (shift ? "Shift+" : "") +
    letter
  );
}

/** The command a key means where the focus is, if any. */
export function commandFor(
  event: KeyPress,
  scope: FocusScope,
  isMac: boolean = IS_MAC,
): string | null {
  const combination = scope === "modal" ? null : pressed(event, isMac);
  if (combination === null) {
    return null;
  }
  for (const [command, reach, keys, mac = keys] of KEYS) {
    if (combination === (isMac ? mac : keys)) {
      const applies =
        reach === "anywhere" || scope === "grid"
          ? reach !== "shown"
          : reach === "page" && scope === "page";
      if (applies) {
        return command;
      }
    }
  }
  return null;
}

/** Where the focus is. A dialog has the keys for itself, whatever the event's target is. */
export function focusScope(
  target: EventTarget | null,
  grid: Pick<Node, "contains"> | null,
  isEditing: boolean,
): FocusScope {
  if (document.querySelector("dialog:modal")) {
    return "modal";
  }
  if (!(target instanceof Element)) {
    return "page";
  }
  if (grid?.contains(target)) {
    return isEditing ? "editing" : "grid";
  }
  return target.closest('input, textarea, select, [contenteditable="true"]') ? "input" : "page";
}

/**
 * Keeps the browser's own menu closed that the keys for ours would open: for
 * one of them it comes when the key is released.
 */
export function swallowContextMenu(): void {
  const swallow = (event: Event) => {
    event.preventDefault();
    event.stopPropagation();
  };
  window.addEventListener("contextmenu", swallow, { capture: true });
  window.addEventListener(
    "keyup",
    () =>
      setTimeout(() => window.removeEventListener("contextmenu", swallow, { capture: true }), 50),
    { capture: true, once: true },
  );
}
