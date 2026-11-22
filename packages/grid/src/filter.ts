/**
 * O(log H) worker-local AutoFilter row projection and dropdown button geometry
 * matching the Rust `VisibleRowProjection` mapping.
 */

export interface FilterDisplayState {
  readonly headerRow: number;
  readonly lastRow: number;
  readonly firstCol: number;
  readonly lastCol: number;
  readonly activeCols: readonly number[];
  readonly hiddenRowSpans: readonly (readonly [number, number])[];
}

export interface CellRect {
  readonly firstRow: number;
  readonly lastRow: number;
  readonly firstCol: number;
  readonly lastCol: number;
}

export interface FilterButtonRect {
  readonly x: number;
  readonly y: number;
  readonly size: number;
}

export const FILTER_BUTTON_SIZE = 14;

/**
 * Computes the AutoFilter dropdown chevron button rectangle inside a header
 * cell, or `null` if the cell is too narrow/short to host the 14px control.
 */
export function filterButtonBox(
  cellX: number,
  cellY: number,
  cellW: number,
  cellH: number,
): FilterButtonRect | null {
  if (cellW < 18 || cellH < FILTER_BUTTON_SIZE) {
    return null;
  }
  const marginX = 3;
  const x = cellX + cellW - FILTER_BUTTON_SIZE - marginX;
  const y = cellY + Math.floor((cellH - FILTER_BUTTON_SIZE) / 2);
  return { x, y, size: FILTER_BUTTON_SIZE };
}

/**
 * Bidirectionally searchable mapping between visible display rows and physical
 * sheet row indices in O(log H) time over H disjoint hidden spans.
 */
export class FilterRowMap {
  readonly totalRows: number;
  readonly visibleRows: number;
  readonly #spans: readonly (readonly [number, number])[];
  readonly #cumHidden: readonly number[];
  readonly #displaySkips: readonly number[];

  constructor(totalRows: number, hiddenRowSpans: readonly (readonly [number, number])[]) {
    this.totalRows = Math.max(0, totalRows);
    const sorted = [...hiddenRowSpans]
      .filter(([s, e]) => s <= e && e >= 0 && s < this.totalRows)
      .map(([s, e]): [number, number] => [Math.max(0, s), Math.min(this.totalRows - 1, e)])
      .sort((a, b) => a[0] - b[0]);

    const merged: [number, number][] = [];
    for (const [s, e] of sorted) {
      const prev = merged[merged.length - 1];
      if (prev && s <= prev[1] + 1) {
        if (e > prev[1]) {
          prev[1] = e;
        }
      } else {
        merged.push([s, e]);
      }
    }

    const cumHidden: number[] = [];
    const displaySkips: number[] = [];
    let hiddenSoFar = 0;
    for (const [s, e] of merged) {
      displaySkips.push(s - hiddenSoFar);
      hiddenSoFar += e - s + 1;
      cumHidden.push(hiddenSoFar);
    }

    this.#spans = merged;
    this.#cumHidden = cumHidden;
    this.#displaySkips = displaySkips;
    this.visibleRows = Math.max(0, this.totalRows - hiddenSoFar);
  }

  get hasHiddenRows(): boolean {
    return this.#spans.length > 0;
  }

  /** Maps a visible display row `0..visibleRows` to its physical row index. */
  displayToPhysical(displayRow: number): number {
    if (this.#spans.length === 0 || displayRow < 0 || displayRow >= this.visibleRows) {
      return displayRow;
    }
    let lo = 0;
    let hi = this.#displaySkips.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if ((this.#displaySkips[mid] ?? 0) <= displayRow) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    const hiddenBefore = lo > 0 ? (this.#cumHidden[lo - 1] ?? 0) : 0;
    return displayRow + hiddenBefore;
  }

  /** Maps a physical row index `0..totalRows` to its display row, or `null` if hidden. */
  physicalToDisplay(physicalRow: number): number | null {
    if (physicalRow < 0 || physicalRow >= this.totalRows) {
      return null;
    }
    if (this.#spans.length === 0) {
      return physicalRow;
    }
    let lo = 0;
    let hi = this.#spans.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      const span = this.#spans[mid];
      if (span && span[0] <= physicalRow) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    if (lo > 0) {
      const prev = this.#spans[lo - 1];
      if (prev && physicalRow <= prev[1]) {
        return null;
      }
      const hiddenBefore = this.#cumHidden[lo - 1] ?? 0;
      return physicalRow - hiddenBefore;
    }
    return physicalRow;
  }

  /** Returns `true` if hidden row(s) lie immediately above `displayRow`. */
  isGapBefore(displayRow: number): boolean {
    if (this.#displaySkips.length === 0) {
      return false;
    }
    let lo = 0;
    let hi = this.#displaySkips.length - 1;
    while (lo <= hi) {
      const mid = (lo + hi) >>> 1;
      const v = this.#displaySkips[mid] ?? -1;
      if (v === displayRow) {
        return true;
      }
      if (v < displayRow) {
        lo = mid + 1;
      } else {
        hi = mid - 1;
      }
    }
    return false;
  }

  /** Returns `true` if hidden row(s) lie immediately below `displayRow`. */
  isGapAfter(displayRow: number): boolean {
    return this.isGapBefore(displayRow + 1);
  }
}
