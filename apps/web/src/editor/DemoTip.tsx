import type { SheetInfo } from "@reticula/protocol";
import { useState } from "react";
import { t } from "../i18n/index.ts";
import { tx } from "../i18n/react.ts";

export interface DemoTipProps {
  readonly sheet: SheetInfo | null;
  readonly onJump: () => void;
}

/** What to try first, above the first sheet of a generated sample. */
export function DemoTip({ sheet, onJump }: DemoTipProps) {
  const [open, setOpen] = useState(true);
  if (!open || !sheet || sheet.seed === 0 || sheet.sheet !== 0) {
    return null;
  }
  return (
    <div className="demo-tip" data-testid="demo-tip">
      <span>
        {tx(
          "Try live recalculation: edit parameter cell <0>V1</0> (e.g. type <1>5%</1> and press Enter) to recalculate <2>100,001</2> dependent formulas.",
          { 0: <code />, 1: <code />, 2: <strong /> },
        )}
      </span>
      <button type="button" className="tip-action" data-testid="jump-v1" onClick={onJump}>
        {t("Jump to V1")}
      </button>
      <button
        type="button"
        className="tip-close"
        aria-label={t("Dismiss tip")}
        onClick={() => setOpen(false)}
      >
        ×
      </button>
    </div>
  );
}
