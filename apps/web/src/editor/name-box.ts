import type { Grid } from "@reticula/grid";
import type { SheetInfo } from "@reticula/protocol";
import type { EditorHandle } from "../commands/handle.ts";
import { t } from "../i18n/index.ts";

/** Rows and columns by their places in the sheet itself, where rows a filter hides count. */
export interface Area {
  readonly scope: "cells" | "rows" | "cols";
  readonly firstRow: number;
  readonly endRow: number;
  readonly firstCol: number;
  readonly endCol: number;
}

export interface Reference {
  /** The name of a worksheet, or `null` for the one in view. */
  readonly sheet: string | null;
  readonly areas: readonly Area[];
}

const MAX_ROWS = 2 ** 31;

/** The position of a column by its letters. */
function columnAt(letters: string): number {
  let position = 0;
  for (const letter of letters.toUpperCase()) {
    position = position * 26 + (letter.charCodeAt(0) - 64);
  }
  return position - 1;
}

const SHEET = /^(?:'((?:[^']|'')+)'|([^'!,;:]+))!/;
const CELLS = /^\$?([a-z]{1,3})\$?([1-9]\d{0,9})(?::\$?([a-z]{1,3})\$?([1-9]\d{0,9}))?$/i;
const ROWS = /^\$?([1-9]\d{0,9}):\$?([1-9]\d{0,9})$/;
const COLS = /^\$?([a-z]{1,3}):\$?([a-z]{1,3})$/i;

function between(one: number, other: number): readonly [first: number, end: number] {
  return [Math.min(one, other), Math.max(one, other) + 1];
}

function parseArea(text: string): Area | null {
  const cells = CELLS.exec(text);
  if (cells) {
    const [, col = "", row = "", lastCol = col, lastRow = row] = cells;
    const [firstRow, endRow] = between(Number(row) - 1, Number(lastRow) - 1);
    const [firstCol, endCol] = between(columnAt(col), columnAt(lastCol));
    return { scope: "cells", firstRow, endRow, firstCol, endCol };
  }
  const rows = ROWS.exec(text);
  if (rows) {
    const [firstRow, endRow] = between(Number(rows[1]) - 1, Number(rows[2]) - 1);
    return { scope: "rows", firstRow, endRow, firstCol: 0, endCol: 1 };
  }
  const cols = COLS.exec(text);
  if (cols) {
    const [firstCol, endCol] = between(columnAt(cols[1] ?? ""), columnAt(cols[2] ?? ""));
    return { scope: "cols", firstRow: 0, endRow: 1, firstCol, endCol };
  }
  return null;
}

