import {
  Compartment,
  EditorState,
  type Extension,
  type Range,
  StateEffect,
  StateField,
} from "@codemirror/state";
import { Decoration, type DecorationSet, EditorView } from "@codemirror/view";
import type { WireFormulaAnalysis, WireSyntaxTokenKind } from "@reticula/protocol";

/** Dispatches a fresh Rust `WireFormulaAnalysis` payload into a CodeMirror editor. */
export const setFormulaAnalysisEffect = StateEffect.define<WireFormulaAnalysis | null>();

const TOKEN_CLASSES: Readonly<Record<WireSyntaxTokenKind, string>> = {
  equals: "cm-tok-op",
  function: "cm-tok-fn",
  reference: "cm-tok-ref",
  sheetRef: "cm-tok-ref",
  number: "cm-tok-num",
  string: "cm-tok-str",
  boolean: "cm-tok-bool",
  error: "cm-tok-err",
  operator: "cm-tok-op",
  paren: "cm-tok-paren",
  punctuation: "cm-tok-punct",
  unknown: "cm-tok-unknown",
};

function buildDecorations(docLen: number, analysis: WireFormulaAnalysis | null): DecorationSet {
  if (!analysis?.isFormula || docLen === 0) {
    return Decoration.none;
  }
  const ranges: Range<Decoration>[] = [];
  for (const tok of analysis.tokens) {
    const from = Math.max(0, Math.min(docLen, tok.startUtf16));
    const to = Math.max(from, Math.min(docLen, tok.endUtf16));
    if (to <= from) {
      continue;
    }
    const classes = [TOKEN_CLASSES[tok.kind]];
    if (tok.colorIndex !== undefined) {
      classes.push(`cm-tok-ref-${tok.colorIndex & 7}`);
    }
    if (tok.matchedBracket) {
      classes.push("cm-tok-paren-match");
    }
    ranges.push(Decoration.mark({ class: classes.join(" ") }).range(from, to));
  }

  for (const diag of analysis.diagnostics) {
    let from = Math.max(0, Math.min(docLen, diag.startUtf16));
    let to = Math.max(from, Math.min(docLen, diag.endUtf16));
    if (from === to) {
      if (docLen === 0) {
        continue;
      }
      from = Math.max(0, Math.min(docLen - 1, from - 1));
      to = Math.min(docLen, from + 1);
    }
    if (to > from) {
      ranges.push(
        Decoration.mark({
          class: "cm-tok-diag",
          attributes: { title: diag.message },
        }).range(from, to),
      );
    }
  }

  return Decoration.set(ranges, true);
}

/**
 * Shared CodeMirror 6 decoration field rendering Rust language-service tokens,
 * `0..7` colored cell/range references, matched parentheses, and diagnostics.
 */
export function formulaDecorationExtension(): Extension {
  return StateField.define<DecorationSet>({
    create() {
      return Decoration.none;
    },
    update(decorations, tr) {
      let next = tr.docChanged ? decorations.map(tr.changes) : decorations;
      for (const effect of tr.effects) {
        if (effect.is(setFormulaAnalysisEffect)) {
          next = buildDecorations(tr.state.doc.length, effect.value);
        }
      }
      return next;
    },
    provide: (field) => EditorView.decorations.from(field),
  });
}

export interface FormulaBarEditorOptions {
  readonly testId?: string;
  readonly ariaLabel?: string;
  readonly disabled?: boolean;
  readonly onChange?: (text: string, cursorUtf16: number) => void;
  readonly onFocusChange?: (focused: boolean) => void;
  /**
   * Intercepts non-IME `keydown` events before CodeMirror handles them.
   * Return `true` when the key has been consumed (e.g. `Enter`, `Escape`, `Tab`, `F4`).
   */
  readonly onKeyDown?: (event: KeyboardEvent, text: string, cursorUtf16: number) => boolean;
}

/**
 * Lightweight single-line CodeMirror 6 editor used by the top Formula Bar (`14-m1b.md` 6.1).
 * Uses only `@codemirror/state` and `@codemirror/view` already bundled with `@reticula/grid`.
 */
export class FormulaBarEditor {
  #view: EditorView;
  #editable = new Compartment();
  #suppressChange = false;
  #options: FormulaBarEditorOptions;

