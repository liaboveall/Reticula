import { type Span, ZOOM_STEPS } from "@reticula/grid";
import type {
  FillDirection,
  SheetTabInfo,
  WireBorderWeight,
  WireFormatPatch,
  WireHorizontalAlign,
  WireStylePatch,
} from "@reticula/protocol";
import { freezeChoices, numberFormats } from "../editor/formats.ts";
import { freezeChoiceOf } from "../editor/freeze.ts";
import type { CellIds } from "../editor/held-commits.ts";
import type { FreezeChoice } from "../editor/types.ts";
import type { EngineMessage } from "../engine/client.ts";
import { fmtInt, getLocale, type Locale, setLocale, t } from "../i18n/index.ts";
import { LANGUAGES } from "../i18n/languages.ts";
import { rendererPreference, setRendererPreference } from "../renderer.ts";
import { announce } from "../ui/announcer.ts";
import type { EditorHandle, EditorState } from "./handle.ts";
import { cycleRegions } from "./regions.ts";
import { TITLES } from "./titles.ts";

/** The headings the commands are listed under. */
export const GROUPS = {
  edit: () => t("Edit"),
  select: () => t("Select"),
  format: () => t("Format"),
  cells: () => t("Rows and columns"),
  data: () => t("Data"),
  sheets: () => t("Worksheets"),
  view: () => t("View"),
  file: () => t("File"),
  help: () => t("Help"),
} as const;

/** Whether a command does anything, for a worksheet if it is about one of them. */
type Test = (state: EditorState, sheet?: number) => boolean;

export interface Command {
  readonly id: string;
  readonly group: keyof typeof GROUPS;
  readonly title: () => string;
  /** English words it is found by besides its title. */
  readonly words?: string;
  readonly enabled?: Test;
  /** Whether it is what is chosen at the moment, of several that exclude each other. */
  readonly checked?: (handle: EditorHandle) => boolean;
  /** Whether it has nothing to do with what is being typed into a cell. */
  readonly whileEditing?: boolean;
  /** Returns `false`, or a promise of it, if it came to nothing. The sheet is the one in view, unless it is told. */
  readonly run: (handle: EditorHandle, sheet?: number) => unknown;
}

const isOpen: Test = ({ sheet }) => sheet !== null;
const mayEdit: Test = ({ sheet, readOnly }) => sheet !== null && !readOnly;
const hasCells: Test = (state) => mayEdit(state) && state.selection !== null;
const hasRows: Test = (state) => mayEdit(state) && state.selection?.rowId != null;
const hasCols: Test = (state) => mayEdit(state) && state.selection?.colId != null;

const spans = (least: { rows?: number; cols?: number }): Test => {
  const { rows = 1, cols = 1 } = least;
  return (state) => {
    const range = state.selection?.range;
    return (
      mayEdit(state) &&
      range !== undefined &&
      range.endRow - range.firstRow >= rows &&
      range.endCol - range.firstCol >= cols
    );
  };
};

// --- Editing ----------------------------------------------------------------

function history(kind: "undo" | "redo") {
  return (handle: EditorHandle) =>
    handle.mutate({ kind }, { action: kind === "undo" ? "undidAction" : "redidAction" });
}

function copy(cut: boolean) {
  return (handle: EditorHandle) => {
    const range = handle.grid()?.selectionRange;
    return (
      range != null &&
      handle.send({
        kind: "copyRange",
        sheet: handle.sheetId(),
        firstRow: range.firstRow,
        rowCount: range.endRow - range.firstRow,
        firstCol: range.firstCol,
        colCount: range.endCol - range.firstCol,
        cut,
      })
    );
  };
}

async function paste(handle: EditorHandle): Promise<void> {
  const range = handle.grid()?.selectionRange;
  if (!range) {
    return;
  }
  const sheet = handle.sheetId();
  let tsv = "";
  let html: string | undefined;
  try {
    tsv = (await navigator.clipboard?.readText?.()) ?? "";
  } catch {
    // The browser does not let the page read it: what was copied here is known.
  }
  // The browser may have asked for a permission meanwhile.
  if (handle.sheetId() !== sheet) {
    return;
  }
  const copied = handle.copied();
  if (tsv === "" && copied) {
    ({ tsv, html } = copied);
  }
  if (tsv !== "" || html) {
    handle.mutate(
      {
        kind: "pasteClipboard",
        sheet,
        destRow: range.firstRow,
        destRowCount: range.endRow - range.firstRow,
        destCol: range.firstCol,
        destColCount: range.endCol - range.firstCol,
        tsv,
        html,
      },
      { computes: true },
    );
  }
}

