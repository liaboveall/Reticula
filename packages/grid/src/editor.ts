import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import type { WireFormulaAnalysis } from "@reticula/protocol";
import { formulaDecorationExtension, setFormulaAnalysisEffect } from "./formula-input.ts";
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
  readonly onTextChange?: (text: string, editing: CellPosition | null, cursorUtf16: number) => void;
  /** Called when `F4` is pressed inside an active formula edit. */
  readonly onToggleDollar?: (text: string, cursorUtf16: number) => void;
  /** Optional hook for accepting formula completions (`Tab` / navigated `Enter`) during in-cell editing. */
  readonly onFormulaKeyDown?: (event: KeyboardEvent, text: string, cursorUtf16: number) => boolean;
  /** Returns `true` when clicking the grid should insert a formula reference instead of committing. */
  readonly isPointAndClickActive?: () => boolean;
  /** `Ctrl+Arrow` / `Ctrl+Shift+Arrow` data-edge jump request. */
  readonly onJumpEdge?: (dir: "up" | "down" | "left" | "right", select: boolean) => void;
  /** `Ctrl+End` / `Ctrl+Shift+End` jump to last used cell. */
  readonly onJumpUsedEnd?: (select: boolean) => void;
  /** `Ctrl+Z` undo shortcut. */
  readonly onUndo?: () => void;
  /** `Ctrl+Y` / `Ctrl+Shift+Z` redo shortcut. */
  readonly onRedo?: () => void;
  /** `Ctrl+C` / `Ctrl+X` copy or cut gesture while navigating. */
  readonly onCopy?: (cut: boolean, clipboardData: DataTransfer | null) => void;
  /** `Ctrl+V` paste gesture while navigating. */
  readonly onPaste?: (tsv: string, html: string | null) => void;
  readonly onEvent?: (event: EditorEvent) => void;
}

const STEPS: Readonly<Record<string, readonly [number, number]>> = {
  ArrowUp: [-1, 0],
  ArrowDown: [1, 0],
  ArrowLeft: [0, -1],
  ArrowRight: [0, 1],
};

const JUMP_DIRS: Readonly<Record<string, "up" | "down" | "left" | "right">> = {
  ArrowUp: "up",
  ArrowDown: "down",
  ArrowLeft: "left",
  ArrowRight: "right",
};

/**
 * The editor of the selected cell (`13-m1a.md` 6.2 & `14-m1b.md` 6.1–6.3).
 */
export class CellEditor {
  #grid: Grid;
  #host: HTMLElement;
  #options: CellEditorOptions;
  #box: HTMLDivElement;
  #view: EditorView;
  #cell: CellPosition | null = null;
  #mode: "input" | "edit" = "input";
  #suppressAutoStart = false;
  #suppressNotify = false;
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
          EditorState.transactionFilter.of((transaction) =>
            transaction.newDoc.lines > 1 ? [] : transaction,
          ),
          formulaDecorationExtension(),
          EditorView.updateListener.of((update) => {
            if (update.docChanged || (this.#cell && update.selectionSet)) {
              if (!this.#suppressAutoStart && !this.#cell && update.state.doc.length > 0) {
                this.#start("typing", "input");
              }
              if (!this.#suppressNotify) {
                this.#options.onTextChange?.(
                  update.state.doc.toString(),
                  this.#cell,
                  update.state.selection.main.head,
                );
              }
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
              this.#options.onTextChange?.(
                this.text,
                this.#cell,
                this.#view.state.selection.main.head,
              );
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

  get editing(): CellPosition | null {
    return this.#cell;
  }

  get editMode(): "input" | "edit" {
    return this.#mode;
  }

  get text(): string {
    return this.#view.state.doc.toString();
  }

  get cursorUtf16(): number {
    return this.#view.state.selection.main.head;
  }

  get composing(): boolean {
    return this.#view.composing || this.#view.compositionStarted;
  }

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
    this.#options.onTextChange?.(this.text, this.#cell, this.#view.state.selection.main.head);
    if (focusEditor) {
      this.#view.focus();
    }
  }

  /**
   * Synchronizes live text typed in the external CodeMirror formula bar without
   * stealing keyboard focus away from the formula bar.
   */
  setTextExternal(text: string, cursorUtf16?: number): void {
    if (!this.#cell) {
      this.startEdit("formulaBar", text, false);
      return;
    }
    this.#mode = "edit";
    const head = Math.min(text.length, Math.max(0, cursorUtf16 ?? text.length));
    if (this.text !== text || this.#view.state.selection.main.head !== head) {
      this.#suppressAutoStart = true;
      this.#view.dispatch({
        changes: { from: 0, to: this.#view.state.doc.length, insert: text },
        selection: { anchor: head },
      });
      this.#suppressAutoStart = false;
    }
  }

