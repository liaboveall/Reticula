import type { WireCompletionItem, WireSignatureHelp } from "@reticula/protocol";
import { useEffect } from "react";
import { functionParameters, functionSummary, translateEngineText } from "../i18n/engine-text.ts";
import { t, tn } from "../i18n/index.ts";
import { announce } from "../ui/announcer.ts";

export interface FormulaAssistProps {
  readonly id: string;
  readonly signature: WireSignatureHelp | undefined;
  readonly completions: readonly WireCompletionItem[];
  readonly selected: number;
  readonly onSelect: (index: number) => void;
  readonly onAccept: (item: WireCompletionItem) => void;
}

/** What the engine says about a name that has no comment. */
const NO_COMMENT = "Named range";

/** What is said about a worksheet or a name. The formula and the comment of a name are the user's. */
function ownText({ kind }: Pick<WireCompletionItem, "kind">, text: string): string {
  return kind === "namedRange" && text !== NO_COMMENT ? text : translateEngineText(text);
}

/** The function that is being typed, and how what has been typed may go on. */
export default function FormulaAssist({
  id,
  signature,
  completions,
  selected,
  onSelect,
  onAccept,
}: FormulaAssistProps) {
  const first = completions[0]?.label;
  const { length } = completions;
  useEffect(() => {
    if (first !== undefined) {
      announce(tn(length, "{item}, {n} suggestion", "{item}, {n} suggestions", { item: first }));
    }
  }, [first, length]);

  return (
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
          <span className="sig-summary">{functionSummary(signature.name, signature.summary)}</span>
        </div>
      )}

      {completions.length > 0 && (
        <div
          id={id}
          className="formula-completions"
          role="listbox"
          aria-label={t("Formula completions")}
          data-testid="formula-completions"
        >
          {completions.map((item, idx) => {
            const active = idx === selected;
            return (
              <div
                key={`${item.kind}-${item.label}`}
                id={`${id}-${idx}`}
                role="option"
                tabIndex={-1}
                aria-selected={active}
                className={active ? "completion-item active" : "completion-item"}
                data-testid={`completion-${item.label}`}
                onMouseEnter={() => onSelect(idx)}
                onMouseDown={(e) => {
                  e.preventDefault();
                  onAccept(item);
                }}
              >
                <span
                  className={
                    item.kind === "function"
                      ? "completion-kind kind-fn"
                      : "completion-kind kind-sheet"
                  }
                  aria-hidden="true"
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
                {active && (
                  <span className="completion-kbd" aria-hidden="true">
                    Tab
                  </span>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