function clearContents(handle: EditorHandle): boolean {
  const runs = handle.grid()?.selectedCellRuns();
  return (
    runs != null &&
    handle.mutate(
      { kind: "clearCells", sheet: handle.sheetId(), ...runs },
      { action: "clearedCells" },
    )
  );
}

/** Copies the first row or column of the selection over the rest, or the one before it. */
function copyFill(dir: FillDirection) {
  return (handle: EditorHandle) => {
    const range = handle.grid()?.selectionRange;
    if (!range) {
      return false;
    }
    let { firstRow, firstCol } = range;
    if (dir === "down" && range.endRow - firstRow === 1) {
      firstRow--;
    } else if (dir === "right" && range.endCol - firstCol === 1) {
      firstCol--;
    }
    return (
      firstRow >= 0 &&
      firstCol >= 0 &&
      handle.mutate(
        {
          kind: "copyFill",
          sheet: handle.sheetId(),
          firstRow,
          rowCount: range.endRow - firstRow,
          firstCol,
          colCount: range.endCol - firstCol,
          dir,
        },
        { action: "filledRange", computes: true },
      )
    );
  };
}

/** How long to wait for the engine to answer. */
const PATIENCE_MS = 1_000;

/**
 * Where the cells in use end on a sheet, once the engine has done all it has
 * been asked for before. `null` if that takes too long. An answer that comes
 * while there are others to come first is one to somebody else.
 */
async function usedEnd(
  handle: EditorHandle,
  sheet: number,
  isNext: (message: EngineMessage) => boolean = () => true,
): Promise<{ readonly row: number; readonly col: number } | null> {
  let stop = () => {};
  const end = await new Promise<{ row: number; col: number } | null>((resolve) => {
    stop = handle.intercept((message) => {
      if (!isNext(message) || message.kind !== "edgeJumped" || message.sheet !== sheet) {
        return false;
      }
      resolve(message);
      return true;
    });
    setTimeout(resolve, PATIENCE_MS, null);
    handle.send({ kind: "jumpUsedEnd", sheet, select: false });
  });
  stop();
  return end;
}

/**
 * Continues what is at one side of the selection over the rest of it. Which
 * part that is, is seen from what is empty: the rows or columns from that side
 * on with anything in them. The engine is asked: what has been entered a moment
 * ago may not be shown yet.
 */
function fillSeries(direction: "down" | "up" | "right" | "left") {
  return async (handle: EditorHandle): Promise<boolean> => {
    const grid = handle.grid();
    const dst = grid?.selectionRange;
    if (!grid || !dst) {
      return false;
    }
    const sheet = handle.sheetId();
    const { rows, cols } = grid.sheetSize;
    const isVertical = direction === "down" || direction === "up";
    const length = isVertical ? dst.endRow - dst.firstRow : dst.endCol - dst.firstCol;
    const across = isVertical ? dst.endCol - dst.firstCol : dst.endRow - dst.firstRow;
    const isForward = direction === "down" || direction === "right";
    const cellsOf = (line: number): CellIds[] => {
      const at = isForward ? line : length - 1 - line;
      const cells: CellIds[] = [];
      for (let cross = 0; cross < across; cross++) {
        const rowId = grid.rowIdAt(dst.firstRow + (isVertical ? at : cross));
        const colId = grid.colIdAt(dst.firstCol + (isVertical ? cross : at));
        // What has not been loaded is out of view, and not known to be anything.
        if (rowId === null || colId === null) {
          break;
        }
        cells.push({ rowId, colId });
      }
      return cells;
    };
    const keyOf = ({ rowId, colId }: CellIds) => `${rowId} ${colId}`;
    const asked = new Set<string>();
    const inUse = new Set<string>();
    // The first one is taken to be in use.
    const lines: CellIds[][] = [[]];
    let filled = 1;
    for (let next = 8; filled === lines.length && filled < length; next *= 2) {
      for (const end = Math.min(length, filled + next); lines.length < end; ) {
        const cells = cellsOf(lines.length);
        lines.push(cells);
        for (const cell of cells) {
          asked.add(keyOf(cell));
          handle.send({ kind: "inspectCell", sheet, ...cell });
        }
      }
      const isAnswered = await usedEnd(handle, sheet, (message) => {
        if (message.kind === "cellInspected" && message.sheet === sheet) {
          // Of several answers, the last one is the one that has been asked for here.
          const key = keyOf(message);
          asked.delete(key);
          inUse.delete(key);
          if (message.raw !== "" || message.display !== "") {
            inUse.add(key);
          }
        }
        return asked.size === 0;
      });
      const size = grid.sheetSize;
      // Where rows or columns have come or gone meanwhile, others may be in their places.
      if (!isAnswered || handle.sheetId() !== sheet || size.rows !== rows || size.cols !== cols) {
        return false;
      }
      while (lines[filled]?.some((cell) => inUse.has(keyOf(cell)))) {
        filled++;
      }
    }
    const size = filled === length ? 1 : filled;
    const src: Span = {
      firstRow: direction === "up" ? dst.endRow - size : dst.firstRow,
      endRow: direction === "down" ? dst.firstRow + size : dst.endRow,
      firstCol: direction === "left" ? dst.endCol - size : dst.firstCol,
      endCol: direction === "right" ? dst.firstCol + size : dst.endCol,
    };
    return handle.fill(src, dst);
  };
}

