import type { PageCache } from "../cache.ts";
import type { SheetLayout } from "../layout.ts";
import type { Page } from "../page.ts";
import { type Look, NO_COLOR, type StyleTable } from "./look.ts";
import { lineWidth, RectList } from "./rects.ts";
import { layoutText, type PageGeometry, TextRuns } from "./text-layout.ts";
import type { TextMeasure } from "./text-measure.ts";

/** Text is laid out this many rows at a time, as they come into view. */
export const BAND_ROWS = 8;

/** The texts of some rows of a page, and the lines through them. */
export interface TextBand {
  readonly first: number;
  readonly end: number;
  readonly firstStrike: number;
  readonly endStrike: number;
}

/** Position of a row or column edge in device pixels from the sheet's corner. */
export function edgeOf(axis: SheetLayout["rowAxis"], index: number, scale: number): number {
  return Math.round(axis.offsetOf(index) * scale);
}

/** What pages are laid out with. The same objects mean the same layouts. */
export interface PageSetting {
  readonly measure: TextMeasure;
  readonly look: Look;
  readonly styles: StyleTable;
  readonly frozenCols: number;
  readonly clipText?: boolean;
}

/** The edges of a page's rows and columns relative to its first ones. */
export function geometryOf(page: Page, layout: SheetLayout, scale: number): PageGeometry {
  const { rowAxis, colAxis } = layout;
  const originX = edgeOf(colAxis, page.firstCol, scale);
  const originY = edgeOf(rowAxis, page.firstRow, scale);
  const xs = new Int32Array(page.colCount + 1);
  for (let col = 0; col <= page.colCount; col++) {
    xs[col] = edgeOf(colAxis, page.firstCol + col, scale) - originX;
  }
  const ys = new Int32Array(page.rowCount + 1);
  for (let row = 0; row <= page.rowCount; row++) {
    ys[row] = edgeOf(rowAxis, page.firstRow + row, scale) - originY;
  }
  const overflowXs = new Int32Array(page.overflows.length * 2);
  page.overflows.forEach((record, index) => {
    overflowXs[index * 2] = edgeOf(colAxis, record.originCol, scale) - originX;
    overflowXs[index * 2 + 1] = edgeOf(colAxis, record.originCol + 1, scale) - originX;
  });
  return { xs, ys, overflowXs };
}

function same(a: Int32Array, b: Int32Array): boolean {
  return a.length === b.length && a.every((value, index) => value === b[index]);
}

/** Rectangles of the cells' own fills; cells that look the same share one. */
export function layoutFills(page: Page, geometry: PageGeometry, styles: StyleTable): RectList {
  const { xs, ys } = geometry;
  const { rowCount, colCount } = page;
  const fills = new RectList(8);
  // The rectangles that end at the row above, by the column they start in.
  let above = new Int32Array(colCount).fill(-1);
  let current = new Int32Array(colCount);
  for (let row = 0; row < rowCount; row++) {
    current.fill(-1);
    const top = ys[row] ?? 0;
    const height = (ys[row + 1] ?? 0) - top;
    for (let col = 0; col < colCount; ) {
      const color = styles.get(page.styleIds[row * colCount + col] ?? 0).fill;
      let end = col + 1;
      while (
        end < colCount &&
        styles.get(page.styleIds[row * colCount + end] ?? 0).fill === color
      ) {
        end++;
      }
      if (color !== NO_COLOR) {
        const left = xs[col] ?? 0;
        const width = (xs[end] ?? 0) - left;
        const upper = above[col] ?? -1;
        if (
          upper >= 0 &&
          fills.colors[upper] === color &&
          fills.geometry[upper * 4 + 2] === width
        ) {
          fills.geometry[upper * 4 + 3] = (fills.geometry[upper * 4 + 3] ?? 0) + height;
          current[col] = upper;
        } else {
          const before = fills.length;
          fills.push(left, top, width, height, color);
          current[col] = fills.length > before ? before : -1;
        }
      }
      col = end;
    }
    [above, current] = [current, above];
  }
  return fills;
}

