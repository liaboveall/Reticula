import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import type { Grid } from "./grid.ts";

/** A cell, by position. */
export interface CellPosition {
  readonly row: number;
  readonly col: number;
}

/** Something the editor did or saw: for a log, or for a test to wait on. */
export type EditorEvent =
  | { readonly kind: "started"; readonly cause: "typing" | "composition" | "F2" }
  | { readonly kind: "committed"; readonly cell: CellPosition; readonly text: string }
  | { readonly kind: "cancelled"; readonly cell: CellPosition }
  | {
      readonly kind: "composition";
      readonly phase: "start" | "update" | "end";
      readonly data: string;
    }
  | {
      readonly kind: "key";
      readonly key: string;
      /** What became of the key press. */
      readonly outcome:
        | "input method"
        | "moved"
        | "committed"
        | "cancelled"
        | "swallowed"
        | "passed";
    }
  | { readonly kind: "clipboard"; readonly type: "copy" | "cut" | "paste" };

export interface CellEditorOptions {
  /** The text to start from when a cell is opened for editing with F2. */
  readonly textOf: (cell: CellPosition) => string;
  /** Called with what was typed when an edit is accepted. */
  readonly onCommit: (cell: CellPosition, text: string) => void;
  readonly onEvent?: (event: EditorEvent) => void;
}

/** How far each key moves the selection: rows, then columns. */
const STEPS: Readonly<Record<string, readonly [number, number]>> = {
  ArrowUp: [-1, 0],
  ArrowDown: [1, 0],
  ArrowLeft: [0, -1],
  ArrowRight: [0, 1],
};

/**
 * The editor of the selected cell.
 *
 * A canvas cannot take text input, least of all from an input method, whose
 * window of candidates has to open next to the text being written. So there
 * is a real text editor, and it is always there: it holds the keyboard focus
 * and sits on the selected cell, invisible. Moving around the grid is its key
 * presses, passed on. The moment a character or a composition arrives, it
 * arrives in the right place, and all that is left to do is to show the
 * editor.
 */
export class CellEditor {
  #grid: Grid;
  #host: HTMLElement;
  #options: CellEditorOptions;
  #box: HTMLDivElement;
  #view: EditorView;
  /** The cell being edited, while one is. */
  #cell: CellPosition | null = null;
  #stopWatching: () => void;

  constructor(grid: Grid, host: HTMLElement, options: CellEditorOptions) {
    this.#grid = grid;
    this.#host = host;
    this.#options = options;

    this.#box = document.createElement("div");
    this.#box.className = "reticula-editor";
    host.append(this.#box);
    this.#view = new EditorView({
      parent: this.#box,
      state: EditorState.create({
        extensions: [
          EditorView.contentAttributes.of({ "aria-label": "Cell editor" }),
          // A cell's text is one line; whatever would add another is refused.
          EditorState.transactionFilter.of((transaction) =>
            transaction.newDoc.lines > 1 ? [] : transaction,
          ),
          EditorView.updateListener.of((update) => {
            // Text has arrived in the hidden editor: that is how an edit starts.
            if (update.docChanged && !this.#cell && update.state.doc.length > 0) {
              this.#start("typing");
            }
          }),
          EditorView.domEventHandlers({
            keydown: (event, view) => this.#onKeyDown(event, view),
            compositionstart: (event) => {
              this.#note({ kind: "composition", phase: "start", data: event.data });
              if (!this.#cell) {
                // Shown before the first character is settled, so that the
                // text being composed is seen where it will go.
                this.#start("composition");
              }
            },
            compositionupdate: (event) => {
              this.#note({ kind: "composition", phase: "update", data: event.data });
            },
            compositionend: (event) => {
              this.#note({ kind: "composition", phase: "end", data: event.data });
            },
            copy: (event) => this.#onClipboard(event, "copy"),
            cut: (event) => this.#onClipboard(event, "cut"),
            paste: (event) => this.#onClipboard(event, "paste"),
          }),
        ],
      }),
    });

    // Focus that lands on the grid belongs to the editor.
    host.addEventListener("focus", this.#takeFocus);
    // Before the grid moves the selection to a cell that was clicked.
    host.addEventListener("pointerdown", this.#onPointerDown, { capture: true });
    this.#stopWatching = grid.watchFrames(this.#place);
    this.#place();
  }

  /** The cell being edited, or `null` while the user is moving around. */
  get editing(): CellPosition | null {
    return this.#cell;
  }

  /** The text typed so far. */
  get text(): string {
    return this.#view.state.doc.toString();
  }

  /** Whether an input method is composing text in the editor. */
  get composing(): boolean {
    return this.#view.composing || this.#view.compositionStarted;
  }

  /** Gives the editor the keyboard. */
  focus(): void {
    this.#view.focus();
  }

  destroy(): void {
    this.#stopWatching();
    this.#host.removeEventListener("focus", this.#takeFocus);
    this.#host.removeEventListener("pointerdown", this.#onPointerDown, { capture: true });
    this.#view.destroy();
    this.#box.remove();
  }

  #note(event: EditorEvent): void {
    this.#options.onEvent?.(event);
  }