  /** Applies an `F4` toggle or point-and-click reference edit to the active editor. */
  applyTextAndSelection(text: string, anchorUtf16: number, headUtf16 = anchorUtf16): void {
    if (!this.#cell) {
      this.startEdit("formulaBar", text, false);
    }
    const clampedAnchor = Math.min(text.length, Math.max(0, anchorUtf16));
    const clampedHead = Math.min(text.length, Math.max(0, headUtf16));
    this.#suppressAutoStart = true;
    this.#view.dispatch({
      changes: { from: 0, to: this.#view.state.doc.length, insert: text },
      selection: { anchor: clampedAnchor, head: clampedHead },
    });
    this.#suppressAutoStart = false;
    this.#options.onTextChange?.(text, this.#cell, clampedHead);
  }

  /** Updates live formula syntax/reference/diagnostic decorations on the cell editor. */
  setFormulaAnalysis(analysis: WireFormulaAnalysis | null): void {
    this.#view.dispatch({
      effects: setFormulaAnalysisEffect.of(analysis),
    });
  }

  commitExternal(moveDown = true): void {
    const cell = this.#commit();
    if (cell && moveDown) {
      this.#grid.select(cell);
      this.#grid.moveSelection(1, 0);
    }
    this.#view.focus();
  }

  cancelExternal(): void {
    const cell = this.#cell;
    if (cell) {
      this.#finish();
      this.#note({ kind: "cancelled", cell });
    }
    this.#view.focus();
  }

  /** Closes the in-grid overlay without emitting `cancelled` or stealing focus (used when switching tabs mid-formula). */
  clearSilent(): void {
    if (this.#cell) {
      this.#finish(false);
    }
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
    this.#options.onTextChange?.(this.text, this.#cell, this.#view.state.selection.main.head);
    this.#note({ kind: "started", cause });
  }

  #finish(notify = true): void {
    this.#cell = null;
    this.#mode = "input";
    delete this.#box.dataset.editing;
    this.#suppressAutoStart = true;
    this.#suppressNotify = !notify;
    this.#clear();
    this.#suppressNotify = false;
    this.#suppressAutoStart = false;
    this.#place();
    if (notify) {
      this.#options.onTextChange?.("", null, 0);
    }
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
    const mod = event.ctrlKey || event.metaKey;
    const plain = !mod && !event.altKey;

    if (this.#cell) {
      if (
        this.text.startsWith("=") &&
        this.#options.onFormulaKeyDown?.(event, this.text, view.state.selection.main.head)
      ) {
        return "swallowed";
      }
      if (event.key === "F4" && this.text.startsWith("=")) {
        event.preventDefault();
        this.#options.onToggleDollar?.(this.text, view.state.selection.main.head);
        return "swallowed";
      }
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

    // Undo / Redo / Select All while navigating (`Ctrl/Cmd + Z/Y/A`).
    if (mod && !event.altKey) {
      const lower = event.key.toLowerCase();
      if (lower === "z") {
        event.preventDefault();
        if (event.shiftKey) {
          this.#options.onRedo?.();
        } else {
          this.#options.onUndo?.();
        }
        return "swallowed";
      }
      if (lower === "y" && !event.shiftKey) {
        event.preventDefault();
        this.#options.onRedo?.();
        return "swallowed";
      }
      if (lower === "a" && !event.shiftKey) {
        event.preventDefault();
        this.#grid.selectAll();
        return "swallowed";
      }
      const jumpDir = JUMP_DIRS[event.key];
      if (jumpDir && this.#options.onJumpEdge) {
        event.preventDefault();
        this.#options.onJumpEdge(jumpDir, event.shiftKey);
        return "moved";
      }
      if (event.key === "Home") {
        event.preventDefault();
        this.#grid.jumpTo(0, 0, event.shiftKey);
        return "moved";
      }
      if (event.key === "End" && this.#options.onJumpUsedEnd) {
        event.preventDefault();
        this.#options.onJumpUsedEnd(event.shiftKey);
        return "moved";
      }
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
    if (plain && event.key === "PageDown") {
      event.preventDefault();
      this.#grid.pageVertical(1, event.shiftKey);
      return "moved";
    }
    if (plain && event.key === "PageUp") {
      event.preventDefault();
      this.#grid.pageVertical(-1, event.shiftKey);
      return "moved";
    }
    if (plain && event.key === "Home") {
      const cur = this.#grid.selection;
      if (cur) {
        event.preventDefault();
        this.#grid.jumpTo(cur.row, 0, event.shiftKey);
        return "moved";
      }
    }
    if (plain && event.key === "End") {
      const cur = this.#grid.selection;
      if (cur && this.#grid.sheetSize.cols > 0) {
        event.preventDefault();
        this.#grid.jumpTo(cur.row, this.#grid.sheetSize.cols - 1, event.shiftKey);
        return "moved";
      }
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
      this.#grid.setClipboardRange(null);
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
    if (type === "paste") {
      const tsv = event.clipboardData?.getData("text/plain") ?? "";
      const html = event.clipboardData?.getData("text/html") || null;
      this.#options.onPaste?.(tsv, html);
    } else {
      this.#options.onCopy?.(type === "cut", event.clipboardData);
    }
    return true;
  }

  #onPointerDown = (event: PointerEvent): void => {
    if (!this.#cell || (event.target instanceof Node && this.#box.contains(event.target))) {
      return;
    }
    if (this.#options.isPointAndClickActive?.()) {
      return;
    }
    delete this.#box.dataset.editing;
    setTimeout(() => {
      this.#commit();
      this.#view.focus();
    }, 0);
  };
}