  constructor(parent: HTMLElement, options: FormulaBarEditorOptions = {}) {
    this.#options = options;
    const disabled = options.disabled ?? false;
    const contentAttrs: Record<string, string> = {
      "aria-multiline": "false",
      ...(options.ariaLabel ? { "aria-label": options.ariaLabel } : {}),
      spellcheck: "false",
      autocorrect: "off",
      autocapitalize: "off",
    };
    if (options.testId) {
      contentAttrs["data-testid"] = options.testId;
    }

    this.#view = new EditorView({
      parent,
      state: EditorState.create({
        extensions: [
          this.#editable.of(EditorView.editable.of(!disabled)),
          EditorView.contentAttributes.of(contentAttrs),
          EditorState.transactionFilter.of((tr) => {
            if (!tr.docChanged || tr.newDoc.lines <= 1) {
              return tr;
            }
            const flattened = tr.newDoc.toString().replace(/\r\n|\r|\n/g, " ");
            return {
              changes: { from: 0, to: tr.startState.doc.length, insert: flattened },
              selection: { anchor: Math.min(flattened.length, tr.newSelection.main.head) },
            };
          }),
          formulaDecorationExtension(),
          EditorView.updateListener.of((update) => {
            if (update.focusChanged) {
              this.#options.onFocusChange?.(update.view.hasFocus);
            }
            if (!this.#suppressChange && (update.docChanged || update.selectionSet)) {
              this.#options.onChange?.(
                update.state.doc.toString(),
                update.state.selection.main.head,
              );
            }
          }),
          EditorView.domEventHandlers({
            focus: (_event, view) => {
              // CodeMirror defers its focus-change update by 10 ms; flush it
              // synchronously on focus so a programmatic focus + select-all
              // (such as Playwright `fill()`) cannot have its DOM selection
              // collapsed back to offset 0 mid-edit.
              if (view.hasFocus) {
                view.update([]);
              }
              return false;
            },
            keydown: (event, view) => {
              if (
                event.isComposing ||
                event.keyCode === 229 ||
                view.composing ||
                view.compositionStarted
              ) {
                return false;
              }
              return (
                this.#options.onKeyDown?.(
                  event,
                  view.state.doc.toString(),
                  view.state.selection.main.head,
                ) ?? false
              );
            },
            compositionend: (_event, view) => {
              if (!this.#suppressChange) {
                this.#options.onChange?.(view.state.doc.toString(), view.state.selection.main.head);
              }
            },
          }),
        ],
      }),
    });
  }

  get value(): string {
    return this.#view.state.doc.toString();
  }

  get cursorUtf16(): number {
    return this.#view.state.selection.main.head;
  }

  get hasFocus(): boolean {
    return this.#view.hasFocus;
  }

  focus(): void {
    this.#view.focus();
  }

  blur(): void {
    this.#view.contentDOM.blur();
  }

  setDisabled(disabled: boolean): void {
    this.#view.dispatch({
      effects: this.#editable.reconfigure(EditorView.editable.of(!disabled)),
    });
  }

  /** Updates displayed text from selection/cell inspection without firing `onChange`. */
  setValue(text: string, cursorUtf16?: number): void {
    const cur = this.#view.state.doc.toString();
    const nextHead =
      cursorUtf16 !== undefined
        ? Math.min(text.length, Math.max(0, cursorUtf16))
        : Math.min(text.length, this.#view.state.selection.main.head);
    if (cur === text && (cursorUtf16 === undefined || this.cursorUtf16 === nextHead)) {
      return;
    }
    this.#suppressChange = true;
    this.#view.dispatch({
      changes: { from: 0, to: this.#view.state.doc.length, insert: text },
      selection: { anchor: nextHead },
    });
    this.#suppressChange = false;
  }

  /** Applies an edit (completion, `F4` toggle, or point-and-click reference) and fires `onChange`. */
  applyTextAndSelection(text: string, anchorUtf16: number, headUtf16 = anchorUtf16): void {
    const clampedAnchor = Math.min(text.length, Math.max(0, anchorUtf16));
    const clampedHead = Math.min(text.length, Math.max(0, headUtf16));
    this.#suppressChange = true;
    this.#view.dispatch({
      changes: { from: 0, to: this.#view.state.doc.length, insert: text },
      selection: { anchor: clampedAnchor, head: clampedHead },
      effects: setFormulaAnalysisEffect.of(null),
      scrollIntoView: true,
    });
    this.#suppressChange = false;
    this.#options.onChange?.(text, clampedHead);
  }

  setAnalysis(analysis: WireFormulaAnalysis | null): void {
    const view = this.#view;
    // Let CodeMirror ingest pending native input before redrawing decorations.
    if (!view.composing && view.contentDOM.textContent === this.value) {
      let selection: { anchor: number; head: number } | undefined;
      if (view.hasFocus) {
        const domSel = view.contentDOM.ownerDocument.getSelection();
        if (
          domSel?.anchorNode &&
          domSel.focusNode &&
          view.contentDOM.contains(domSel.anchorNode) &&
          view.contentDOM.contains(domSel.focusNode)
        ) {
          const anchor = view.posAtDOM(domSel.anchorNode, domSel.anchorOffset);
          const head = view.posAtDOM(domSel.focusNode, domSel.focusOffset);
          const cur = view.state.selection.main;
          if (anchor !== cur.anchor || head !== cur.head) {
            selection = { anchor, head };
          }
        }
      }
      this.#suppressChange = true;
      view.dispatch({
        effects: setFormulaAnalysisEffect.of(analysis),
        ...(selection ? { selection } : {}),
      });
      this.#suppressChange = false;
    }
  }

  destroy(): void {
    this.#view.destroy();
  }
}