/** What is between the commas, of which those in the name of a worksheet are none. */
function partsOf(text: string): string[] {
  return text.split(/[,;，；](?=(?:[^']*'[^']*')*[^']*$)/).map((part) => part.trim());
}

/**
 * Cells, whole rows or columns, or several of them, of one worksheet, as in
 * `A1:B2,D4` or `'Sheet 2'!5:5`. `null` for anything else.
 */
export function parseReference(text: string): Reference | null {
  let sheet: string | null = null;
  const areas: Area[] = [];
  for (const part of partsOf(text)) {
    const prefix = SHEET.exec(part);
    if (prefix) {
      const [, quoted, plain = ""] = prefix;
      const name = quoted === undefined ? plain.trim() : quoted.replaceAll("''", "'");
      if (areas.length > 0 && name !== sheet) {
        return null;
      }
      sheet = name;
    }
    const area = parseArea(part.slice(prefix?.[0].length ?? 0).replace(/\s+/g, ""));
    if (!area || area.endRow > MAX_ROWS) {
      return null;
    }
    areas.push(area);
  }
  return areas.length > 0 ? { sheet, areas } : null;
}

/** Where the rows between two are shown that a filter leaves, or `null` if it leaves none. */
export function shownRows(
  { firstRow, endRow }: Pick<Area, "firstRow" | "endRow">,
  hidden: readonly (readonly [number, number])[],
): { readonly firstRow: number; readonly endRow: number } | null {
  const shownBefore = (row: number) =>
    hidden.reduce(
      (shown, [first, last]) => shown - Math.max(0, Math.min(last + 1, row) - first),
      row,
    );
  const first = shownBefore(firstRow);
  const end = shownBefore(endRow);
  return end > first ? { firstRow: first, endRow: end } : null;
}

/** How long to wait for the engine to list the names there are. */
const PATIENCE_MS = 2_000;

/** What a name stands for, as it was defined. */
async function lookUpName(handle: EditorHandle, text: string): Promise<string | null> {
  const name = text.trim().toLocaleLowerCase();
  let stop = () => {};
  const formula = await new Promise<string | null>((resolve) => {
    stop = handle.intercept((message) => {
      if (message.kind !== "namedRangesListed") {
        return false;
      }
      const named = message.items.filter((item) => item.name.toLocaleLowerCase() === name);
      const own = named.find(({ scope }) => scope === handle.sheetId());
      resolve((own ?? named.find(({ scope }) => scope === undefined))?.formula ?? null);
      return true;
    });
    setTimeout(resolve, PATIENCE_MS, null);
    if (!handle.send({ kind: "listNamedRanges" })) {
      resolve(null);
    }
  });
  stop();
  return formula;
}

function isWithin(area: Area, { totalRows, cols }: SheetInfo): boolean {
  return (
    (area.scope === "cols" || area.firstRow < totalRows) &&
    (area.scope === "rows" || area.firstCol < cols)
  );
}

/** Selects what a text refers to. Returns what is wrong with it, if anything is. */
export async function goTo(handle: EditorHandle, text: string): Promise<string | null> {
  const { sheet, table } = handle.state();
  if (table) {
    return table.view?.goTo(text) ?? null;
  }
  if (!sheet) {
    return t("There is no worksheet to go to.");
  }
  let reference = parseReference(text);
  // What is like a cell that there is none of may be a name, such as XFE1.
  if (
    !reference ||
    (reference.sheet === null && !reference.areas.every((area) => isWithin(area, sheet)))
  ) {
    const formula = await lookUpName(handle, text);
    if (formula !== null) {
      reference = parseReference(formula.replace(/^=/, ""));
      if (!reference) {
        return t("“{name}” does not stand for cells.", { name: text.trim() });
      }
    }
  }
  if (!reference) {
    return t("“{name}” is neither a reference nor a name.", { name: text.trim() });
  }
  const { areas, sheet: wanted } = reference;
  const tab =
    wanted === null
      ? sheet.sheets.find(({ id }) => id === sheet.sheet)
      : sheet.sheets.find(({ name }) => name.toLocaleLowerCase() === wanted.toLocaleLowerCase());
  if (!tab) {
    return t("There is no worksheet “{name}”.", { name: wanted ?? "" });
  }
  // How large another one is, and what is hidden there, is known once it is in view.
  return new Promise((resolve) => {
    handle.onSheet(tab.id, (grid, arrived = sheet) => resolve(select(grid, arrived, areas)));
  });
}

/** Selects cells of the worksheet in view. Returns what is wrong with them, if anything is. */
function select(grid: Grid, sheet: SheetInfo, areas: readonly Area[]): string | null {
  if (!areas.every((area) => isWithin(area, sheet))) {
    return t("That is outside the worksheet.");
  }
  const hidden = sheet.filter?.hiddenRowSpans ?? [];
  const shown = areas.map((area) => ({ ...area, rows: shownRows(area, hidden) }));
  if (shown.some(({ scope, rows }) => scope !== "cols" && rows === null)) {
    return t("Those rows are hidden by the filter.");
  }
  for (const [at, { scope, rows, firstCol, endCol }] of shown.entries()) {
    const { firstRow, endRow } = rows ?? { firstRow: 0, endRow: 1 };
    const { rows: allRows, cols: allCols } = grid.sheetSize;
    const span = {
      firstRow: scope === "cols" ? 0 : firstRow,
      endRow: scope === "cols" ? allRows : Math.min(endRow, allRows),
      firstCol: scope === "rows" ? 0 : firstCol,
      endCol: scope === "rows" ? allCols : Math.min(endCol, allCols),
    };
    if (at > 0) {
      grid.addSelectionRange(span);
    } else if (scope === "rows") {
      grid.selectRows(firstRow, endRow - 1);
    } else if (scope === "cols") {
      grid.selectCols(firstCol, endCol - 1);
    } else {
      grid.selectRange({ row: firstRow, col: firstCol }, { row: endRow - 1, col: endCol - 1 });
    }
    if (at === 0) {
      const { row, col } = grid.selection ?? { row: firstRow, col: firstCol };
      grid.reveal(scope === "cols" ? row : firstRow, scope === "rows" ? col : firstCol);
    }
  }
  return null;
}
