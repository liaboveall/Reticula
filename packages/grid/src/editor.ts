import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import type { WireFormulaAnalysis } from "@reticula/protocol";
import { formulaDecorationExtension, setFormulaAnalysisEffect } from "./formula-input.ts";
import type { Grid } from "./grid.ts";
import { isCommand, type JumpDirection, navigate, STEPS } from "./navigation.ts";
import { PADDING } from "./render/frame.ts";
import type { TapEvent } from "./touch-input.ts";

const TAP_EVENT: TapEvent = "reticula-tap";

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
  /**
   * The text to start from when a cell is opened for editing (`F2` or double-click).
   * `null` if that is not known yet: `supplyText` is to be called when it may be.
   */
  readonly textOf: (cell: CellPosition) => string | null;
  /**
   * Called with what was typed when an edit is accepted. It goes behind what is
   * in the cell, if that has not been supplied by then.
   */
  readonly onCommit: (cell: CellPosition, text: string, isRest: boolean) => void;
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
  readonly onJumpEdge?: (dir: JumpDirection, select: boolean) => void;
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
  /** Optional guard preventing in-cell typing, editing, undo/redo, clear, cut, and paste. */
  readonly isReadOnly?: () => boolean;
  /** Whether there is nothing but a touch screen, which would show a keyboard for the editor. */
  readonly isTouchOnly?: () => boolean;
  readonly onEvent?: (event: EditorEvent) => void;
}

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
  /** Whether what is in the cell is yet to come, and to be put before what is typed meanwhile. */
  #isWaiting = false;
  #suppressAutoStart = false;
  #suppressNotify = false;
  /** Whether a finger was the last to do anything here. */
  #isTouched = false;
  #stopWatching: () => void;
  #detach: () => void;

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
          EditorView.contentAttributes.of(() => this.#attributes()),
          EditorState.transactionFilter.of((transaction) => {
            if (transaction.newDoc.lines > 1) {
              return [];
            }
            if (
              !this.#suppressAutoStart &&
              !this.#cell &&
              this.#options.isReadOnly?.() &&
              transaction.docChanged
            ) {
              return [];
            }
            return transaction;
          }),
          formulaDecorationExtension(),
          EditorView.updateListener.of((update) => {
            if (update.docChanged || (this.#cell && update.selectionSet)) {
              if (
                !this.#suppressAutoStart &&
                !this.#cell &&
                !this.#options.isReadOnly?.() &&
                update.state.doc.length > 0
              ) {
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
              if (!this.#cell && !this.#options.isReadOnly?.()) {
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
              // What has been composed is in the text by then.
              setTimeout(() => this.supplyText());
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
    host.addEventListener(TAP_EVENT, this.#onTap);
    this.#stopWatching = grid.watchFrames(this.#place);
    this.#detach = grid.attachEditor(() => this.refreshAttributes());
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

  /** To be called when what `isReadOnly` or `isTouchOnly` says has changed. */
  refreshAttributes(): void {
    // They are asked for with every update. What is being composed is left alone until the next.
    if (!this.composing) {
      this.#view.dispatch({});
    }
  }

  /** What the editor is for assistive technology: one line, which may not be for typing into. */
  #attributes(): Record<string, string> {
    const { cellEditor } = this.#grid.strings;
    return {
      "aria-multiline": "false",
      ...(cellEditor ? { "aria-label": cellEditor } : {}),
      ...(this.#options.isReadOnly?.() ? { "aria-readonly": "true" } : {}),
      // It has the focus of a grid that is tapped, for a keyboard that is one.
      ...(this.#isTouched || this.#options.isTouchOnly?.() ? { inputmode: "none" } : {}),
    };
  }

  #setTouched(isTouched: boolean): void {
    if (isTouched !== this.#isTouched) {
      this.#isTouched = isTouched;
      this.refreshAttributes();
    }
  }

  /** Opens the currently selected cell in `"edit"` mode (double-click or `F2`). */
  startEdit(
    cause: "F2" | "doubleClick" | "formulaBar" = "doubleClick",
    initialText?: string,
    focusEditor = true,
  ): void {
    if (this.#options.isReadOnly?.()) {
      return;
    }
    const cell = this.#grid.selection;
    if (!cell) {
      return;
    }
    const known = initialText ?? this.#options.textOf(cell);
    const text = known ?? "";
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
    this.#isWaiting = known === null;
    this.#options.onTextChange?.(this.text, this.#cell, this.#view.state.selection.main.head);
    if (focusEditor) {
      this.#view.focus();
    }
  }

  /** To be called when `textOf` may know of a cell what it did not when that was opened. */
  supplyText(): void {
    // What is being composed stays as it is until it is done.
    const text =
      this.#isWaiting && this.#cell && !this.composing ? this.#options.textOf(this.#cell) : null;
    if (text !== null) {
      this.#isWaiting = false;
      this.#view.dispatch({
        changes: { from: 0, insert: text },
        selection: { anchor: text.length + this.cursorUtf16 },
      });
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

  cancelExternal(focusEditor = true): void {
    const cell = this.#cell;
    if (cell) {
      this.#finish();
      this.#note({ kind: "cancelled", cell });
    }
    if (focusEditor) {
      this.#view.focus();
    }
  }

  /** Closes the in-grid overlay without emitting `cancelled` or stealing focus (used when switching tabs mid-formula). */
  clearSilent(): void {
    if (this.#cell) {
      this.#finish(false);
    }
  }

  destroy(): void {
    this.#stopWatching();
    this.#detach();
    this.#host.removeEventListener("focus", this.#takeFocus);
    this.#host.removeEventListener("pointerdown", this.#onPointerDown, { capture: true });
    this.#host.removeEventListener(TAP_EVENT, this.#onTap);
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
    // Hidden, it is as large as it is going to be: an input method opens its
    // window where the editor is before there is anything to show in it.
    const box = cell
      ? this.#grid.editorBoxOf(cell.row, cell.col, this.#cell !== null)
      : { left: 0, top: 0, width: 0, height: 0, zoom: 1, maxWidth: 0 };
    const style = this.#box.style;
    style.transform = `translate(${box.left}px, ${box.top}px)`;
    style.minWidth = `${box.width}px`;
    style.maxWidth = `${box.maxWidth}px`;
    style.height = `${box.height}px`;
    style.lineHeight = `${box.height}px`;
    style.fontSize = `${box.zoom}em`;
    style.setProperty("--editor-padding", `${PADDING * box.zoom}px`);
  };

  #start(
    cause: "typing" | "composition" | "F2" | "doubleClick" | "formulaBar",
    mode: "input" | "edit",
  ): void {
    if (this.#options.isReadOnly?.()) {
      return;
    }
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
    this.#isWaiting = false;
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
    const { text } = this;
    const isRest = this.#isWaiting;
    this.#finish();
    if (text || !isRest) {
      this.#options.onCommit(cell, text, isRest);
      this.#note({ kind: "committed", cell, text });
    }
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
    this.#setTouched(false);
    if (event.key === "Escape" && this.#grid.cancelDrag()) {
      event.preventDefault();
      return "swallowed";
    }
    const plain = !event.ctrlKey && !event.metaKey && !event.altKey;

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

    // Undo / Redo while navigating (`Ctrl + Z/Y`, with `Cmd` on macOS).
    if (isCommand(event) && !event.altKey) {
      const lower = event.key.toLowerCase();
      if (lower === "z") {
        event.preventDefault();
        if (!this.#options.isReadOnly?.()) {
          if (event.shiftKey) {
            this.#options.onRedo?.();
          } else {
            this.#options.onUndo?.();
          }
        }
        return "swallowed";
      }
      if (lower === "y" && !event.shiftKey) {
        event.preventDefault();
        if (!this.#options.isReadOnly?.()) {
          this.#options.onRedo?.();
        }
        return "swallowed";
      }
    }

    const { onJumpEdge, onJumpUsedEnd } = this.#options;
    if (navigate(this.#grid, event, { toEdge: onJumpEdge, toUsedEnd: onJumpUsedEnd })) {
      return "moved";
    }
    if (plain && (event.key === "Enter" || event.key === "Tab")) {
      this.#move(event);
      return "moved";
    }
    if (event.key === "F2") {
      if (!this.#options.isReadOnly?.()) {
        this.startEdit("F2");
      }
      return "swallowed";
    }
    if (event.key === "Backspace" || event.key === "Delete") {
      if (!this.#options.isReadOnly?.()) {
        this.#options.onClear?.();
      }
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
    if (type !== "copy" && this.#options.isReadOnly?.()) {
      return true;
    }
    if (type === "paste") {
      const tsv = event.clipboardData?.getData("text/plain") ?? "";
      const html = event.clipboardData?.getData("text/html") || null;
      this.#options.onPaste?.(tsv, html);
    } else {
      this.#options.onCopy?.(type === "cut", event.clipboardData);
    }
    return true;
  }

  /** A tap leaves the cell that is edited, as a click does. It is no way to pick a reference. */
  #onTap = (event: Event): void => {
    if (this.#options.isPointAndClickActive?.()) {
      event.preventDefault();
    } else {
      this.#commit();
    }
  };

  #onPointerDown = (event: PointerEvent): void => {
    this.#setTouched(event.pointerType === "touch");
    // What a finger is there for shows when it is lifted.
    if (
      this.#isTouched ||
      !this.#cell ||
      (event.target instanceof Node && this.#box.contains(event.target))
    ) {
      return;
    }
    if (this.#options.isPointAndClickActive?.()) {
      return;
    }
    delete this.#box.dataset.editing;
    setTimeout(() => {
      this.#commit();
      // A menu that opens with the same press has the focus by now.
      const { activeElement, body } = document;
      if (!activeElement || activeElement === body || this.#host.contains(activeElement)) {
        this.#view.focus();
      }
    }, 0);
  };
}