// --- Formats ----------------------------------------------------------------

function format(
  handle: EditorHandle,
  patch: { readonly numberFormat?: WireFormatPatch; readonly style?: WireStylePatch },
): boolean {
  const target = handle.grid()?.selectedFormatTarget();
  return (
    target != null &&
    handle.mutate(
      { kind: "setFormat", sheet: handle.sheetId(), target, ...patch },
      { action: "formattedCells" },
    )
  );
}

function toggle(id: "bold" | "italic" | "strikethrough"): Command {
  return {
    id,
    group: "format",
    title: TITLES[id],
    enabled: hasCells,
    run: (handle) => format(handle, { style: { [id]: handle.state().style?.[id] !== true } }),
  };
}

function align(id: "alignLeft" | "alignCenter" | "alignRight", to: WireHorizontalAlign): Command {
  return {
    id,
    group: "format",
    title: TITLES[id],
    enabled: hasCells,
    run: (handle) =>
      format(handle, { style: { align: handle.state().style?.align === to ? "auto" : to } }),
  };
}

function borders(
  id: "borderThin" | "borderMedium" | "borderNone",
  weight: WireBorderWeight,
): Command {
  return {
    id,
    group: "format",
    title: TITLES[id],
    enabled: hasCells,
    run: (handle) =>
      format(handle, {
        style: {
          borderTop: weight,
          borderRight: weight,
          borderBottom: weight,
          borderLeft: weight,
        },
      }),
  };
}

/** The colors there are to choose from, by their place in the palettes. */
export function colorNames(): readonly string[] {
  return [
    t("Gray"),
    t("Red"),
    t("Orange"),
    t("Green"),
    t("Blue"),
    t("Purple"),
    t("Pink"),
    t("Teal"),
  ];
}

const COLOR_SLOTS = [0, 1, 2, 3, 4, 5, 6, 7];

export type ColorTarget = "textColor" | "fillColor";

const COLOR_TITLES: Readonly<Record<ColorTarget, () => string>> = {
  textColor: () => t("Text color"),
  fillColor: () => t("Fill color"),
};

function colors(target: ColorTarget, none: () => string): Command[] {
  return [
    {
      id: `${target}:none`,
      group: "format",
      title: none,
      words: "color colour",
      enabled: hasCells,
      run: (handle) => format(handle, { style: { [target]: { kind: "clear" } } }),
    },
    ...COLOR_SLOTS.map(
      (slot): Command => ({
        id: `${target}:${slot}`,
        group: "format",
        title: () => `${COLOR_TITLES[target]()}: ${colorNames()[slot]}`,
        words: "colour highlight background",
        enabled: hasCells,
        run: (handle) =>
          format(handle, {
            style: { [target]: { kind: "set", color: { kind: "palette", slot } } },
          }),
      }),
    ),
  ];
}

// --- Rows, columns and panes ------------------------------------------------

function insert(axis: "row" | "col", after: boolean) {
  return (handle: EditorHandle) => {
    const grid = handle.grid();
    const range = grid?.selectionRange;
    if (!grid) {
      return false;
    }
    const sheet = handle.sheetId();
    // Without a selection, before everything.
    const at = axis === "row" ? range?.firstRow : range?.firstCol;
    const end = axis === "row" ? range?.endRow : range?.endCol;
    const position = (after ? (end ?? 0) : (at ?? 0)) - 1;
    const idAt = axis === "row" ? grid.rowIdAt(position) : grid.colIdAt(position);
    const id = position < 0 ? null : idAt;
    if (position >= 0 && id === null) {
      return false;
    }
    return axis === "row"
      ? handle.mutate(
          { kind: "insertRows", sheet, afterRowId: id, count: 1 },
          { action: "insertedRows" },
        )
      : handle.mutate(
          { kind: "insertCols", sheet, afterColId: id, count: 1 },
          { action: "insertedColumns" },
        );
  };
}

