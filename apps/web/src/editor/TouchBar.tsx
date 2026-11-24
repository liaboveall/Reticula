import type { ReactNode } from "react";
import { t } from "../i18n/index.ts";
import type { CellInspection } from "./cell-announcer.ts";

export interface TouchBarProps {
  /** What shows and takes what is selected. */
  readonly nameBox: ReactNode;
  /** What is known of the active cell. */
  readonly inspection: CellInspection | null;
  /** What it shows as long as nothing else is known. */
  readonly text: string;
  readonly inert: boolean;
}

/** What is in the active cell, all of it, where there is no formula bar. */
export default function TouchBar({ nameBox, inspection, text, inert }: TouchBarProps) {
  const formula = inspection?.isFormula ? inspection.raw : null;
  return (
    <section
      className="formulabar touchbar"
      data-testid="touch-bar"
      aria-label={t("Contents of the active cell")}
      inert={inert}
    >
      {nameBox}
      <div className="touchbar-text">
        <div data-testid="touch-value">{inspection?.display ?? text}</div>
        {formula !== null && <code data-testid="touch-formula">{formula}</code>}
      </div>
    </section>
  );
}