/**
 * Rectangles of the cells' borders. An edge two cells share is drawn once, in
 * the heavier of their weights, and where edges meet one of them has the corner:
 * no pixel is painted twice.
 */
export function layoutBorders(
  page: Page,
  geometry: PageGeometry,
  styles: StyleTable,
  look: Look,
): RectList {
  const borders = new RectList(8);
  const { xs, ys } = geometry;
  const { rowCount, colCount } = page;
  // Edges above each row and below the last, and to the left of each column
  // and right of the last. Only what has a border is worth the arrays.
  let across: Uint8Array | null = null;
  let down: Uint8Array | null = null;
  const stride = colCount + 1;
  for (let cell = 0; cell < rowCount * colCount; cell++) {
    const id = page.styleIds[cell] ?? 0;
    if (id === 0) {
      continue;
    }
    const style = styles.get(id);
    const { borderTop, borderRight, borderBottom, borderLeft } = style;
    if (borderTop + borderRight + borderBottom + borderLeft === 0) {
      continue;
    }
    across ??= new Uint8Array((rowCount + 1) * colCount);
    down ??= new Uint8Array(rowCount * stride);
    const col = cell % colCount;
    const left = cell + (cell - col) / colCount;
    across[cell] = Math.max(across[cell] ?? 0, borderTop);
    across[cell + colCount] = Math.max(across[cell + colCount] ?? 0, borderBottom);
    down[left] = Math.max(down[left] ?? 0, borderLeft);
    down[left + 1] = Math.max(down[left + 1] ?? 0, borderRight);
  }
  if (!across || !down) {
    return borders;
  }

  const acrossAt = (row: number, col: number) =>
    col < 0 || col >= colCount ? 0 : (across[row * colCount + col] ?? 0);
  const downAt = (row: number, col: number) =>
    row < 0 || row >= rowCount ? 0 : (down[row * stride + col] ?? 0);
  // A thin border covers the grid line, which is in front of its edge; a medium
  // one is wider by as much on either side as whole pixels allow.
  const thin = lineWidth(1, look.scale);
  const medium = lineWidth(2, look.scale);
  const spare = Math.floor((medium - thin) / 2);
  const before = [0, thin, thin + spare];
  const after = [0, 0, medium - thin - spare];
  const beforeOf = (weight: number) => before[weight] ?? 0;
  const afterOf = (weight: number) => after[weight] ?? 0;
  const color = look.text;

  const corners = new Uint8Array((rowCount + 1) * stride);
  const cornerTaken = new Uint8Array((rowCount + 1) * stride);
  for (let row = 0; row <= rowCount; row++) {
    for (let col = 0; col <= colCount; col++) {
      corners[row * stride + col] = Math.max(
        acrossAt(row, col - 1),
        acrossAt(row, col),
        downAt(row - 1, col),
        downAt(row, col),
      );
    }
  }

  for (let row = 0; row <= rowCount; row++) {
    const y = ys[row] ?? 0;
    let weight = 0;
    let start = 0;
    const close = (end: number) => {
      if (weight > 0) {
        borders.push(
          start,
          y - beforeOf(weight),
          end - start,
          beforeOf(weight) + afterOf(weight),
          color,
        );
      }
      weight = 0;
    };
    for (let col = 0; col <= colCount; col++) {
      const x = xs[col] ?? 0;
      const corner = corners[row * stride + col] ?? 0;
      const next = acrossAt(row, col);
      const taken = corner > 0 && (corner === next || corner === acrossAt(row, col - 1));
      if (taken) {
        cornerTaken[row * stride + col] = 1;
      }
      if (weight > 0 && (weight !== corner || next !== weight)) {
        close(weight === corner ? x + afterOf(corner) : x - beforeOf(corner));
      }
      if (weight === 0 && next > 0) {
        const own = taken && corner === next && corner !== acrossAt(row, col - 1);
        weight = next;
        start = own ? x - beforeOf(corner) : x + afterOf(corner);
      }
    }
  }

  for (let col = 0; col <= colCount; col++) {
    const x = xs[col] ?? 0;
    let weight = 0;
    let start = 0;
    const close = (end: number) => {
      if (weight > 0) {
        borders.push(
          x - beforeOf(weight),
          start,
          beforeOf(weight) + afterOf(weight),
          end - start,
          color,
        );
      }
      weight = 0;
    };
    for (let row = 0; row <= rowCount; row++) {
      const y = ys[row] ?? 0;
      const corner = corners[row * stride + col] ?? 0;
      const free = cornerTaken[row * stride + col] === 0;
      const next = downAt(row, col);
      const previous = downAt(row - 1, col);
      if (weight > 0 && !(free && weight === corner && next === weight)) {
        close(free && weight === corner ? y + afterOf(corner) : y - beforeOf(corner));
      }
      if (weight === 0 && next > 0) {
        const own = free && corner === next && corner !== previous;
        weight = next;
        start = own ? y - beforeOf(corner) : y + afterOf(corner);
      }
    }
  }
  return borders;
}