  #takeFocus = (): void => {
    this.#view.focus();
  };

  /** Keeps the editor on its cell. Runs at the end of every frame the grid draws. */
  #place = (): void => {
    const cell = this.#cell ?? this.#grid.selection;
    // With nothing selected it waits in the corner. It is never taken out of
    // rendering: an element that is not rendered cannot hold the focus.
    const box = cell
      ? this.#grid.boxOf(cell.row, cell.col)
      : { left: 0, top: 0, width: 0, height: 0 };
    let { left, top } = box;
    if (!this.#cell) {
      // While hidden it stays inside the grid even if its cell is scrolled
      // away: an input method opens its window wherever the editor is.
      left = Math.min(Math.max(0, left), Math.max(0, this.#host.clientWidth - box.width));
      top = Math.min(Math.max(0, top), Math.max(0, this.#host.clientHeight - box.height));
    }
    const style = this.#box.style;
    style.transform = `translate(${left}px, ${top}px)`;
    style.minWidth = `${box.width}px`;
    style.height = `${box.height}px`;
    // One line, as tall as the row: the text sits where the cell's text is drawn.
    style.lineHeight = `${box.height}px`;
  };

  #start(cause: "typing" | "composition" | "F2"): void {
    const cell = this.#grid.selection;
    if (!cell) {
      // Nothing is selected, so there is nowhere to type into.
      if (!this.composing) {
        this.#clear();
      }
      return;
    }
    this.#cell = { row: cell.row, col: cell.col };
    this.#grid.reveal(cell.row, cell.col);
    this.#box.dataset.editing = "";
    this.#place();
    this.#note({ kind: "started", cause });
  }

  /** Empties the editor and hides it again. */
  #finish(): void {
    this.#cell = null;
    delete this.#box.dataset.editing;
    this.#clear();
    this.#place();
  }

  #clear(): void {
    const length = this.#view.state.doc.length;
    if (length > 0) {
      this.#view.dispatch({ changes: { from: 0, to: length, insert: "" } });
    }
  }

  #commit(): CellPosition | null {
    const cell = this.#cell;
    if (!cell) {
      return null;
    }
    const text = this.text;
    this.#finish();
    this.#options.onCommit(cell, text);
    this.#note({ kind: "committed", cell, text });
    return cell;
  }

  #onKeyDown(event: KeyboardEvent, view: EditorView): boolean {
    const outcome = this.#handleKey(event, view);
    this.#note({ kind: "key", key: event.key, outcome });
    return outcome !== "passed" && outcome !== "input method";
  }

  #handleKey(
    event: KeyboardEvent,
    view: EditorView,
  ): "input method" | "moved" | "committed" | "cancelled" | "swallowed" | "passed" {
    // A key the input method is using is not a key press of ours: Enter
    // accepts a candidate, Escape discards the composition, the arrows walk
    // the candidates. Browsers mark such events in one of these ways. The key
    // code is what marks the key that starts a composition, and the Enter
    // that Safari reports only after the composition it accepted has ended.
    if (event.isComposing || event.keyCode === 229 || view.composing || view.compositionStarted) {
      return "input method";
    }
    const plain = !event.ctrlKey && !event.metaKey && !event.altKey;

    if (this.#cell) {
      if (event.key === "Enter" || event.key === "Tab") {
        const cell = this.#commit();
        if (cell) {
          this.#grid.select(cell);
          this.#move(event);
        }
        return "committed";
      }
      if (event.key === "Escape") {
        const cell = this.#cell;
        this.#finish();
        this.#note({ kind: "cancelled", cell });
        return "cancelled";
      }
      // Everything else is editing: typing, moving the caret, deleting.
      return "passed";
    }

    if (plain && (STEPS[event.key] || event.key === "Enter" || event.key === "Tab")) {
      this.#move(event);
      return "moved";
    }
    if (event.key === "F2") {
      const cell = this.#grid.selection;
      if (cell) {
        const text = this.#options.textOf(cell);
        this.#view.dispatch({
          changes: { from: 0, to: this.#view.state.doc.length, insert: text },
          selection: { anchor: text.length },
        });
        if (!this.#cell) {
          // The cell was empty, so no text arrived to start the edit.
          this.#start("F2");
        }
      }
      return "swallowed";
    }
    // Keys that would act on the hidden, empty editor and mean nothing yet.
    if (event.key === "Backspace" || event.key === "Delete" || event.key === "Escape") {
      return "swallowed";
    }
    // A character starts an edit by arriving in the editor; a shortcut is
    // the browser's, or raises a clipboard event handled below.
    return "passed";
  }

  /** Moves the selection as a key asks: an arrow, Enter (down) or Tab (right), Shift reversing the last two. */
  #move(event: KeyboardEvent): void {
    const back = event.shiftKey ? -1 : 1;
    const [rows, cols] =
      STEPS[event.key] ??
      (event.key === "Enter" ? [back, 0] : event.key === "Tab" ? [0, back] : [0, 0]);
    this.#grid.moveSelection(rows, cols);
  }

  #onClipboard(event: ClipboardEvent, type: "copy" | "cut" | "paste"): boolean {
    this.#note({ kind: "clipboard", type });
    // While a cell is being edited the clipboard works on its text, as in any
    // editor. Otherwise it is about the selected cells, which is the grid's
    // business once it can copy and paste; the hidden editor must not change.
    if (this.#cell) {
      return false;
    }
    event.preventDefault();
    return true;
  }

  /** A click elsewhere in the grid accepts the edit before the selection moves there. */
  #onPointerDown = (event: PointerEvent): void => {
    if (!this.#cell || (event.target instanceof Node && this.#box.contains(event.target))) {
      return;
    }
    // Hidden at once, so that it is not seen jumping to the clicked cell.
    delete this.#box.dataset.editing;
    // The click takes the focus away, which makes an input method finish or
    // drop what it was composing. The text is read after that has happened.
    setTimeout(() => {
      this.#commit();
      this.#view.focus();
    }, 0);
  };
}