function deleteRows(handle: EditorHandle): boolean {
  const rows = handle.grid()?.selectedRowRuns();
  return (
    rows != null &&
    rows.length > 0 &&
    handle.mutate({ kind: "deleteRows", sheet: handle.sheetId(), rows }, { action: "deletedRows" })
  );
}

function deleteCols(handle: EditorHandle): boolean {
  const cols = handle.grid()?.selectedColRuns();
  return (
    cols != null &&
    cols.length > 0 &&
    handle.mutate(
      { kind: "deleteCols", sheet: handle.sheetId(), cols },
      { action: "deletedColumns" },
    )
  );
}

/** The most rows and columns that can be frozen. */
const MOST_FROZEN = 32;

function freeze(choice: FreezeChoice) {
  return (handle: EditorHandle) => {
    const active = handle.grid()?.selection;
    let frozenRows = choice === "row1" || choice === "row1col1" ? 1 : 0;
    let frozenCols = choice === "col1" || choice === "row1col1" ? 1 : 0;
    if (choice === "active" && active) {
      frozenRows = Math.min(MOST_FROZEN, active.row);
      frozenCols = Math.min(MOST_FROZEN, active.col);
      if (frozenRows === 0 && frozenCols === 0) {
        frozenRows = 1;
      }
    }
    return handle.mutate(
      { kind: "setFreeze", sheet: handle.sheetId(), frozenRows, frozenCols },
      { action: "updatedFreezePanes" },
    );
  };
}

// --- Data -------------------------------------------------------------------

/** Rows a table is taken to have whose end is not selected. */
const FILTERED_ROWS = 10_000;
const FILTERED_COLS = 26;

function toggleFilter(handle: EditorHandle): boolean {
  const { sheet } = handle.state();
  if (!sheet) {
    return false;
  }
  if (sheet.filter) {
    return handle.send({ kind: "clearFilter", sheet: sheet.sheet });
  }
  const range = handle.grid()?.selectionRange;
  const headerRow = range?.firstRow ?? 0;
  const firstCol = range?.firstCol ?? 0;
  const lastRow =
    range && range.endRow - range.firstRow > 1
      ? range.endRow - 1
      : Math.min(sheet.rows - 1, headerRow + FILTERED_ROWS);
  const lastCol =
    range && range.endCol - range.firstCol > 1
      ? range.endCol - 1
      : Math.min(sheet.cols - 1, Math.max(firstCol, FILTERED_COLS - 1));
  return handle.send({
    kind: "setFilterTable",
    sheet: sheet.sheet,
    headerRow,
    lastRow,
    firstCol,
    lastCol,
  });
}

const inFilter: Test = ({ sheet, selection }) =>
  sheet?.filter !== undefined &&
  selection !== null &&
  selection.col >= sheet.filter.firstCol &&
  selection.col <= sheet.filter.lastCol;

function clearFilters(handle: EditorHandle): void {
  const { sheet } = handle.state();
  for (const col of sheet?.filter?.activeCols ?? []) {
    handle.send({ kind: "setColumnFilter", sheet: handle.sheetId(), col, criterion: undefined });
  }
}

// --- Worksheets -------------------------------------------------------------

function tabAt(
  { sheet }: EditorState,
  id: number | undefined,
  offset = 0,
): SheetTabInfo | undefined {
  const at = sheet?.sheets.findIndex((tab) => tab.id === (id ?? sheet.sheet)) ?? -1;
  return at < 0 ? undefined : sheet?.sheets[at + offset];
}

const hasTab =
  (offset: number): Test =>
  (state, sheet) =>
    mayEdit(state) && tabAt(state, sheet, offset) !== undefined;

/** From the one that has been asked for last, which may not be in view yet. */
const goToTab = (offset: number) => (handle: EditorHandle) => {
  const tab = tabAt(handle.state(), handle.askedSheetId(), offset);
  return tab !== undefined && handle.switchSheet(tab.id) === undefined;
};

function moveSheet(direction: -1 | 1) {
  return (handle: EditorHandle, sheet?: number) => {
    const state = handle.state();
    const tab = tabAt(state, sheet);
    const after = tabAt(state, sheet, direction === 1 ? 1 : -2);
    return (
      tab !== undefined &&
      handle.mutate(
        { kind: "moveSheet", sheet: tab.id, afterSheet: after?.id },
        { action: "reorderedSheets" },
      )
    );
  };
}