/**
 * Everything a page looks like, in device pixels from its origin: independent
 * of where it is scrolled to and of the pages around it, so that it can be kept
 * until the page itself changes. What is asked for is laid out then.
 */
export class PageLayout {
  readonly page: Page;
  readonly geometry: PageGeometry;
  #setting: PageSetting;
  #spare: TextRuns[];
  #stylesVersion: number;
  #stylesLength: number;
  #layout: SheetLayout;
  #fills: RectList | null = null;
  #borders: RectList | null = null;
  #texts: TextRuns | null = null;
  /** Where the texts of each band start: after the most that those before it can have. Then the end. */
  #bandStarts: number[] = [];
  #bands: (TextBand | undefined)[] = [];
  #drawn: Uint16Array | null = null;

  constructor(page: Page, layout: SheetLayout, setting: PageSetting, spare: TextRuns[] = []) {
    this.page = page;
    this.geometry = geometryOf(page, layout, setting.look.scale);
    this.#layout = layout;
    this.#setting = setting;
    this.#spare = spare;
    this.#stylesVersion = setting.styles.version;
    this.#stylesLength = setting.styles.length;
  }

  /** Whether the layout still holds; sizes are compared, as every edit makes new axes. */
  isValidFor(layout: SheetLayout, setting: PageSetting): boolean {
    const { look, styles, frozenCols, clipText } = this.#setting;
    if (
      look !== setting.look ||
      styles !== setting.styles ||
      frozenCols !== setting.frozenCols ||
      clipText !== setting.clipText ||
      styles.version !== this.#stylesVersion
    ) {
      return false;
    }
    if (styles.length !== this.#stylesLength) {
      // Styles the page was still waiting for may be among the new ones.
      if (this.#lacksStyles()) {
        return false;
      }
      this.#stylesLength = styles.length;
    }
    if (layout !== this.#layout) {
      const { xs, ys, overflowXs } = geometryOf(this.page, layout, look.scale);
      const { geometry } = this;
      if (
        !same(xs, geometry.xs) ||
        !same(ys, geometry.ys) ||
        !same(overflowXs, geometry.overflowXs)
      ) {
        return false;
      }
      this.#layout = layout;
    }
    return true;
  }

