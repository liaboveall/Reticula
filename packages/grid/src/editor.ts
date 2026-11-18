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
  | {
      readonly kind: "started";
      readonly cause: "typing" | "composition" | "F2" | "doubleClick" | "formulaBar";
    }
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
  /** The text to start from when a cell is opened for editing (`F2` or double-click). */
  readonly textOf: (cell: CellPosition) => string;
  /** Called with what was typed when an edit is accepted. */
  readonly onCommit: (cell: CellPosition, text: string) => void;
  /** Called when the user presses `Delete` or `Backspace` while navigating. */
  readonly onClear?: () => void;
  /** Called whenever the live editor text or active editing cell changes (for formula bar sync). */
  readonly onTextChange?: (text: string, editing: CellPosition | null) => void;
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
 * The editor of the selected cell (`13-m1a.md` 6.2).
 *
 * Always present and focused on the active cell, invisible until an edit
 * starts. Distinguishes two entry modes matching Excel:
 * - `"input"` mode (direct typing or IME composition): pressing an arrow key
 *   outside IME composition commits the cell and moves the grid selection;
 * - `"edit"` mode (`F2`, double-click, or formula bar): arrow keys move the
 *   text caret inside the editor without leaving the cell.
 */
export class CellEditor {
  #grid: Grid;
  #host: HTMLElement;
  #options: CellEditorOptions;
  #box: HTMLDivElement;
  #view: EditorView;
  /** The cell being edited, while one is. */
  #cell: CellPosition | null = null;
  #mode: "input" | "edit" = "input";
  #suppressAutoStart = false;
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
            if (update.docChanged) {
              if (!this.#suppressAutoStart && !this.#cell && update.state.doc.length > 0) {
                this.#start("typing", "input");
              }
              this.#options.onTextChange?.(update.state.doc.toString(), this.#cell);
            }
          }),
          EditorView.domEventHandlers({
            keydown: (event, view) => this.#onKeyDown(event, view),
            compositionstart: (event) => {
              this.#note({ kind: "composition", phase: "start", data: event.data });
              if (!this.#cell) {
                this.#start("composition", "input");
              }
            },
            compositionupdate: (event) => {
              this.#note({ kind: "composition", phase: "update", data: event.data });
            },
            compositionend: (event) => {
              this.#note({ kind: "composition", phase: "end", data: event.data });
              this.#options.onTextChange?.(this.text, this.#cell);
            },
            copy: (event) => this.#onClipboard(event, "copy"),
            cut: (event) => this.#onClipboard(event, "cut"),
            paste: (event) => this.#onClipboard(event, "paste"),
          }),
        ],
      }),
    });

    host.addEventListener("focus", this.#takeFocus);
    host.addEventListener("pointerdown", this.#onPointerDown, { capture: true });
    this.#stopWatching = grid.watchFrames(this.#place);
    this.#place();
  }

  /** The cell being edited, or `null` while navigating. */
  get editing(): CellPosition | null {
    return this.#cell;
  }

  /** Current entry mode while editing (`"input"` or `"edit"`). */
  get editMode(): "input" | "edit" {
    return this.#mode;
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

  /** Opens the currently selected cell in `"edit"` mode (double-click or `F2`). */
  startEdit(
    cause: "F2" | "doubleClick" | "formulaBar" = "doubleClick",
    initialText?: string,
    focusEditor = true,
  ): void {
    const cell = this.#grid.selection;
    if (!cell) {
      return;
    }
    const text = initialText ?? this.#options.textOf(cell);
    this.#suppressAutoStart = true;
    this.#view.dispatch({
      changes: { from: 0, to: this.#view.state.doc.length, insert: text },
      selection: { anchor: text.length },
    });
    this.#suppressAutoStart = false;
    if (!this.#cell) {
      this.#start(cause, "edit");
    } else {
      this.#mode = "edit";
    }
    this.#options.onTextChange?.(this.text, this.#cell);
    if (focusEditor) {
      this.#view.focus();
    }
  }

  /**
   * Synchronizes live text typed in the external formula bar without stealing
   * keyboard focus away from the formula bar `<input>`.
   */
  setTextExternal(text: string): void {
    if (!this.#cell) {
      this.startEdit("formulaBar", text, false);
      return;
    }
    this.#mode = "edit";
    if (this.text !== text) {
      this.#suppressAutoStart = true;
      this.#view.dispatch({
        changes: { from: 0, to: this.#view.state.doc.length, insert: text },
        selection: { anchor: text.length },
      });
      this.#suppressAutoStart = false;
    }
  }

  /** Commits an active edit initiated or modified from the formula bar and moves down. */
  commitExternal(moveDown = true): void {
    const cell = this.#commit();
    if (cell && moveDown) {
      this.#grid.select(cell);
      this.#grid.moveSelection(1, 0);
    }
    this.#view.focus();
  }

  /** Cancels an active edit from the formula bar and returns focus to the grid. */
  cancelExternal(): void {
    const cell = this.#cell;
    if (cell) {
      this.#finish();
      this.#note({ kind: "cancelled", cell });
    }
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
    const box = cell
      ? this.#grid.boxOf(cell.row, cell.col)
      : { left: 0, top: 0, width: 0, height: 0 };
    let { left, top } = box;
    if (!this.#cell) {
      left = Math.min(Math.max(0, left), Math.max(0, this.#host.clientWidth - box.width));
      top = Math.min(Math.max(0, top), Math.max(0, this.#host.clientHeight - box.height));
    }
    const style = this.#box.style;
    style.transform = `translate(${left}px, ${top}px)`;
    style.minWidth = `${box.width}px`;
    style.height = `${box.height}px`;
    style.lineHeight = `${box.height}px`;
  };

  #start(
    cause: "typing" | "composition" | "F2" | "doubleClick" | "formulaBar",
    mode: "input" | "edit",
  ): void {
    const cell = this.#grid.selection;
    if (!cell) {
      if (!this.composing) {
        this.#clear();
      }
      return;
    }
    this.#cell = { row: cell.row, col: cell.col };
    this.#mode = mode;
    this.#grid.reveal(cell.row, cell.col);
    this.#box.dataset.editing = "";
    this.#place();
    this.#options.onTextChange?.(this.text, this.#cell);
    this.#note({ kind: "started", cause });
  }

  /** Empties the editor and hides it again. */
  #finish(): void {
    this.#cell = null;
    this.#mode = "input";
    delete this.#box.dataset.editing;
    this.#suppressAutoStart = true;
    this.#clear();
    this.#suppressAutoStart = false;
    this.#place();
    this.#options.onTextChange?.("", null);
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
      // In `"input"` mode (direct typing without F2), plain arrow keys commit
      // the cell and step the grid selection, matching Excel.
      if (this.#mode === "input" && plain && !event.shiftKey && STEPS[event.key]) {
        const cell = this.#commit();
        if (cell) {
          this.#grid.select(cell);
          this.#move(event);
        }
        return "committed";
      }
      return "passed";
    }

    if (plain && STEPS[event.key]) {
      const [rows, cols] = STEPS[event.key] ?? [0, 0];
      if (event.shiftKey) {
        this.#grid.extendSelection(rows, cols);
      } else {
        this.#grid.moveSelection(rows, cols);
      }
      return "moved";
    }
    if (plain && (event.key === "Enter" || event.key === "Tab")) {
      this.#move(event);
      return "moved";
    }
    if (event.key === "F2") {
      this.startEdit("F2");
      return "swallowed";
    }
    if (event.key === "Backspace" || event.key === "Delete") {
      this.#options.onClear?.();
      return "swallowed";
    }
    if (event.key === "Escape") {
      return "swallowed";
    }
    return "passed";
  }

  #move(event: KeyboardEvent): void {
    const back = event.shiftKey ? -1 : 1;
    const [rows, cols] =
      STEPS[event.key] ??
      (event.key === "Enter" ? [back, 0] : event.key === "Tab" ? [0, back] : [0, 0]);
    this.#grid.moveSelection(rows, cols);
  }

  #onClipboard(event: ClipboardEvent, type: "copy" | "cut" | "paste"): boolean {
    this.#note({ kind: "clipboard", type });
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
    delete this.#box.dataset.editing;
    setTimeout(() => {
      this.#commit();
      this.#view.focus();
    }, 0);
  };
}