/** Whether a sheet may have anything on it, as far as that can be told. */
async function mayHaveContent(handle: EditorHandle, sheet: number): Promise<boolean> {
  const inView = sheet === handle.sheetId();
  if (inView && handle.grid()?.textAt(0, 0) !== "") {
    return true;
  }
  const end = await usedEnd(handle, sheet);
  // It is the first cell if there is none, and also if that is the only one.
  return end === null || !inView || end.row > 0 || end.col > 0;
}

async function deleteSheet(handle: EditorHandle, sheet?: number): Promise<void> {
  const tab = tabAt(handle.state(), sheet);
  if (!tab) {
    return;
  }
  if (await mayHaveContent(handle, tab.id)) {
    handle.openDialog({ kind: "deleteSheet", tab });
  } else {
    handle.mutate({ kind: "deleteSheet", sheet: tab.id }, { action: "deletedSheet" });
  }
}

// --- The list ---------------------------------------------------------------

const ZOOM_WORDS = "scale size";

function zoomTo(zoom: number): Command {
  return {
    id: `zoom:${Math.round(zoom * 100)}`,
    group: "view",
    title: () => t("Zoom: {percent}", { percent: `${fmtInt(Math.round(zoom * 100))}%` }),
    words: ZOOM_WORDS,
    enabled: isOpen,
    checked: (handle) => handle.grid()?.zoom === zoom,
    whileEditing: true,
    run: (handle) => handle.grid()?.setZoom(zoom),
  };
}

function renderer(id: string, preference: "auto" | "canvas2d", title: () => string): Command {
  return {
    id,
    group: "view",
    title,
    words: "webgpu canvas graphics drawing",
    checked: () => (rendererPreference() === "canvas2d") === (preference === "canvas2d"),
    whileEditing: true,
    run: (handle) => setRendererPreference(handle.grid(), preference),
  };
}

function language([locale, name]: readonly [Locale, string]): Command {
  return {
    id: `language:${locale}`,
    group: "view",
    title: () => name,
    words: `language ${locale === "en" ? "english" : "chinese"}`,
    checked: () => getLocale() === locale,
    whileEditing: true,
    run: () => void setLocale(locale),
  };
}