  get fills(): RectList {
    this.#fills ??= this.page.styleIds.every((id) => id === 0)
      ? EMPTY
      : layoutFills(this.page, this.geometry, this.#setting.styles);
    return this.#fills;
  }

  get borders(): RectList {
    // That is what a page without styles has for fills.
    this.#borders ??=
      this.fills === EMPTY
        ? EMPTY
        : layoutBorders(this.page, this.geometry, this.#setting.styles, this.#setting.look);
    return this.#borders;
  }

  /** The page's texts, as far as its bands have been asked for. */
  get texts(): TextRuns {
    if (!this.#texts) {
      this.#drawn ??= this.#countCells();
      const most = this.#bandStarts.at(-1) ?? 0;
      const spare = this.#spare.pop();
      this.#texts = spare && spare.x.length >= most ? spare : new TextRuns(most);
    }
    return this.#texts;
  }

  /** Forgets the texts. */
  release(): void {
    if (this.#texts && this.#spare.length < MOST_SPARE) {
      this.#texts.clear();
      this.#spare.push(this.#texts);
    }
    this.#texts = null;
    this.#bands = [];
  }

  /** Which of the texts are those of a band of rows, with what overflows into it from the left. */
  band(index: number): TextBand {
    let band = this.#bands[index];
    if (!band) {
      const { texts } = this;
      const firstRow = index * BAND_ROWS;
      const endRow = Math.min(this.page.rowCount, firstRow + BAND_ROWS);
      // Each band has a place of its own, in whatever order they are laid out.
      const first = this.#bandStarts[index] ?? 0;
      const firstStrike = texts.strikes.length;
      texts.length = first;
      layoutText(this.page, this.geometry, this.#setting, firstRow, endRow, texts);
      const end = texts.length;
      texts.batch(first, end);
      band = { first, end, firstStrike, endStrike: texts.strikes.length };
      this.#bands[index] = band;
    }
    return band;
  }

  /** How many cells of a range of the page's rows and columns show text of their own. */
  cellsWithText(firstRow: number, endRow: number, firstCol: number, endCol: number): number {
    this.#drawn ??= this.#countCells();
    const drawn = this.#drawn;
    const at = (row: number, col: number) => drawn[row * (this.page.colCount + 1) + col] ?? 0;
    return (
      at(endRow, endCol) - at(firstRow, endCol) - at(endRow, firstCol) + at(firstRow, firstCol)
    );
  }

  /**
   * Counts the texts. Returns sums of those that are drawn, over the rows and
   * columns before each corner of a cell.
   */
  #countCells(): Uint16Array {
    const { rowCount, colCount, texts, overflows } = this.page;
    const { xs } = this.geometry;
    const stride = colCount + 1;
    const least = 2 * this.#setting.look.padding;
    const sums = new Uint16Array((rowCount + 1) * stride);
    let most = 0;
    for (let row = 0; row < rowCount; row++) {
      if (row % BAND_ROWS === 0) {
        this.#bandStarts.push(most);
      }
      // No more than one text overflows into a row.
      most += overflows.length > 0 ? 1 : 0;
      let sum = 0;
      for (let col = 0; col < colCount; col++) {
        if (texts[row * colCount + col]) {
          most++;
          sum += (xs[col + 1] ?? 0) - (xs[col] ?? 0) > least ? 1 : 0;
        }
        sums[(row + 1) * stride + col + 1] = (sums[row * stride + col + 1] ?? 0) + sum;
      }
    }
    this.#bandStarts.push(most);
    return sums;
  }

  #lacksStyles(): boolean {
    const known = this.#stylesLength;
    return (
      this.page.styleIds.some((id) => id >= known && id > 0) ||
      this.page.overflows.some((record) => record.styleId >= known && record.styleId > 0)
    );
  }
}

const EMPTY = new RectList(1);

/** How many pages' room for texts is kept for others. */
const MOST_SPARE = 16;

/** The layouts of the pages that are held. */
export class PageLayouts {
  #layouts = new Map<Page, PageLayout>();
  /**
   * Room for texts that pages no longer need. Memory that has been written to is
   * faster to lay out texts in than new memory, and leaves no garbage: when the
   * sheet is scrolled fast, every frame lays out a screen of them.
   */
  #spare: TextRuns[] = [];

  /** Forgets the pages that are not held any more, and what was laid out of them. */
  begin(cache: PageCache): void {
    for (const [page, held] of this.#layouts) {
      if (cache.pageOf(page.firstRow, page.firstCol) !== page) {
        held.release();
        this.#layouts.delete(page);
      }
    }
  }

  get(page: Page, layout: SheetLayout, setting: PageSetting): PageLayout {
    let held = this.#layouts.get(page);
    if (!held?.isValidFor(layout, setting)) {
      held?.release();
      held = new PageLayout(page, layout, setting, this.#spare);
      this.#layouts.set(page, held);
    }
    return held;
  }
}
