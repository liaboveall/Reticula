import { FormulaBarEditor } from "@reticula/grid";
import type { WireCompletionItem, WireFormulaAnalysis } from "@reticula/protocol";
import {
  type Ref,
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { functionParameters, functionSummary, translateEngineText } from "./i18n/engine-text.ts";
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
  readonly nameBoxText: string;
  readonly value: string;
  readonly placeholder: string;
  readonly disabled: boolean;
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

/** What the engine says about a name that has no comment. */
const NO_COMMENT = "Named range";

/** What is said about a worksheet or a name. The formula and the comment of a name are the user's. */
function ownText({ kind }: Pick<WireCompletionItem, "kind">, text: string): string {
  return kind === "namedRange" && text !== NO_COMMENT ? text : translateEngineText(text);
}

export function FormulaBar({
  ref,
  nameBoxText,
  value,
  placeholder,
  disabled,
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
  const latestRef = useRef({
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
  });
  latestRef.current = {
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
      if (event.key === "ArrowDown") {
        event.preventDefault();
        const next = (cur.selectedIndex + 1) % list.length;
        setAssistState({
          forValue: text,
          selectedIndex: next,
          navigated: true,
          dismissed: false,
        });
        return true;
      }
      if (event.key === "ArrowUp") {
        event.preventDefault();
        const next = (cur.selectedIndex - 1 + list.length) % list.length;
        setAssistState({
          forValue: text,
          selectedIndex: next,
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

  useLayoutEffect(() => {
    editorRef.current?.setDisabled(disabled);
  }, [disabled]);

  // The editor takes its name when it is made; the language may change later.
  useEffect(() => {
    mountRef.current?.querySelector('[role="textbox"]')?.setAttribute("aria-label", inputName);
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

  return (
    <div className="formulabar-wrap">
      <div
        className="formulabar"
        data-disabled={disabled ? "" : undefined}
        data-inspected={inspected ? "" : undefined}
      >
        <output
          className="name-box"
          data-testid="name-box"
          aria-label={t("Selected cell or range")}
        >
          {nameBoxText}
        </output>
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
            title={translateEngineText(firstDiagnostic.message)}
          >
            {translateEngineText(firstDiagnostic.message)}
          </span>
        )}
      </div>

      {(signature || completions.length > 0) && (
        <div className="formula-assist" data-testid="formula-assist">
          {signature && (
            <div className="formula-signature" data-testid="formula-signature">
              <span className="sig-fn">{signature.name}</span>
              <span className="sig-punct">(</span>
              {signature.params.map((param, idx) => {
                const isLastFixed = idx === signature.params.length - 1;
                const isActive =
                  idx === signature.activeParam ||
                  (isLastFixed &&
                    signature.activeParam >= signature.params.length &&
                    param.includes("..."));
                return (
                  <span key={`${signature.name}-${param}`}>
                    {idx > 0 && <span className="sig-punct">, </span>}
                    <span className={isActive ? "sig-param active" : "sig-param"}>
                      {functionParameters(param)}
                    </span>
                  </span>
                );
              })}
              <span className="sig-punct">)</span>
              <span className="sig-summary">
                {functionSummary(signature.name, signature.summary)}
              </span>
            </div>
          )}

          {completions.length > 0 && (
            <div
              className="formula-completions"
              role="listbox"
              aria-label={t("Formula completions")}
              data-testid="formula-completions"
            >
              {completions.map((item, idx) => {
                const active = idx === clampedIndex;
                return (
                  <div
                    key={`${item.kind}-${item.label}`}
                    role="option"
                    tabIndex={-1}
                    aria-selected={active}
                    className={active ? "completion-item active" : "completion-item"}
                    data-testid={`completion-${item.label}`}
                    onMouseEnter={() =>
                      setAssistState((prev) => ({
                        forValue: value,
                        selectedIndex: idx,
                        navigated: prev.forValue === value ? prev.navigated : false,
                        dismissed: false,
                      }))
                    }
                    onMouseDown={(e) => {
                      e.preventDefault();
                      onAcceptCompletion(value, item, focused);
                    }}
                  >
                    <span
                      className={
                        item.kind === "function"
                          ? "completion-kind kind-fn"
                          : "completion-kind kind-sheet"
                      }
                    >
                      {item.kind === "function" ? "fx" : "⊞"}
                    </span>
                    <span className="completion-label">{item.label}</span>
                    <span className="completion-sig">
                      {item.kind === "function"
                        ? functionParameters(item.signature)
                        : ownText(item, item.signature)}
                    </span>
                    <span className="completion-summary">
                      {item.kind === "function"
                        ? functionSummary(item.label, item.summary)
                        : ownText(item, item.summary)}
                    </span>
                    {active && <kbd className="completion-kbd">Tab</kbd>}
                  </div>
                );
              })}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