/** Every command of the editor, under its heading. */
export const COMMANDS: readonly Command[] = [
  {
    id: "undo",
    group: "edit",
    title: TITLES.undo,
    enabled: (state) => mayEdit(state) && state.sheet?.canUndo === true,
    run: history("undo"),
  },
  {
    id: "redo",
    group: "edit",
    title: TITLES.redo,
    enabled: (state) => mayEdit(state) && state.sheet?.canRedo === true,
    run: history("redo"),
  },
  { id: "cut", group: "edit", title: () => t("Cut"), enabled: hasCells, run: copy(true) },
  { id: "copy", group: "edit", title: () => t("Copy"), enabled: isOpen, run: copy(false) },
  {
    id: "paste",
    group: "edit",
    title: () => t("Paste"),
    words: "clipboard",
    enabled: hasCells,
    run: (handle) => void paste(handle),
  },
  {
    id: "clearContents",
    group: "edit",
    title: () => t("Clear Contents"),
    words: "delete empty",
    enabled: hasCells,
    run: clearContents,
  },
  {
    id: "fillDown",
    group: "edit",
    title: () => t("Fill Down"),
    words: "copy",
    enabled: hasCells,
    run: copyFill("down"),
  },
  {
    id: "fillRight",
    group: "edit",
    title: () => t("Fill Right"),
    words: "copy",
    enabled: hasCells,
    run: copyFill("right"),
  },
  {
    id: "fillSeriesDown",
    group: "edit",
    title: () => t("Fill Series Down"),
    words: "continue extend",
    enabled: spans({ rows: 2 }),
    run: fillSeries("down"),
  },
  {
    id: "fillSeriesUp",
    group: "edit",
    title: () => t("Fill Series Up"),
    words: "continue extend",
    enabled: spans({ rows: 2 }),
    run: fillSeries("up"),
  },
  {
    id: "fillSeriesRight",
    group: "edit",
    title: () => t("Fill Series Right"),
    words: "continue extend",
    enabled: spans({ cols: 2 }),
    run: fillSeries("right"),
  },
  {
    id: "fillSeriesLeft",
    group: "edit",
    title: () => t("Fill Series Left"),
    words: "continue extend",
    enabled: spans({ cols: 2 }),
    run: fillSeries("left"),
  },
  {
    id: "autoFillDown",
    group: "edit",
    title: () => t("Fill Down to the End of the Table"),
    words: "auto column",
    enabled: hasCells,
    run: (handle) => {
      const src = handle.grid()?.selectionRange;
      return src != null && handle.autoFillDown(src);
    },
  },
  {
    id: "find",
    group: "edit",
    title: () => t("Find"),
    words: "search",
    enabled: isOpen,
    run: (handle) => handle.openTool({ kind: "find", replace: false }),
  },
  {
    id: "replace",
    group: "edit",
    title: () => t("Find & Replace"),
    words: "search substitute",
    enabled: isOpen,
    run: (handle) => handle.openTool({ kind: "find", replace: true }),
  },
  {
    id: "goTo",
    group: "select",
    title: () => t("Go to a Cell, Range or Name"),
    words: "jump address reference select",
    enabled: isOpen,
    run: (handle) => handle.editNameBox(),
  },
  {
    id: "selectRows",
    group: "select",
    title: () => t("Select Whole Rows"),
    enabled: isOpen,
    run: (handle) => {
      const grid = handle.grid();
      const range = grid?.selectionRange;
      if (grid && range) {
        grid.selectRows(range.firstRow, range.endRow - 1);
      }
    },
  },
  {
    id: "selectCols",
    group: "select",
    title: () => t("Select Whole Columns"),
    enabled: isOpen,
    run: (handle) => {
      const grid = handle.grid();
      const range = grid?.selectionRange;
      if (grid && range) {
        grid.selectCols(range.firstCol, range.endCol - 1);
      }
    },
  },
  {
    id: "selectAll",
    group: "select",
    title: () => t("Select All Cells"),
    enabled: isOpen,
    run: (handle) => handle.grid()?.selectAll(),
  },
  toggle("bold"),
  toggle("italic"),
  toggle("strikethrough"),
  align("alignLeft", "left"),
  align("alignCenter", "center"),
  align("alignRight", "right"),
  ...numberFormatCommands(),
  ...colors("textColor", () => t("Default text color")),
  ...colors("fillColor", () => t("No fill")),
  borders("borderThin", "thin"),
  borders("borderMedium", "medium"),
  borders("borderNone", "none"),
  {
    id: "insertRowAbove",
    group: "cells",
    title: TITLES.insertRowAbove,
    words: "add new",
    enabled: mayEdit,
    run: insert("row", false),
  },
  {
    id: "insertRowBelow",
    group: "cells",
    title: () => t("Insert Row Below"),
    words: "add new",
    enabled: mayEdit,
    run: insert("row", true),
  },
  {
    id: "deleteRows",
    group: "cells",
    title: TITLES.deleteRows,
    words: "remove",
    enabled: hasRows,
    run: deleteRows,
  },
  {
    id: "insertColLeft",
    group: "cells",
    title: TITLES.insertColLeft,
    words: "add new",
    enabled: mayEdit,
    run: insert("col", false),
  },
  {
    id: "insertColRight",
    group: "cells",
    title: () => t("Insert Column Right"),
    words: "add new",
    enabled: mayEdit,
    run: insert("col", true),
  },
  {
    id: "deleteCols",
    group: "cells",
    title: TITLES.deleteCols,
    words: "remove",
    enabled: hasCols,
    run: deleteCols,
  },
  {
    id: "moveRowsUp",
    group: "cells",
    title: () => t("Move Rows Up"),
    enabled: hasRows,
    run: (handle) => handle.grid()?.moveSelectedRows(-1),
  },
  {
    id: "moveRowsDown",
    group: "cells",
    title: () => t("Move Rows Down"),
    enabled: hasRows,
    run: (handle) => handle.grid()?.moveSelectedRows(1),
  },
  {
    id: "moveColsLeft",
    group: "cells",
    title: () => t("Move Columns Left"),
    enabled: hasCols,
    run: (handle) => handle.grid()?.moveSelectedCols(-1),
  },
  {
    id: "moveColsRight",
    group: "cells",
    title: () => t("Move Columns Right"),
    enabled: hasCols,
    run: (handle) => handle.grid()?.moveSelectedCols(1),
  },
  {
    id: "rowHeight",
    group: "cells",
    title: () => t("Row Height…"),
    words: "size resize",
    enabled: hasRows,
    run: (handle) => handle.openDialog({ kind: "size", axis: "row" }),
  },
  {
    id: "colWidth",
    group: "cells",
    title: () => t("Column Width…"),
    words: "size resize",
    enabled: hasCols,
    run: (handle) => handle.openDialog({ kind: "size", axis: "col" }),
  },
  {
    id: "autofitCols",
    group: "cells",
    title: () => t("Fit Column Width to Text"),
    words: "auto size resize autofit",
    enabled: hasCols,
    run: (handle) => handle.grid()?.autofitSelectedCols(),
  },
  ...freezeCommands(),
  {
    id: "sort",
    group: "data",
    title: () => t("Sort Range"),
    words: "order ascending descending",
    enabled: mayEdit,
    run: (handle) => handle.openTool({ kind: "sort" }),
  },
  {
    id: "filter",
    group: "data",
    title: () => t("Turn AutoFilter On or Off"),
    enabled: isOpen,
    run: toggleFilter,
  },
  {
    id: "filterColumn",
    group: "data",
    title: () => t("Filter This Column…"),
    enabled: inFilter,
    run: (handle) => {
      const col = handle.grid()?.selection?.col;
      return col !== undefined && handle.openTool({ kind: "filter", col }) === undefined;
    },
  },
  {
    id: "clearFilters",
    group: "data",
    title: () => t("Clear Filters"),
    words: "show all",
    enabled: ({ sheet }) => (sheet?.filter?.activeCols.length ?? 0) > 0,
    run: clearFilters,
  },
  {
    id: "names",
    group: "data",
    title: () => t("Name Manager"),
    words: "named ranges define",
    enabled: isOpen,
    run: (handle) => handle.openTool({ kind: "names" }),
  },
  {
    id: "resumeRecalc",
    group: "data",
    title: () => t("Resume Recalculation"),
    words: "calculate formulas paused",
    enabled: ({ sheet }) => sheet?.recalcFrozen === true,
    run: (handle) => handle.send({ kind: "resumeRecalc" }),
  },
  {
    id: "addSheet",
    group: "sheets",
    title: () => t("Add worksheet"),
    words: "insert new tab",
    enabled: mayEdit,
    run: (handle) => {
      handle.cellEditor()?.focus();
      return handle.mutate(
        { kind: "addSheet", afterSheet: handle.sheetId() },
        { action: "addedSheet" },
      );
    },
  },
  {
    id: "renameSheet",
    group: "sheets",
    title: () => t("Rename Sheet…"),
    words: "worksheet tab title",
    enabled: hasTab(0),
    run: (handle, sheet) => {
      const tab = tabAt(handle.state(), sheet);
      if (tab) {
        handle.renameSheet(tab);
      }
    },
  },
  {
    id: "moveSheetLeft",
    group: "sheets",
    title: () => t("Move Sheet Left"),
    words: "worksheet tab order",
    enabled: hasTab(-1),
    run: moveSheet(-1),
  },
  {
    id: "moveSheetRight",
    group: "sheets",
    title: () => t("Move Sheet Right"),
    words: "worksheet tab order",
    enabled: hasTab(1),
    run: moveSheet(1),
  },
  {
    id: "deleteSheet",
    group: "sheets",
    title: () => t("Delete Sheet"),
    words: "worksheet tab remove",
    enabled: (state) => mayEdit(state) && (state.sheet?.sheets.length ?? 0) > 1,
    run: (handle, sheet) => void deleteSheet(handle, sheet),
  },
  {
    id: "nextSheet",
    group: "sheets",
    title: () => t("Next Sheet"),
    words: "worksheet tab switch",
    enabled: isOpen,
    run: goToTab(1),
  },
  {
    id: "previousSheet",
    group: "sheets",
    title: () => t("Previous Sheet"),
    words: "worksheet tab switch",
    enabled: isOpen,
    run: goToTab(-1),
  },
  {
    id: "zoomIn",
    group: "view",
    title: () => t("Zoom in"),
    words: ZOOM_WORDS,
    enabled: isOpen,
    whileEditing: true,
    run: (handle) => handle.grid()?.zoomIn(),
  },
  {
    id: "zoomOut",
    group: "view",
    title: () => t("Zoom out"),
    words: ZOOM_WORDS,
    enabled: isOpen,
    whileEditing: true,
    run: (handle) => handle.grid()?.zoomOut(),
  },
  ...ZOOM_STEPS.map(zoomTo),
  renderer("rendererAuto", "auto", () => t("Draw with the Graphics Card")),
  renderer("rendererCanvas", "canvas2d", () => t("Draw with Canvas 2D")),
  ...LANGUAGES.map(language),
  {
    id: "contextMenu",
    group: "view",
    title: () => t("Open the Menu of the Selection"),
    words: "context right click",
    enabled: isOpen,
    run: (handle) => handle.openMenu({ kind: "selection" }),
  },
  {
    id: "nextRegion",
    group: "view",
    title: () => t("Go to the Next Part of the Window"),
    words: "focus pane region",
    whileEditing: true,
    run: () => cycleRegions(1),
  },
  {
    id: "previousRegion",
    group: "view",
    title: () => t("Go to the Previous Part of the Window"),
    words: "focus pane region",
    whileEditing: true,
    run: () => cycleRegions(-1),
  },
  {
    id: "workbooks",
    group: "file",
    title: () => t("Manage Workbooks"),
    words: "open local files",
    run: (handle) => handle.openWorkbooks(),
  },
  {
    id: "newWorkbook",
    group: "file",
    title: () => t("New local workbook"),
    words: "create blank empty file",
    run: (handle) => handle.newWorkbook(),
  },
  {
    id: "import",
    group: "file",
    title: () => t("Import .rtcl / .csv / .xlsx"),
    words: "open upload file excel",
    run: (handle) => handle.openWorkbooks(true),
  },
  {
    id: "exportRtcl",
    group: "file",
    title: () => t("Export .rtcl"),
    words: "save download backup file",
    enabled: isOpen,
    run: (handle) => handle.sendHost({ kind: "exportRtcl" }),
  },
  {
    id: "exportCsv",
    group: "file",
    title: () => t("Export .csv"),
    words: "save download file",
    enabled: isOpen,
    run: (handle) => handle.sendHost({ kind: "exportCsv", sheet: handle.sheetId() }),
  },
  {
    id: "exportXlsx",
    group: "file",
    title: () => t("Export .xlsx"),
    words: "save download file excel",
    enabled: isOpen,
    run: (handle) => handle.sendHost({ kind: "exportXlsxSnapshot" }),
  },
  {
    id: "cloud",
    group: "file",
    title: () => t("Cloud Collaboration"),
    words: "share sign in account members link",
    run: (handle) => handle.openCloud(),
  },
  {
    id: "history",
    group: "file",
    title: () => t("Version History"),
    words: "revisions versions restore",
    enabled: ({ isCloud }) => isCloud,
    run: (handle) => handle.toggleHistory(),
  },
  {
    id: "ai",
    group: "data",
    title: () => t("AI Analyst"),
    words: "assistant draft demo",
    run: (handle) => handle.toggleAi(),
  },
  {
    id: "palette",
    group: "help",
    title: () => t("Commands"),
    words: "palette search",
    whileEditing: true,
    run: (handle) => handle.openDialog({ kind: "palette" }),
  },
  {
    id: "keyboardHelp",
    group: "help",
    title: () => t("Keyboard Shortcuts"),
    words: "keys help",
    whileEditing: true,
    run: (handle) => handle.openDialog({ kind: "help" }),
  },
];

