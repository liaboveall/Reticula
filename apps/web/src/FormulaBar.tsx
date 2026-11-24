import { FormulaBarEditor } from "@reticula/grid";
import type { WireCompletionItem, WireFormulaAnalysis } from "@reticula/protocol";
import {
  type ReactNode,
  type Ref,
  useCallback,
  useEffect,
  useId,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { engineText, laterNow, loadLater } from "./editor/load-later.ts";
import { t } from "./i18n/index.ts";

export interface FormulaBarHandle {
  focus(): void;
  applyTextAndSelection(text: string, anchorUtf16: number, headUtf16?: number): void;
  /**
   * Shared completion keyboard handler (`ArrowDown`/`ArrowUp`/`Tab`/navigated `Enter`)
   * so both in-cell editing and formula-bar editing drive the same completion menu.
   */
  handleAssistKeyDown(event: KeyboardEvent, text: string, cursorUtf16: number): boolean;
}

export interface FormulaBarProps {
  readonly ref?: Ref<FormulaBarHandle>;
  /** What shows and takes what is selected. */
  readonly nameBox: ReactNode;
  readonly value: string;
  readonly placeholder: string;
  readonly disabled: boolean;
  /** Whether it has nothing to do with the page at the moment. */
  readonly inert?: boolean;
  readonly isEditing: boolean;
  readonly inspected?: boolean;
  readonly isSpillShadow?: boolean;
  readonly analysis: WireFormulaAnalysis | null;
  readonly analysisSource?: string | null;
  readonly getEditSource: () => string | null;
  readonly onChange: (text: string, cursorUtf16: number) => void;
  readonly onFocus?: () => void;
  readonly onCommit: (moveDown: boolean) => void;
  readonly onCancel: () => void;
  readonly onToggleDollar: (text: string, cursorUtf16: number) => void;
  readonly onAcceptCompletion: (
    currentSource: string,
    item: WireCompletionItem,
    fromFormulaBar: boolean,
  ) => void;
}

const MAX_VISIBLE_COMPLETIONS = 8;

/** The help with formulas, once it is there. Typing goes on without it. */
function useFormulaAssist(wanted: boolean) {
  const [, setLoaded] = useState(false);
  useEffect(() => {
    if (wanted && !laterNow()) {
      loadLater().then(
        () => setLoaded(true),
        () => {},
      );
    }
  }, [wanted]);
  return laterNow()?.FormulaAssist;
}

export function FormulaBar({
  ref,
  nameBox,
  value,
  placeholder,
  disabled,
  inert = false,
  isEditing,
  inspected,
  isSpillShadow,
  analysis,
  analysisSource,
  getEditSource,
  onChange,
  onFocus,
  onCommit,
  onCancel,
  onToggleDollar,
  onAcceptCompletion,
}: FormulaBarProps) {
  const inputName = t("Formula bar");
  const mountRef = useRef<HTMLDivElement>(null);
  const editorRef = useRef<FormulaBarEditor | null>(null);
  const [focused, setFocused] = useState(false);
  const [assistState, setAssistState] = useState<{
    readonly forValue: string;
    readonly selectedIndex: number;
    readonly navigated: boolean;
    readonly dismissed: boolean;
  }>({ forValue: value, selectedIndex: 0, navigated: false, dismissed: false });

  const currentAssist =
    assistState.forValue === value
      ? assistState
      : { forValue: value, selectedIndex: 0, navigated: false, dismissed: false };

  // What has been chosen from the list of one edit means nothing in the next.
  useEffect(() => {
    if (!isEditing) {
      setAssistState({ forValue: "", selectedIndex: 0, navigated: false, dismissed: false });
    }
  }, [isEditing]);

  const analysisMatchesValue = analysisSource === undefined || analysisSource === value;

  const completions =
    isEditing &&
    analysisMatchesValue &&
    analysis?.isFormula &&
    !currentAssist.dismissed &&
    analysis.completions.length > 0
      ? analysis.completions.slice(0, MAX_VISIBLE_COMPLETIONS)
      : [];

  // Keep latest callbacks/completions in a ref for CodeMirror's synchronous DOM keydown listener.
  const latest = {
    value,
    inputName,
    disabled,
    analysisSource,
    completions,
    selectedIndex: currentAssist.selectedIndex,
    navigatedList: currentAssist.navigated,
    onChange,
    onFocus,
    onCommit,
    onCancel,
    onToggleDollar,
    onAcceptCompletion,
  };
  const latestRef = useRef(latest);
  latestRef.current = latest;

  const handleAssistKey = useCallback(
    (
      event: KeyboardEvent,
      text: string,
      _cursorUtf16: number,
      fromFormulaBar: boolean,
    ): boolean => {
      const cur = latestRef.current;
      if (cur.analysisSource !== undefined && cur.analysisSource !== text) {
        return false;
      }
      const list = cur.completions;
      if (list.length === 0) {
        return false;
      }
      const step = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
      if (step !== 0) {
        event.preventDefault();
        setAssistState({
          forValue: text,
          selectedIndex: (cur.selectedIndex + step + list.length) % list.length,
          navigated: true,
          dismissed: false,
        });
        return true;
      }
      if (event.key === "Tab" && !event.shiftKey && !event.ctrlKey && !event.metaKey) {
        const picked = list[cur.selectedIndex] ?? list[0];
        if (picked) {
          event.preventDefault();
          cur.onAcceptCompletion(text, picked, fromFormulaBar);
          return true;
        }
      }
      if (event.key === "Enter" && cur.navigatedList) {
        const picked = list[cur.selectedIndex];
        if (picked) {
          event.preventDefault();
          cur.onAcceptCompletion(text, picked, fromFormulaBar);
          return true;
        }
      }
      if (event.key === "Escape" && fromFormulaBar && cur.navigatedList) {
        event.preventDefault();
        setAssistState({
          forValue: text,
          selectedIndex: 0,
          navigated: false,
          dismissed: true,
        });
        return true;
      }
      return false;
    },
    [],
  );

  useEffect(() => {
    const parent = mountRef.current;
    if (!parent) {
      return;
    }
    const cm = new FormulaBarEditor(parent, {
      testId: "formula-input",
      ariaLabel: latestRef.current.inputName,
      disabled: latestRef.current.disabled,
      onChange: (text, cursorUtf16) => {
        latestRef.current.onChange(text, cursorUtf16);
      },
      onFocusChange: (isFocused) => {
        setFocused(isFocused);
        if (isFocused) {
          latestRef.current.onFocus?.();
        }
      },
      onKeyDown: (event, text, cursorUtf16) => {
        if (handleAssistKey(event, text, cursorUtf16, true)) {
          return true;
        }
        if (event.key === "F4" && text.startsWith("=")) {
          event.preventDefault();
          latestRef.current.onToggleDollar(text, cursorUtf16);
          return true;
        }
        if (event.key === "Enter") {
          event.preventDefault();
          latestRef.current.onCommit(!event.shiftKey);
          return true;
        }
        if (event.key === "Escape") {
          event.preventDefault();
          latestRef.current.onCancel();
          return true;
        }
        return false;
      },
    });
    editorRef.current = cm;
    cm.setValue(latestRef.current.value);
    return () => {
      cm.destroy();
      editorRef.current = null;
    };
  }, [handleAssistKey]);

  // What cannot be typed into is still there to be read.
  useLayoutEffect(() => {
    editorRef.current?.setDisabled(disabled);
    const content = mountRef.current?.querySelector(".cm-content");
    if (disabled) {
      content?.setAttribute("tabindex", "0");
      content?.setAttribute("aria-readonly", "true");
    } else {
      content?.removeAttribute("tabindex");
      content?.removeAttribute("aria-readonly");
    }
  }, [disabled]);

  // The editor takes its name when it is made; the language may change later.
  useEffect(() => {
    mountRef.current?.querySelector(".cm-content")?.setAttribute("aria-label", inputName);
  }, [inputName]);

  useLayoutEffect(() => {
    // A React render can lag behind the current CodeMirror edit.
    if (getEditSource() === (isEditing ? value : null)) {
      editorRef.current?.setValue(value);
    }
  }, [getEditSource, isEditing, value]);

  useEffect(() => {
    const editor = editorRef.current;
    const source = getEditSource();
    if (editor && source === (isEditing ? editor.value : null)) {
      editor.setAnalysis(isEditing && analysisSource === source ? analysis : null);
    }
  }, [analysis, analysisSource, getEditSource, isEditing]);

  useImperativeHandle(ref, () => ({
    focus() {
      editorRef.current?.focus();
    },
    applyTextAndSelection(text, anchorUtf16, headUtf16 = anchorUtf16) {
      editorRef.current?.applyTextAndSelection(text, anchorUtf16, headUtf16);
    },
    handleAssistKeyDown(event, text, cursorUtf16) {
      return handleAssistKey(event, text, cursorUtf16, false);
    },
  }));

  const signature =
    isEditing && analysisMatchesValue && analysis?.isFormula ? analysis.signature : undefined;
  const firstDiagnostic =
    isEditing && analysisMatchesValue && analysis?.isFormula && analysis.diagnostics.length > 0
      ? analysis.diagnostics[0]
      : undefined;
  const clampedIndex = Math.min(currentAssist.selectedIndex, Math.max(0, completions.length - 1));
  const hasList = completions.length > 0;
  const Assist = useFormulaAssist(signature !== undefined || hasList);
  const listId = useId();
  const option = currentAssist.navigated ? `${listId}-${clampedIndex}` : null;

  // Of the two editors of a formula, the list is for the one that has the focus.
  useEffect(() => {
    const input = document.activeElement;
    if (!hasList || !Assist || !input?.matches(".cm-content")) {
      return;
    }
    const attributes = new Map([
      ["aria-haspopup", "listbox"],
      ["aria-autocomplete", "list"],
      ["aria-controls", listId],
    ]);
    if (option !== null) {
      attributes.set("aria-activedescendant", option);
    }
    for (const [name, setting] of attributes) {
      input.setAttribute(name, setting);
    }
    return () => {
      for (const name of attributes.keys()) {
        input.removeAttribute(name);
      }
    };
  }, [Assist, hasList, listId, option]);

  return (
    <div className="formulabar-wrap" inert={inert}>
      <div
        className="formulabar"
        data-disabled={disabled ? "" : undefined}
        data-inspected={inspected ? "" : undefined}
      >
        {nameBox}
        {isSpillShadow && (
          <span
            className="spill-badge"
            data-testid="spill-badge"
            title={t("Spilled from dynamic array formula")}
          >
            {t("SPILL")}
          </span>
        )}
        <span
          className={value.startsWith("=") ? "fx-badge fx-active" : "fx-badge"}
          aria-hidden="true"
        >
          fx
        </span>
        <div className={focused ? "formula-cm-wrap focused" : "formula-cm-wrap"}>
          {value.length === 0 && !focused && (
            <span className="formula-placeholder" aria-hidden="true">
              {placeholder}
            </span>
          )}
          <div ref={mountRef} className="formula-cm-host" />
        </div>
        {firstDiagnostic && (
          <span
            className="formula-diag-pill"
            data-testid="formula-diagnostic"
            title={engineText(firstDiagnostic.message)}
          >
            {engineText(firstDiagnostic.message)}
          </span>
        )}
      </div>

      {Assist && (signature || hasList) && (
        <Assist
          id={listId}
          signature={signature}
          completions={completions}
          selected={clampedIndex}
          onSelect={(index) =>
            setAssistState((prev) => ({
              forValue: value,
              selectedIndex: index,
              navigated: prev.forValue === value ? prev.navigated : false,
              dismissed: false,
            }))
          }
          onAccept={(item) => onAcceptCompletion(value, item, focused)}
        />
      )}
    </div>
  );
}
