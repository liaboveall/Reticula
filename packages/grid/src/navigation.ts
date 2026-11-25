import type { SheetSize } from "./layout.ts";

// The keys that move the selection, the same with and without an editor.

interface Cell {
  readonly row: number;
  readonly col: number;
}

export type JumpDirection = "up" | "down" | "left" | "right";

/** A grid, as far as keys move its selection. */
export interface NavigationTarget {
  /** The active cell. */
  readonly selection: Cell | null;
  /** The corner of the selection that is across from the active cell. */
  readonly selectionFocus: Cell | null;
  readonly sheetSize: SheetSize;
  selectAll(): void;
  moveSelection(rows: number, cols: number): void;
  extendSelection(rows: number, cols: number): void;
  pageVertical(direction: 1 | -1, extend: boolean): void;
  jumpTo(row: number, col: number, extend: boolean): void;
}

/** Who knows where the data are. Without, jumps go as far as the sheet. */
export interface NavigationJumps {
  readonly toEdge?: ((direction: JumpDirection, extend: boolean) => void) | undefined;
  readonly toUsedEnd?: ((extend: boolean) => void) | undefined;
}

type Modifiers = Pick<KeyboardEvent, "ctrlKey" | "metaKey">;

type NavigationKey = Pick<KeyboardEvent, "key" | "shiftKey" | "altKey"> & Modifiers;

/** Whether Cmd is for what Ctrl is for elsewhere: on an iPad with a keyboard, too. */
export const IS_MAC = typeof navigator !== "undefined" && /^Mac|^iP/.test(navigator.platform);

/** Whether the modifier of commands is held, and not the other one: Ctrl has uses of its own on macOS. */
export function isCommand({ ctrlKey, metaKey }: Modifiers, isMac = IS_MAC): boolean {
  return ctrlKey !== metaKey && metaKey === isMac;
}

/** Rows and columns that a key moves the selection by. */
export const STEPS: Readonly<Record<string, readonly [number, number]>> = {
  ArrowUp: [-1, 0],
  ArrowDown: [1, 0],
  ArrowLeft: [0, -1],
  ArrowRight: [0, 1],
};

const DIRECTIONS: Readonly<Record<string, JumpDirection>> = {
  ArrowUp: "up",
  ArrowDown: "down",
  ArrowLeft: "left",
  ArrowRight: "right",
};

/** Moves the selection as a key says. Returns whether it is one of these keys. */
export function navigate(
  grid: NavigationTarget,
  event: NavigationKey,
  jumps: NavigationJumps = {},
  isMac = IS_MAC,
): boolean {
  const { key, shiftKey: extend } = event;
  const cell = extend ? (grid.selectionFocus ?? grid.selection) : grid.selection;
  const command = isCommand(event, isMac);
  if (event.altKey || (!command && (event.ctrlKey || event.metaKey))) {
    return false;
  }
  const { rows, cols } = grid.sheetSize;
  if (command) {
    const direction = DIRECTIONS[key];
    if (direction) {
      if (jumps.toEdge) {
        jumps.toEdge(direction, extend);
      } else if (cell) {
        grid.jumpTo(
          direction === "up" ? 0 : direction === "down" ? rows - 1 : cell.row,
          direction === "left" ? 0 : direction === "right" ? cols - 1 : cell.col,
          extend,
        );
      }
      return true;
    }
    if (key === "Home") {
      grid.jumpTo(0, 0, extend);
      return true;
    }
    if (key === "End") {
      if (jumps.toUsedEnd) {
        jumps.toUsedEnd(extend);
      } else {
        grid.jumpTo(rows - 1, cols - 1, extend);
      }
      return true;
    }
    if (key.toLowerCase() === "a" && !extend) {
      grid.selectAll();
      return true;
    }
    return false;
  }
  const step = STEPS[key];
  if (step) {
    if (extend) {
      grid.extendSelection(...step);
    } else {
      grid.moveSelection(...step);
    }
    return true;
  }
  switch (key) {
    case "PageDown":
      grid.pageVertical(1, extend);
      return true;
    case "PageUp":
      grid.pageVertical(-1, extend);
      return true;
    case "Home":
    case "End":
      if (cell && cols > 0) {
        grid.jumpTo(cell.row, key === "Home" ? 0 : cols - 1, extend);
      }
      return cell !== null;
    default:
      return false;
  }
}