function numberFormatCommands(): Command[] {
  return numberFormats().map(({ value: code }) => ({
    id: `numberFormat:${code}`,
    group: "format",
    title: () =>
      `${t("Number format")}: ${numberFormats().find(({ value }) => value === code)?.text}`,
    words: "number currency money date time percent",
    enabled: hasCells,
    run: (handle) =>
      format(handle, {
        numberFormat: code === "General" ? { kind: "clear" } : { kind: "set", code },
      }),
  }));
}

function freezeCommands(): Command[] {
  return freezeChoices().map(({ value: choice }) => ({
    id: `freeze:${choice}`,
    group: "view",
    title: () => freezeChoices().find(({ value }) => value === choice)?.text ?? "",
    words: "freeze panes lock",
    enabled: mayEdit,
    checked: (handle) => freezeChoiceOf(handle.state().sheet) === choice,
    run: freeze(choice),
  }));
}

const byId = new Map(COMMANDS.map((command) => [command.id, command]));

export function commandOf(id: string): Command | undefined {
  return byId.get(id);
}

export function isEnabled(command: Command, state: EditorState, sheet?: number): boolean {
  return command.enabled?.(state, sheet) !== false;
}

/** Runs a command, and says so if that comes to nothing. */
export function run(handle: EditorHandle, id: string, sheet?: number): void {
  const command = byId.get(id);
  if (!command) {
    return;
  }
  if (!command.whileEditing) {
    handle.commitPending();
  }
  const say = (outcome: unknown) => {
    if (outcome === false) {
      announce(t("Not possible here: {command}", { command: command.title() }));
    }
  };
  const outcome = isEnabled(command, handle.state(), sheet) && command.run(handle, sheet);
  if (outcome instanceof Promise) {
    void outcome.then(say);
  } else {
    say(outcome);
  }
}
