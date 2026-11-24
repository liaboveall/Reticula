import type { WireFormulaAnalysis } from "@reticula/protocol";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { FormulaBar } from "./FormulaBar.tsx";
import type { Catalogue } from "./i18n/index.ts";
import zhEngine from "./i18n/zh-engine.ts";
import zhFunctions from "./i18n/zh-functions.ts";

// The editor of the formula is made when the bar is mounted, which it is not here.
vi.mock("@reticula/grid", () => ({ FormulaBarEditor: class {} }));

let shown: Readonly<Record<string, Catalogue>> = {};

vi.mock("./i18n/index.ts", async (original) => ({
  ...(await original<typeof import("./i18n/index.ts")>()),
  catalogueOf: (name: string) => shown[name],
}));

const SUMIFS = "Sums cells matching multiple criteria.";
const ITEM = { insertText: "", replaceStartUtf16: 1, replaceEndUtf16: 3 };

const analysis: WireFormulaAnalysis = {
  isFormula: true,
  tokens: [],
  diagnostics: [{ startUtf16: 0, endUtf16: 1, message: "Unknown function 'SUMS'." }],
  completions: [
    {
      ...ITEM,
      label: "SUMIFS",
      kind: "function",
      signature: "(sum_range, criteria_range1, criteria1, ...)",
      summary: SUMIFS,
    },
    {
      ...ITEM,
      label: "Sums",
      kind: "sheet",
      signature: "Worksheet",
      summary: "Reference cells on this worksheet.",
    },
    {
      ...ITEM,
      label: "Summary",
      kind: "namedRange",
      signature: "=Sums!$A$1",
      summary: "Named range",
    },
    {
      ...ITEM,
      label: "Sumtotal",
      kind: "namedRange",
      signature: "='My row 5'!$A$1",
      summary: "Delete old totals",
    },
  ],
  signature: {
    name: "SUMIFS",
    params: ["sum_range", "criteria_range1", "criteria1", "..."],
    activeParam: 0,
    summary: SUMIFS,
  },
  references: [],
  canInsertRef: false,
  activeRefSpan: undefined,
};

function shownText(): string {
  const markup = renderToStaticMarkup(
    createElement(FormulaBar, {
      nameBoxText: "A1",
      value: "=SUMS",
      placeholder: "",
      disabled: false,
      isEditing: true,
      analysis,
      getEditSource: () => "=SUMS",
      onChange: () => {},
      onCommit: () => {},
      onCancel: () => {},
      onToggleDollar: () => {},
      onAcceptCompletion: () => {},
    }),
  );
  return markup.replace(/<[^>]*>/g, "\n").replace(/&#x27;/g, "'");
}

beforeEach(() => {
  shown = {};
});

describe("what the formula bar says about a formula", () => {
  it("is in English, as the engine wrote it", () => {
    const text = shownText();
    for (const part of [
      "Unknown function 'SUMS'.",
      "criteria_range1",
      "(sum_range, criteria_range1, criteria1, ...)",
      SUMIFS,
      "Worksheet",
      "Reference cells on this worksheet.",
      "Named range",
    ]) {
      expect(text).toContain(part);
    }
  });

  it("is in Chinese, but for the names and the formulas", () => {
    shown = { "zh-engine": zhEngine, "zh-functions": zhFunctions };
    const text = shownText();
    for (const part of [
      "未知的函数“SUMS”。",
      "条件区域1",
      "(求和区域, 条件区域1, 条件1, ...)",
      "对满足多个条件的单元格求和。",
      "工作表",
      "引用此工作表中的单元格。",
      "\n名称\n",
      "SUMIFS",
      "=Sums!$A$1",
      "='My row 5'!$A$1",
      "\nDelete old totals\n",
    ]) {
      expect(text).toContain(part);
    }
    expect(text).not.toMatch(/[a-z]+_[a-z]+|cells|range/);
  });
});
