/** What `completeness.test.ts` lets pass. Entries nothing needs any more fail the test. */

export interface AllowedText {
  readonly file: string;
  readonly text: string;
  readonly reason: string;
}

const KEY = "the name of a key";
const SAMPLE = "what the sample's cells contain";
const GLYPH = "a sign, with a name of its own";
const TITLE = "a title that is stored with the workbook";
const BRAND = "the name of the product";

/** Texts that are the same in every language. */
export const ALLOWED: readonly AllowedText[] = [
  { file: "apps/web/src/EditorPage.tsx", text: "Sample sales data", reason: TITLE },
  { file: "apps/web/src/EditorPage.tsx", text: "Untitled workbook", reason: TITLE },
  { file: "apps/web/src/FormulaBar.tsx", text: "fx", reason: GLYPH },
  { file: "apps/web/src/LocalModals.tsx", text: "Sample 1M ({})", reason: TITLE },
  { file: "apps/web/src/LocalModals.tsx", text: "Untitled workbook", reason: TITLE },
  { file: "apps/web/src/LocalModals.tsx", text: "Workbook {}", reason: TITLE },
  { file: "apps/web/src/M3aToolsModal.tsx", text: "TAX_RATE", reason: "a name to go by" },
  {
    file: "apps/web/src/ai/labels.ts",
    text: "Completed analysis with no workbook changes.",
    reason: "what the server writes, which is translated as its text",
  },
  { file: "apps/web/src/collab/CollabShell.tsx", text: "Cloud Workbook", reason: TITLE },
  {
    file: "apps/web/src/collab/labels.ts",
    text: "Deleted user",
    reason: "what the server calls it",
  },
  {
    file: "apps/web/src/collab/labels.ts",
    text: "Link-view access",
    reason: "a label that is stored with the link",
  },
  { file: "apps/web/src/commands/Dialogs.tsx", text: "Enter / Tab", reason: KEY },
  { file: "apps/web/src/commands/Dialogs.tsx", text: "Home / End / {}Home / {}End", reason: KEY },
  { file: "apps/web/src/commands/Dialogs.tsx", text: "PageUp / PageDown", reason: KEY },
  { file: "apps/web/src/editor/FormatToolbar.tsx", text: "B", reason: GLYPH },
  { file: "apps/web/src/editor/FormatToolbar.tsx", text: "I", reason: GLYPH },
  { file: "apps/web/src/editor/FormatToolbar.tsx", text: "S", reason: GLYPH },
  {
    file: "apps/web/src/editor/FormulaAssist.tsx",
    text: "Named range",
    reason: "what the engine writes, which is translated as its text",
  },
  { file: "apps/web/src/editor/FormulaAssist.tsx", text: "Tab", reason: KEY },
  { file: "apps/web/src/editor/FormulaAssist.tsx", text: "fx", reason: GLYPH },
  {
    file: "apps/web/src/editor/MobileFallback.tsx",
    text: "M2..M100001: =ROUND(J2*K2*(1-L2-$V$1), 2)",
    reason: SAMPLE,
  },
  { file: "apps/web/src/editor/MobileFallback.tsx", text: "← Reticula", reason: BRAND },
  { file: "apps/web/src/editor/TitleBar.tsx", text: "Reticula", reason: BRAND },
  { file: "apps/web/src/landing/names.ts", text: "Canvas 2D", reason: BRAND },
  { file: "apps/web/src/pages/WorkbooksPage.tsx", text: "Cloud Workbook", reason: TITLE },
  { file: "apps/web/src/pages/WorkbooksPage.tsx", text: "Untitled Cloud Workbook", reason: TITLE },
  {
    file: "apps/web/src/sw.ts",
    text: "Reticula is offline and the cached shell is not available yet.",
    reason: "the Service Worker answers in place of the app",
  },
];

/** Modules that are still in English only. Every one of them has to have some left. */
export const PENDING: readonly string[] = [];
