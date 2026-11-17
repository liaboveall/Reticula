import type { GridStats } from "@reticula/grid";
import type { CellPosition, EditorEvent } from "@reticula/grid/editor";

/** A window onto the input method page for automated checks of the editor. */
export interface ImeHooks {
  /** Everything the editor has reported so far. */
  readonly events: readonly EditorEvent[];
  state(): {
    /** The cell being edited, if one is. */
    readonly editing: CellPosition | null;
    readonly composing: boolean;
    /** The text in the editor. */
    readonly text: string;
    readonly selection: CellPosition | null;
    /** What was typed into each cell, by the cell's name. */
    readonly committed: Readonly<Record<string, string>>;
    /** Whether the editor can be seen. */
    readonly shown: boolean;
    /** Where the editor is, in CSS pixels from the top left corner of the window. */
    readonly box: { readonly left: number; readonly top: number };
  };
  grid(): GridStats;
  /** The text the grid shows in a cell, or `null` while it does not hold the cell. */
  textAt(row: number, col: number): string | null;
}

declare global {
  interface Window {
    __ime?: ImeHooks;
  }
}
