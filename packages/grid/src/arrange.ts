import type { Grid, IdRun } from "./grid.ts";

// Moving and resizing what is selected, for the commands that do, and loaded with
// them. What it needs of the grid it asks the grid for: imports would have the
// bundler take the grid apart.

type Axis = "row" | "col";

interface Cell {
  readonly row: number;
  readonly col: number;
}

/** How long the engine has to send rows or columns that have moved, after which it is taken not to have moved them. */
const MOVE_SETTLE_MS = 2_000;

/** The identifiers of some rows or columns, or `null` if not all of them are known. */
function idsOf(grid: Grid, axis: Axis, first: number, end: number): string[] | null {
  const ids: string[] = [];
  for (let position = first; position < end; position++) {
    const id = axis === "row" ? grid.rowIdAt(position) : grid.colIdAt(position);
    if (id === null) {
      return null;
    }
    ids.push(id);
  }
  return ids;
}

/**
 * Whether the rows or columns that were moved last are still held where they
 * were before. Their pages arrive one by one, some of which are not of them.
 */
function isMoving(grid: Grid): boolean {
  if (grid.moved) {
    const { axis, first, ids, until } = grid.moved;
    const held = idsOf(grid, axis, first, first + ids.length);
    if (performance.now() >= until || ids.every((id, index) => id === held?.[index])) {
      grid.moved = null;
    }
  }
  return grid.moved !== null;
}

function moveSelected(grid: Grid, axis: Axis, direction: -1 | 1): boolean {
  const { selection, selectionFocus, selectionRanges, sheetSize } = grid;
  const isRow = axis === "row";
  const [lines] = grid.selectedLines(axis);
  if (!lines || !selection || !selectionFocus || selectionRanges.length > 1 || isMoving(grid)) {
    return false;
  }
  const { first, end } = lines;
  const before = direction < 0 ? first - 1 : end + 1;
  if (before < 0 || before > (isRow ? sheetSize.rows : sheetSize.cols)) {
    return false;
  }
  const after = before === 0 ? null : idsOf(grid, axis, before - 1, before)?.[0];
  const ids = idsOf(grid, axis, first, end);
  const passed =
    direction < 0 ? idsOf(grid, axis, first - 1, first) : idsOf(grid, axis, end, end + 1);
  const runs: readonly IdRun[] | null = isRow ? grid.selectedRowRuns() : grid.selectedColRuns();
  if (after === undefined || !ids || !passed || !runs || !grid.moveLines(axis, runs, after)) {
    return false;
  }
  grid.moved = {
    axis,
    first: direction < 0 ? first - 1 : first,
    ids: direction < 0 ? [...ids, ...passed] : [...passed, ...ids],
    until: performance.now() + MOVE_SETTLE_MS,
  };
  const moved = ({ row, col }: Cell): Cell =>
    isRow ? { row: row + direction, col } : { row, col: col + direction };
  grid.selectRange(moved(selection), moved(selectionFocus), grid.selectionScope);
  grid.revealAnchor();
  return true;
}

/**
 * Moves the selected rows past the row before or after them, and the selection
 * with them. Returns whether it did: there has to be such a row, and the grid
 * has to know the rows, which it does of the ones around what is in view.
 */
export function moveSelectedRows(grid: Grid, direction: -1 | 1): boolean {
  return moveSelected(grid, "row", direction);
}

export function moveSelectedCols(grid: Grid, direction: -1 | 1): boolean {
  return moveSelected(grid, "col", direction);
}

function resizeSelected(grid: Grid, axis: Axis, px: number | null): boolean {
  const lines = grid.selectedLines(axis);
  return (
    lines.length > 0 &&
    grid.resizeLines(
      axis,
      lines.map((run) => [run, px] as const),
    )
  );
}

/**
 * Gives the selected rows a height in pixels at a zoom of 1, or the standard
 * one. Returns whether it did; the grid has to know the rows.
 */
export function resizeSelectedRows(grid: Grid, px: number | null): boolean {
  return resizeSelected(grid, "row", px);
}

/** The same for the width of the selected columns. */
export function resizeSelectedCols(grid: Grid, px: number | null): boolean {
  return resizeSelected(grid, "col", px);
}

/**
 * Makes the selected columns as wide as their texts in view need. Returns
 * whether there was any of them whose texts the grid has.
 */
export function autofitSelectedCols(grid: Grid): boolean {
  const { custom, standard } = grid.sizesOf("col");
  const widths = new Map(custom);
  const row = grid.stats().visible.firstRow;
  const sizes: (readonly [{ readonly first: number; readonly end: number }, number])[] = [];
  let found = false;
  for (const { first, end } of grid.selectedLines("col")) {
    for (let col = first; col < end; col++) {
      if (grid.textAt(row, col) === null) {
        continue;
      }
      found = true;
      const px = grid.fitWidth(col);
      if (px !== (widths.get(col) ?? standard)) {
        sizes.push([{ first: col, end: col + 1 }, px]);
      }
    }
  }
  if (sizes.length > 0) {
    grid.resizeLines("col", sizes);
  }
  return found;
}

/** The size that all of the selected rows or columns have, or `null` if they have none in common. */
function commonSize(grid: Grid, axis: Axis): number | null {
  const { custom, standard } = grid.sizesOf(axis);
  let common: number | null = null;
  let count = 0;
  let sized = 0;
  for (const { first, end } of grid.selectedLines(axis)) {
    count += end - first;
    for (const [index, size] of custom) {
      if (index >= first && index < end) {
        if (common !== null && size !== common) {
          return null;
        }
        common = size;
        sized++;
      }
    }
  }
  if (count === 0) {
    return null;
  }
  if (sized === 0) {
    return standard;
  }
  return sized === count || common === standard ? common : null;
}

/** The sizes that all selected rows and columns have, in pixels at a zoom of 1. */
export function sizeOfSelection(grid: Grid): {
  readonly rowPx: number | null;
  readonly colPx: number | null;
} {
  return { rowPx: commonSize(grid, "row"), colPx: commonSize(grid, "col") };
}
