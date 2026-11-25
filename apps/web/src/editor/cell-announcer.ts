/** What is read out of the grid: where the selection is, and what has come of an action. */

import { columnName, type Selection, type SelectionScope, type Span } from "@reticula/grid";
import type { SheetInfo, WireSpillAnchor, WireValueKind } from "@reticula/protocol";
import { type RefObject, useEffect, useRef } from "react";
import { translateEngineText } from "../i18n/engine-text.ts";
import { fmtInt, t } from "../i18n/index.ts";
import { announce } from "../ui/announcer.ts";
import { type Activity, actionText } from "./activity.ts";

/** What the engine says about a cell, as far as it is read out. */
export interface CellInspection {
  /** In the sheet itself: rows a filter hides count. */
  readonly address: string;
  readonly raw: string;
  readonly display: string;
  readonly isFormula: boolean;
  readonly error: string | null;
  readonly valueKind: WireValueKind;
  readonly spillAnchor: WireSpillAnchor | null;
}

/** The number of a row in the sheet itself, by its place among the rows shown. */
export type RowNumber = (row: number) => number;

/** Longer texts are cut: listening is not skimming. */
const MOST_READ = 200;

function shortened(text: string): string {
  if (text.length <= MOST_READ) {
    return text;
  }
  // Not between the halves of a character.
  const end = (text.codePointAt(MOST_READ - 1) ?? 0) > 0xffff ? MOST_READ - 1 : MOST_READ;
  return t("{text}, and {n} more characters", {
    text: text.slice(0, end),
    n: fmtInt(text.length - end),
  });
}

function valueText({ display, error, valueKind }: CellInspection): string {
  if (error !== null) {
    return t("error {error}", { error });
  }
  if (display === "") {
    return valueKind === "text" ? t("empty text") : t("blank|cell");
  }
  return shortened(display);
}

function originText({ isFormula, raw, spillAnchor }: CellInspection): string | null {
  if (spillAnchor) {
    const { address, formula } = spillAnchor;
    return t("spilled from {cell}, formula {formula}", {
      cell: address,
      formula: shortened(formula),
    });
  }
  return isFormula ? t("formula {formula}", { formula: shortened(raw) }) : null;
}

/** A cell with what it shows and where that comes from. */
export function cellText(inspection: CellInspection): string {
  const cell = inspection.address;
  const value = valueText(inspection);
  const origin = originText(inspection);
  return origin === null
    ? t("{cell}, {value}", { cell, value })
    : t("{cell}, {value}, {origin}", { cell, value, origin });
}

const rowsOf = (span: Span) => span.endRow - span.firstRow;
const colsOf = (span: Span) => span.endCol - span.firstCol;

/** What a column is called, if not by letters. */
export type ColumnName = (col: number) => string;

/** A cell by its column and the number of its row. A name of a column's own may end with a digit. */
function cellName(col: number, row: number, columnOf: ColumnName | undefined): string {
  return columnOf ? `${columnOf(col)} ${row}` : `${columnName(col)}${row}`;
}

function rangeText(
  span: Span,
  scope: SelectionScope,
  rowNumber: RowNumber,
  columnOf?: ColumnName,
): string {
  const firstRow = rowNumber(span.firstRow);
  const lastRow = rowNumber(span.endRow - 1);
  const firstCol = (columnOf ?? columnName)(span.firstCol);
  const lastCol = (columnOf ?? columnName)(span.endCol - 1);
  switch (scope) {
    case "all":
      return t("All cells selected");
    case "rows":
      return rowsOf(span) === 1
        ? t("Row {row} selected", { row: firstRow })
        : t("Rows {first} to {last} selected, {n} rows", {
            first: firstRow,
            last: lastRow,
            n: fmtInt(rowsOf(span)),
          });
    case "cols":
      return colsOf(span) === 1
        ? t("Column {col} selected", { col: firstCol })
        : t("Columns {first} to {last} selected, {n} columns", {
            first: firstCol,
            last: lastCol,
            n: fmtInt(colsOf(span)),
          });
    case "cells": {
      const cells = rowsOf(span) * colsOf(span);
      const first = cellName(span.firstCol, firstRow, columnOf);
      return cells === 1
        ? t("{cell} selected", { cell: first })
        : t("{first} to {last} selected, {n} cells", {
            first,
            last: cellName(span.endCol - 1, lastRow, columnOf),
            n: fmtInt(cells),
          });
    }
  }
}

