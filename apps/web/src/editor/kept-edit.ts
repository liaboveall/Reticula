/**
 * What was being typed when the app was updated. An update reloads every tab of
 * it; what a tab had not entered yet is kept in its session storage, which a
 * reload keeps, and is put back where it was once the workbook is open again.
 */

/** Read by the editor before this is loaded, to know whether to load it. */
export const KEY = "reticula:kept-edit";
/** A reload takes seconds; what is older is of another visit. */
const FRESH_MS = 5 * 60_000;

export interface KeptEdit {
  /** The workbook it is in: `local:<id>` or `cloud:<id>`. */
  readonly doc: string;
  readonly sheet: number;
  readonly row: number;
  readonly col: number;
  readonly source: string;
  readonly cursorUtf16: number;
  /** Whether it was typed in the formula bar, or in the cell. */
  readonly inFormulaBar: boolean;
}

export function keepEdit(edit: KeptEdit, now = Date.now()): void {
  try {
    sessionStorage.setItem(KEY, JSON.stringify({ ...edit, at: now }));
  } catch {
    // Without storage it is lost as it was before.
  }
}

/** What was kept for this workbook, which is then no longer kept. */
export function takeKeptEdit(doc: string, now = Date.now()): KeptEdit | null {
  let raw: string | null = null;
  try {
    raw = sessionStorage.getItem(KEY);
    if (raw === null) {
      return null;
    }
    sessionStorage.removeItem(KEY);
  } catch {
    return null;
  }
  let kept: unknown;
  try {
    kept = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof kept !== "object" || kept === null) {
    return null;
  }
  const edit = kept as Partial<KeptEdit> & { readonly at?: unknown };
  const isWhole =
    typeof edit.doc === "string" &&
    typeof edit.sheet === "number" &&
    typeof edit.row === "number" &&
    typeof edit.col === "number" &&
    typeof edit.source === "string" &&
    typeof edit.cursorUtf16 === "number" &&
    typeof edit.inFormulaBar === "boolean" &&
    typeof edit.at === "number";
  if (!isWhole || edit.doc !== doc || now - Number(edit.at) > FRESH_MS || Number(edit.at) > now) {
    return null;
  }
  return {
    doc: edit.doc as string,
    sheet: edit.sheet as number,
    row: edit.row as number,
    col: edit.col as number,
    source: edit.source as string,
    cursorUtf16: Math.min(Math.max(0, edit.cursorUtf16 as number), (edit.source as string).length),
    inFormulaBar: edit.inFormulaBar as boolean,
  };
}
