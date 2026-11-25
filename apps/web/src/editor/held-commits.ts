import type { Grid, SheetSize } from "@reticula/grid";

/** What was typed into a cell whose row, column or contents are not known yet. */
export interface HeldCommit {
  readonly sheet: number;
  readonly row: number;
  readonly col: number;
  readonly raw: string;
  /** Whether it goes behind what is in the cell. */
  readonly isRest?: boolean;
}

export interface CellIds {
  readonly rowId: string;
  readonly colId: string;
}

/** What the engine has in a cell. */
export interface CellContents extends CellIds {
  readonly sheet: number;
  readonly raw: string;
}

export interface HeldCommitsHost {
  /** Knows the rows and columns of the sheet in view, once the pages with them have come. */
  readonly grid: Pick<Grid, "rowIdAt" | "colIdAt" | "sheetSize">;
  sheetId(): number;
  /** Asks the engine what is in a cell: `retry` is to be called with the answer. */
  inspect(sheet: number, ids: CellIds): void;
  /** Returns `false` if nothing may be changed. */
  commit(held: HeldCommit, ids: CellIds): boolean;
  /** Called for what could not be committed in time, or may be for another cell by now. */
  fail(held: HeldCommit): void;
}

interface Held extends HeldCommit {
  /** What was known of the cell when it was typed into. */
  readonly rowId: string | null;
  readonly colId: string | null;
  /** The sheet its row and column were counted in. */
  readonly size: SheetSize;
  readonly timer: ReturnType<typeof setTimeout>;
}

/** How long the page with a cell may take to come. */
export const HOLD_MS = 5_000;

/**
 * Keeps what was typed into cells until it is known which they are: right after
 * another sheet or a far part of one has come into view, that takes a moment.
 * Likewise until it is known what is in them, if it goes behind that.
 */
export class HeldCommits {
  readonly #host: HeldCommitsHost;
  #held: Held[] = [];

  constructor(host: HeldCommitsHost) {
    this.#host = host;
  }

  /** Commits what was typed into a cell, now if its row and column are known, as they may be to the caller. */
  hold(commit: HeldCommit, ids?: CellIds | null): void {
    const { grid } = this.#host;
    const timer = setTimeout(() => this.#giveUp(timer), HOLD_MS);
    this.#held.push({
      ...commit,
      rowId: ids?.rowId ?? grid.rowIdAt(commit.row),
      colId: ids?.colId ?? grid.colIdAt(commit.col),
      size: grid.sheetSize,
      timer,
    });
    this.retry();
  }

  /** To be called when a page has come, or the engine has said what is in a cell. */
  retry(inspected?: CellContents): void {
    const { commit, fail, inspect } = this.#host;
    let known = inspected;
    this.#held = this.#held.filter((one) => {
      const cell = this.#cellOf(one);
      if (cell === null) {
        return true;
      }
      let { raw } = one;
      if (one.isRest && cell !== "lost") {
        if (
          known?.sheet !== one.sheet ||
          known.rowId !== cell.rowId ||
          known.colId !== cell.colId
        ) {
          inspect(one.sheet, cell);
          return true;
        }
        raw = known.raw + raw;
        known = { ...known, raw };
      }
      clearTimeout(one.timer);
      if (cell === "lost" || !commit({ ...one, raw }, cell)) {
        fail(one);
      }
      return false;
    });
  }

  /** Gives up on everything: another workbook is being opened, or rows or columns are being moved. */
  clear(): void {
    const held = this.#held;
    this.#held = [];
    for (const one of held) {
      clearTimeout(one.timer);
      this.#host.fail(one);
    }
  }

  /** Which cell it is, or `null` as long as that is not known. */
  #cellOf({ sheet, row, col, rowId, colId, size }: Held): CellIds | "lost" | null {
    const { grid, sheetId } = this.#host;
    if (sheet !== sheetId()) {
      return null;
    }
    const { rows, cols } = grid.sheetSize;
    // Rows or columns have been inserted or deleted: another may be in its place.
    if ((rowId === null && rows !== size.rows) || (colId === null && cols !== size.cols)) {
      return "lost";
    }
    const knownRow = rowId ?? grid.rowIdAt(row);
    const knownCol = colId ?? grid.colIdAt(col);
    return knownRow !== null && knownCol !== null ? { rowId: knownRow, colId: knownCol } : null;
  }

  #giveUp(timer: ReturnType<typeof setTimeout>): void {
    const late = this.#held.find((one) => one.timer === timer);
    if (late) {
      this.#held = this.#held.filter((one) => one !== late);
      this.#host.fail(late);
    }
  }
}