function isSingleCell({ range, ranges, scope }: Selection): boolean {
  return scope === "cells" && ranges.length === 1 && rowsOf(range) * colsOf(range) === 1;
}

/** Where the selection is, or `null` while there is nothing to say of its cell yet. */
export function selectionText(
  selection: Selection,
  inspection: CellInspection | null,
  rowNumber: RowNumber,
): string | null {
  if (isSingleCell(selection)) {
    return inspection && cellText(inspection);
  }
  const { range, ranges, scope } = selection;
  const text = rangeText(range, scope, rowNumber);
  return ranges.length === 1
    ? text
    : t("{range}, {n} ranges selected", { range: text, n: ranges.length });
}

/** The same where there is no engine to ask: the cell as the grid shows it. */
export function plainSelectionText(selection: Selection, columnOf?: ColumnName): string {
  const cell = cellName(selection.col, selection.row + 1, columnOf);
  const value = selection.text ? shortened(selection.text) : t("blank|cell");
  return isSingleCell(selection)
    ? t("{cell}, {value}", { cell, value })
    : rangeText(selection.range, selection.scope, (row) => row + 1, columnOf);
}

type SheetState = Pick<
  SheetInfo,
  "sheet" | "sheetName" | "rows" | "totalRows" | "undoLabel" | "redoLabel"
> & {
  readonly filter?: { readonly activeCols: readonly number[] } | undefined;
};

/** What a filter that was set, changed or cleared has come to. */
export function filterText(before: SheetState, after: SheetState): string | null {
  const hidden = before.totalRows - before.rows;
  const hides = after.totalRows - after.rows;
  const total = fmtInt(after.totalRows);
  if (!after.filter) {
    return !before.filter
      ? null
      : hidden > 0
        ? t("Filter off, all {total} rows shown", { total })
        : t("Filter off");
  }
  const columns = before.filter?.activeCols.join() ?? "";
  if (hides !== hidden || after.filter.activeCols.join() !== columns) {
    return t("{n} of {total} rows shown", { n: fmtInt(after.rows), total });
  }
  return before.filter ? null : t("Filter on");
}

/** What an action has come to. Recalculation says nothing of itself. */
export function outcomeText(
  activity: Activity,
  before: SheetState,
  after: SheetState,
): string | null {
  if (activity.kind !== "done") {
    return null;
  }
  const { undoLabel, redoLabel } = after;
  switch (activity.action) {
    // The sheet is read by its name, and what was entered as it was.
    case "updatedCell":
    case "switchedSheet":
      return null;
    // There may have been nothing to undo or to redo.
    case "undidAction":
      return before.undoLabel && redoLabel
        ? t("Undid: {label}", { label: translateEngineText(redoLabel) })
        : null;
    case "redidAction":
      return before.redoLabel && undoLabel
        ? t("Redid: {label}", { label: translateEngineText(undoLabel) })
        : null;
    case "updated":
      // Sorting, for instance; but not what others do, which cannot be undone here.
      return undoLabel && undoLabel !== before.undoLabel
        ? t("Done: {label}", { label: translateEngineText(undoLabel) })
        : null;
    default:
      return actionText(activity.action);
  }
}

/** Says where the selection is, once. */
export class CellAnnouncer {
  #say: (text: string) => void;
  /** The cell, what it shows and what is in it, as they were last read. */
  #last = "";

  constructor(say: (text: string) => void) {
    this.#say = say;
  }

  show(
    sheet: number,
    selection: Selection,
    inspection: CellInspection | null,
    rowNumber: RowNumber,
  ): void {
    const text = selectionText(selection, inspection, rowNumber);
    if (text === null) {
      return;
    }
    const { rowId, colId } = selection;
    const raw = isSingleCell(selection) ? inspection?.raw : null;
    const key = JSON.stringify([sheet, rowId, colId, text, raw]);
    if (key !== this.#last) {
      this.#last = key;
      this.#say(text);
    }
  }

  /** Has the same be read once more. */
  forget(): void {
    this.#last = "";
  }
}

/** Says what was entered into a cell that the selection has left since. */
export function announceCommit(cell: string, text: string): void {
  announce(
    text === ""
      ? t("Cleared {cell}", { cell })
      : t("Entered {text} in {cell}", { text: shortened(text), cell }),
  );
}

/** Says that what was selected has been cleared; of a single cell, it is read that it is blank. */
export function announceCleared(selection: Selection | null, rowNumber: RowNumber): void {
  if (selection && !isSingleCell(selection)) {
    const { firstRow, endRow, firstCol, endCol } = selection.range;
    announce(
      t("Cleared {first} to {last}", {
        first: `${columnName(firstCol)}${rowNumber(firstRow)}`,
        last: `${columnName(endCol - 1)}${rowNumber(endRow - 1)}`,
      }),
    );
  }
}

/** Whoever is in a drawer beside the grid hears what the drawer says. */
const isInDrawer = () => document.activeElement?.closest("aside") != null;

/** Dialogs say themselves what comes of them, and what is behind them does not matter then. */
const isElsewhere = () => isInDrawer() || document.querySelector("dialog:modal") !== null;

export interface CellAnnouncerOptions {
  /** The grid's element. */
  readonly host: RefObject<HTMLElement | null>;
  readonly documentId: string;
  readonly sheet: SheetInfo | null;
  readonly selection: Selection | null;
  /** What the engine has said about the selected cell, if it has. */
  readonly inspection: CellInspection | null;
  readonly activity: Activity | null;
  readonly rowNumber: RowNumber;
}

interface Known {
  readonly documentId: string;
  readonly sheet: SheetState;
  readonly activity: Activity | null;
}

/** Has the selection read out, and what comes of actions. */
export function useCellAnnouncer(options: CellAnnouncerOptions): void {
  const { host, documentId, sheet, selection, inspection, activity } = options;
  const latest = useRef(options);
  latest.current = options;
  const known = useRef<Known | null>(null);
  const announcer = useRef<CellAnnouncer | null>(null);
  announcer.current ??= new CellAnnouncer((text) => announce(text, { isPlace: true }));

  useEffect(() => {
    const before = known.current;
    known.current = sheet && { documentId, sheet, activity };
    if (!sheet || before?.documentId !== documentId || isInDrawer()) {
      return;
    }
    const place =
      sheet.sheet === before.sheet.sheet
        ? filterText(before.sheet, sheet)
        : t("Worksheet {name}", { name: sheet.sheetName });
    const outcome =
      activity && activity !== before.activity ? outcomeText(activity, before.sheet, sheet) : null;
    announce(
      outcome !== null && place !== null
        ? t("{first}. {second}", { first: outcome, second: place })
        : (outcome ?? place ?? ""),
    );
  }, [documentId, sheet, activity]);

  useEffect(() => {
    if (sheet && selection && !isElsewhere()) {
      announcer.current?.show(sheet.sheet, selection, inspection, latest.current.rowNumber);
    }
  }, [sheet, selection, inspection]);

  useEffect(() => {
    const element = host.current;
    let inside = element?.contains(document.activeElement) ?? false;
    const onFocusOut = ({ relatedTarget }: FocusEvent) => {
      inside &&= relatedTarget instanceof Node && element?.contains(relatedTarget) === true;
    };
    const onFocusIn = () => {
      if (inside) {
        return;
      }
      inside = true;
      // Whoever comes back to the grid wants to know where that is.
      announcer.current?.forget();
      const { sheet, selection, inspection, rowNumber } = latest.current;
      if (sheet && selection) {
        announcer.current?.show(sheet.sheet, selection, inspection, rowNumber);
      }
    };
    element?.addEventListener("focusin", onFocusIn);
    element?.addEventListener("focusout", onFocusOut);
    return () => {
      element?.removeEventListener("focusin", onFocusIn);
      element?.removeEventListener("focusout", onFocusOut);
    };
  }, [host]);
}

/** The same as a part of a page, which is loaded when there is nothing more urgent to load. */
export function CellAnnouncements(options: CellAnnouncerOptions): null {
  useCellAnnouncer(options);
  return null;
}
