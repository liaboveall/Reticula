import { PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import { filterButtonBox } from "../filter.ts";
import {
  columnName,
  defaultLayout,
  frozenPixels,
  isEmpty,
  type SheetLayout,
  type SheetSize,
  type Span,
} from "../layout.ts";
import type { Colors, PackedColor } from "./color.ts";
import {
  type DiffHighlightKind,
  type DiffHighlightMap,
  FILL_HANDLE_SIZE,
  type Frame,
  type GridCellStyle,
} from "./frame.ts";
import { FIXED, type Look, makeLook, NO_COLOR, StyleTable } from "./look.ts";
import {
  BAND_ROWS,
  edgeOf,
  type PageLayout,
  PageLayouts,
  type PageSetting,
} from "./page-layout.ts";
import {
  addDashedOutline,
  addOutline,
  isEmptyBox,
  lineWidth,
  type PixelBox,
  RectList,
} from "./rects.ts";
import type { SpriteSpec } from "./sprites.ts";
import { TextRuns } from "./text-layout.ts";
import type { TextMeasure } from "./text-measure.ts";

/** Part of a page: its layout, where the page's origin is, and what of it may be painted. */
export interface PageStep {
  readonly kind: "fills" | "borders" | "texts";
  readonly layout: PageLayout;
  readonly x: number;
  readonly y: number;
  readonly clip: PixelBox;
  /** Rows of the page that are in view. */
  readonly firstRow: number;
  readonly endRow: number;
}

/** One thing to paint, over all the things before it. Positions are device pixels. */
export type SceneStep =
  /** Rectangles `first` to `end` of the scene's. */
  | { readonly kind: "rects"; readonly first: number; readonly end: number }
  | PageStep
  | { readonly kind: "labels"; readonly runs: TextRuns }
  | {
      readonly kind: "sprite";
      readonly spec: SpriteSpec;
      readonly x: number;
      readonly y: number;
      readonly clip: PixelBox;
    };

/** A frame as primitives in the order they are painted. Holds until the next one is built. */
export interface Scene {
  /** Size in device pixels. */
  readonly width: number;
  readonly height: number;
  readonly look: Look;
  readonly rects: RectList;
  readonly steps: readonly SceneStep[];
  /** Cells in view that have text. */
  readonly cells: number;
  /** Cells in view whose page has not arrived. */
  readonly missing: number;
}

/** A part of the sheet that scrolls in its own way, between its edges on the screen. */
interface Quadrant {
  readonly clip: PixelBox;
  readonly span: Span;
  /** Device position of a column's or a row's leading edge. */
  readonly x: (col: number) => number;
  readonly y: (row: number) => number;
}

/** The cells of a diff, by row. */
interface DiffCells {
  readonly rows: Uint32Array;
  readonly cols: Uint32Array;
  readonly kinds: readonly DiffHighlightKind[];
}

function parseDiff(map: DiffHighlightMap): DiffCells {
  const cells: { row: number; col: number; kind: DiffHighlightKind }[] = [];
  for (const [key, kind] of map) {
    const separator = key.indexOf(":");
    if (separator > 0) {
      const row = Number(key.slice(0, separator));
      cells.push({ row, col: Number(key.slice(separator + 1)), kind });
    }
  }
  cells.sort((a, b) => a.row - b.row || a.col - b.col);
  return {
    rows: Uint32Array.from(cells, (cell) => cell.row),
    cols: Uint32Array.from(cells, (cell) => cell.col),
    kinds: cells.map((cell) => cell.kind),
  };
}

/** First index whose value is at least `value`. */
function lowerBound(sorted: Uint32Array, value: number): number {
  let low = 0;
  let high = sorted.length;
  while (low < high) {
    const middle = (low + high) >>> 1;
    if ((sorted[middle] ?? 0) < value) {
      low = middle + 1;
    } else {
      high = middle;
    }
  }
  return low;
}

function overlaps(a: Span, b: Span): boolean {
  return (
    a.endRow > b.firstRow && a.firstRow < b.endRow && a.endCol > b.firstCol && a.firstCol < b.endCol
  );
}

function holds(span: Span, row: number, col: number): boolean {
  return row >= span.firstRow && row < span.endRow && col >= span.firstCol && col < span.endCol;
}

function isSingle(span: Span): boolean {
  return span.endRow - span.firstRow <= 1 && span.endCol - span.firstCol <= 1;
}

/** Dash and gap of the outlines that are not solid, in CSS pixels. */
const DASH = [4, 3] as const;

const NO_STYLES: readonly GridCellStyle[] = [];

/** Turns frames into primitives, keeping what does not change between them. */
export class SceneBuilder {
  readonly measure: TextMeasure;
  #colors: Colors;
  #look: Look | null = null;
  #styles: StyleTable;
  #setting: PageSetting | null = null;
  #layouts = new PageLayouts();
  #diff: {
    readonly map: DiffHighlightMap;
    readonly size: number;
    readonly cells: DiffCells;
  } | null = null;
  #lastDefault: { readonly metrics: Frame["metrics"]; readonly layout: SheetLayout } | null = null;
  #rects = new RectList(512);
  #labels = new TextRuns(128);
  #steps: SceneStep[] = [];
  /** Rectangles up to this one are part of a step. */
  #mark = 0;
  #cells = 0;
  #missing = 0;

  constructor(measure: TextMeasure, colors: Colors) {
    this.measure = measure;
    this.#colors = colors;
    this.#styles = new StyleTable(colors);
  }

  /** Forgets everything that was measured or laid out. */
  reset(): void {
    this.#look = null;
    this.#setting = null;
    this.#layouts = new PageLayouts();
  }

  build(frame: Frame): Scene {
    const { size, scale, metrics, visible } = frame;
    const look = this.#lookOf(frame);
    const layout = frame.layout ?? this.#defaultLayout(frame);
    const sheet: SheetSize = frame.sheet ?? {
      rows: Math.max(visible.endRow, layout.frozenRows),
      cols: Math.max(visible.endCol, layout.frozenCols),
    };
    const frozen = frozenPixels(metrics, sheet, size, layout);
    this.#styles.use(frame.styles ?? NO_STYLES, frame.theme);
    const setting = this.#settingOf(look, frozen.frozenCols);

    const rects = this.#rects;
    rects.clear();
    this.#labels.clear();
    this.#steps = [];
    this.#mark = 0;
    this.#cells = 0;
    this.#missing = 0;

    const width = Math.round(size.width * scale);
    const height = Math.round(size.height * scale);
    rects.push(0, 0, width, height, look.background);

    const pixels = (css: number) => Math.round(css * scale);
    const left = Math.min(width, pixels(metrics.headerWidth));
    const top = Math.min(height, pixels(metrics.headerHeight));
    const right = Math.max(left, pixels(size.width - metrics.scrollbarSize));
    const bottom = Math.max(top, pixels(size.height - metrics.scrollbarSize));
    const frozenX = Math.min(right, left + edgeOf(layout.colAxis, frozen.frozenCols, scale));
    const frozenY = Math.min(bottom, top + edgeOf(layout.rowAxis, frozen.frozenRows, scale));
    const scrollX = pixels(frame.scroll.left);
    const scrollY = pixels(frame.scroll.top);

    const pinnedX = (col: number) => left + edgeOf(layout.colAxis, col, scale);
    const pinnedY = (row: number) => top + edgeOf(layout.rowAxis, row, scale);
    const scrolledX = (col: number) => pinnedX(col) - scrollX;
    const scrolledY = (row: number) => pinnedY(row) - scrollY;
    const colsPinned = { firstCol: 0, endCol: frozen.frozenCols };
    const rowsPinned = { firstRow: 0, endRow: frozen.frozenRows };
    const leftOf = { left, right: frozenX };
    const rightOf = { left: frozenX, right };
    const above = { top, bottom: frozenY };
    const below = { top: frozenY, bottom };
    const quadrants: Quadrant[] = [
      {
        clip: { ...leftOf, ...above },
        span: { ...rowsPinned, ...colsPinned },
        x: pinnedX,
        y: pinnedY,
      },
      {
        clip: { ...rightOf, ...above },
        span: { ...visible, ...rowsPinned },
        x: scrolledX,
        y: pinnedY,
      },
      {
        clip: { ...leftOf, ...below },
        span: { ...visible, ...colsPinned },
        x: pinnedX,
        y: scrolledY,
      },
      { clip: { ...rightOf, ...below }, span: visible, x: scrolledX, y: scrolledY },
    ];
    for (const quadrant of quadrants) {
      if (!isEmptyBox(quadrant.clip) && !isEmpty(quadrant.span)) {
        this.#quadrant(frame, look, layout, setting, quadrant);
      }
    }

    const strips: Strip[] = [
      { ...colsPinned, ...rowsPinned, ...leftOf, ...above, x: pinnedX, y: pinnedY },
      { ...visible, ...rightOf, ...below, x: scrolledX, y: scrolledY },
    ];
    this.#headers(frame, look, sheet, { left, top, right: width, bottom: height }, strips);
    this.#dividers(look, frozen, { left: 0, top: 0, right, bottom }, frozenX, frozenY);
    this.#reorderGuide(frame, look, sheet, frozen, { left, top, right, bottom }, strips);
    this.#flush();

    return {
      width,
      height,
      look,
      rects,
      steps: this.#steps,
      cells: this.#cells,
      missing: this.#missing,
    };
  }

  #lookOf(frame: Frame): Look {
    if (this.#look?.theme !== frame.theme || this.#look.scale !== frame.scale) {
      this.#look = makeLook(frame.theme, frame.scale, this.measure, this.#colors);
    }
    return this.#look;
  }

  #settingOf(look: Look, frozenCols: number): PageSetting {
    if (this.#setting?.look !== look || this.#setting.frozenCols !== frozenCols) {
      this.#setting = { measure: this.measure, look, styles: this.#styles, frozenCols };
    }
    return this.#setting;
  }

  /** The layout of a frame without one, the same for the same sizes. */
  #defaultLayout(frame: Frame): SheetLayout {
    if (this.#lastDefault?.metrics !== frame.metrics) {
      this.#lastDefault = { metrics: frame.metrics, layout: defaultLayout(frame.metrics) };
    }
    return this.#lastDefault.layout;
  }

  /** Makes a step of the rectangles added since the last one. */
  #flush(): void {
    const end = this.#rects.length;
    if (end > this.#mark) {
      this.#steps.push({ kind: "rects", first: this.#mark, end });
      this.#mark = end;
    }
  }

  #step(step: SceneStep): void {
    this.#flush();
    this.#steps.push(step);
  }

  #quadrant(
    frame: Frame,
    look: Look,
    layout: SheetLayout,
    setting: PageSetting,
    quadrant: Quadrant,
  ): void {
    const pages = this.#pagesOf(frame, look, layout, setting, quadrant);
    for (const page of pages) {
      if (page.layout.fills.length > 0) {
        this.#step({ kind: "fills", ...page });
      }
    }
    this.#selectionFills(frame, look, quadrant);
    this.#lines(look, quadrant);
    for (const page of pages) {
      if (page.layout.borders.length > 0) {
        this.#step({ kind: "borders", ...page });
      }
    }
    for (const page of pages) {
      this.#step({ kind: "texts", ...page });
    }
    this.#filterButtons(frame, look, layout, quadrant);
    this.#formulaReferences(frame, look, quadrant);
    this.#spillOutline(frame, look, quadrant);
    this.#clipboardOutline(frame, look, quadrant);
    this.#selectionOutlines(frame, look, quadrant);
    this.#diffHighlights(frame, quadrant);
    this.#remotePresence(frame, look, quadrant);
    this.#fillPreview(frame, look, quadrant);
  }

  /** The pages a quadrant shows; the ones that have not arrived are marked as such. */
  #pagesOf(
    frame: Frame,
    look: Look,
    layout: SheetLayout,
    setting: PageSetting,
    quadrant: Quadrant,
  ): Omit<PageStep, "kind">[] {
    const { clip, span, x, y } = quadrant;
    const pages: Omit<PageStep, "kind">[] = [];
    const lastPageRow = Math.floor((span.endRow - 1) / PAGE_ROWS);
    const lastPageCol = Math.floor((span.endCol - 1) / PAGE_COLS);
    for (let pageRow = Math.floor(span.firstRow / PAGE_ROWS); pageRow <= lastPageRow; pageRow++) {
      for (let pageCol = Math.floor(span.firstCol / PAGE_COLS); pageCol <= lastPageCol; pageCol++) {
        const firstRow = Math.max(span.firstRow, pageRow * PAGE_ROWS);
        const endRow = Math.min(span.endRow, (pageRow + 1) * PAGE_ROWS);
        const firstCol = Math.max(span.firstCol, pageCol * PAGE_COLS);
        const endCol = Math.min(span.endCol, (pageCol + 1) * PAGE_COLS);
        const page = frame.cache.get(pageRow, pageCol);
        if (!page) {
          this.#rects.add(clip, x(firstCol), y(firstRow), x(endCol), y(endRow), look.placeholder);
          this.#missing += (endRow - firstRow) * (endCol - firstCol);
          continue;
        }
        const held = this.#layouts.get(page, layout, setting);
        const first = firstRow - page.firstRow;
        const end = Math.min(page.rowCount, endRow - page.firstRow);
        if (first >= end) {
          continue;
        }
        this.#cells += held.cellsWithText(
          first,
          end,
          Math.min(page.colCount, firstCol - page.firstCol),
          Math.min(page.colCount, endCol - page.firstCol),
        );
        pages.push({
          layout: held,
          x: x(page.firstCol),
          y: y(page.firstRow),
          clip,
          firstRow: first,
          endRow: end,
        });
      }
    }
    return pages;
  }

  #selectionFills(frame: Frame, look: Look, quadrant: Quadrant): void {
    const ranges = rangesOf(frame);
    const several = ranges.length > 1;
    for (const range of ranges) {
      if (several || !isSingle(range)) {
        this.#fillSpan(quadrant, range, look.selectionFill);
      }
    }
    const { selection } = frame;
    if (
      selection &&
      (several || (frame.selectionRange && !isSingle(frame.selectionRange))) &&
      holds(quadrant.span, selection.row, selection.col)
    ) {
      // The active cell of a range looks the way it does unselected.
      const { row, col } = selection;
      this.#fillSpan(
        quadrant,
        { firstRow: row, endRow: row + 1, firstCol: col, endCol: col + 1 },
        this.#groundOf(frame, look, row, col),
      );
    }
  }

  /** What a cell is painted on. */
  #groundOf(frame: Frame, look: Look, row: number, col: number): PackedColor {
    const page = frame.cache.pageOf(row, col);
    if (!page) {
      return look.placeholder;
    }
    if (row >= page.firstRow + page.rowCount || col >= page.firstCol + page.colCount) {
      return look.background;
    }
    const cell = (row - page.firstRow) * page.colCount + (col - page.firstCol);
    const { fill } = this.#styles.get(page.styleIds[cell] ?? 0);
    return fill === NO_COLOR ? look.background : fill;
  }

  #fillSpan(quadrant: Quadrant, span: Span, color: PackedColor): void {
    const { span: shown, clip, x, y } = quadrant;
    const firstRow = Math.max(shown.firstRow, span.firstRow);
    const endRow = Math.min(shown.endRow, span.endRow);
    const firstCol = Math.max(shown.firstCol, span.firstCol);
    const endCol = Math.min(shown.endCol, span.endCol);
    if (firstRow < endRow && firstCol < endCol) {
      this.#rects.add(clip, x(firstCol), y(firstRow), x(endCol), y(endRow), color);
    }
  }

  #lines(look: Look, quadrant: Quadrant): void {
    const { span, clip, x, y } = quadrant;
    const right = Math.min(clip.right, x(span.endCol));
    const bottom = Math.min(clip.bottom, y(span.endRow));
    for (let row = span.firstRow + 1; row <= span.endRow; row++) {
      const at = y(row);
      this.#rects.add(clip, clip.left, at - look.hairline, right, at, look.line);
    }
    for (let col = span.firstCol + 1; col <= span.endCol; col++) {
      const at = x(col);
      this.#rects.add(clip, at - look.hairline, clip.top, at, bottom, look.line);
    }
  }

  #filterButtons(frame: Frame, look: Look, layout: SheetLayout, quadrant: Quadrant): void {
    const filter = frame.filterState;
    if (!filter) {
      return;
    }
    const row = frame.filterRowMap
      ? frame.filterRowMap.physicalToDisplay(filter.headerRow)
      : filter.headerRow;
    const { span, clip, x, y } = quadrant;
    if (row === null || row < span.firstRow || row >= span.endRow) {
      return;
    }
    const { scale } = frame;
    const first = Math.max(span.firstCol, filter.firstCol);
    const last = Math.min(span.endCol - 1, filter.lastCol);
    for (let col = first; col <= last; col++) {
      const button = filterButtonBox(0, 0, layout.colAxis.sizeOf(col), layout.rowAxis.sizeOf(row));
      if (!button) {
        continue;
      }
      const left = x(col) + Math.round(button.x * scale);
      const top = y(row) + Math.round(button.y * scale);
      const size = Math.round(button.size * scale);
      const active = filter.activeCols.includes(col);
      this.#rects.add(
        clip,
        left,
        top,
        left + size,
        top + size,
        active ? look.selectionFill : look.headerBackground,
      );
      addOutline(
        this.#rects,
        clip,
        left,
        top,
        left + size,
        top + size,
        look.hairline,
        active ? look.filterIconActive : look.headerLine,
      );
      this.#step({
        kind: "sprite",
        spec: {
          kind: active ? "funnel" : "chevron",
          width: size,
          height: size,
          scale,
          color: active ? look.filterIconActive : look.filterIcon,
        },
        x: left,
        y: top,
        clip,
      });
    }
  }

  #formulaReferences(frame: Frame, look: Look, quadrant: Quadrant): void {
    const { scale } = frame;
    const { clip, x, y } = quadrant;
    for (const reference of frame.formulaReferences ?? []) {
      const span = spanOf(reference);
      if (!overlaps(span, quadrant.span)) {
        continue;
      }
      const slot = reference.colorIndex & 7;
      const inset = look.hairline;
      this.#rects.add(
        clip,
        x(span.firstCol) + inset,
        y(span.firstRow) + inset,
        x(span.endCol) - inset,
        y(span.endRow) - inset,
        look.refFills[slot] ?? look.selectionFill,
      );
      this.#dashed(
        quadrant,
        span,
        0,
        lineWidth(1.5, scale),
        DASH,
        scale,
        look.refStrokes[slot] ?? look.selection,
      );
    }
  }

  #spillOutline(frame: Frame, look: Look, quadrant: Quadrant): void {
    if (frame.activeSpillRange) {
      const span = spanOf(frame.activeSpillRange);
      const { scale } = frame;
      this.#dashed(quadrant, span, 0, lineWidth(1.5, scale), DASH, scale, look.spillBorder);
    }
  }

  #clipboardOutline(frame: Frame, look: Look, quadrant: Quadrant): void {
    if (frame.clipboardRange) {
      const { span, cut } = frame.clipboardRange;
      const { scale } = frame;
      const dash = cut ? ([5, 3] as const) : ([3, 3] as const);
      this.#dashed(quadrant, span, 0, lineWidth(1.75, scale), dash, scale, look.selection);
    }
  }

  #fillPreview(frame: Frame, look: Look, quadrant: Quadrant): void {
    const preview = frame.fillPreview;
    if (preview && !isEmpty(preview)) {
      const { scale } = frame;
      const width = lineWidth(1.5, scale);
      this.#dashed(quadrant, preview, width, width, DASH, scale, look.selection);
    }
  }

  /** A dashed frame of some cells; `outset` moves its first row and column of pixels outwards. */
  #dashed(
    quadrant: Quadrant,
    span: Span,
    outset: number,
    width: number,
    dash: readonly [number, number],
    scale: number,
    color: PackedColor,
  ): void {
    if (overlaps(span, quadrant.span)) {
      const { clip, x, y } = quadrant;
      addDashedOutline(
        this.#rects,
        clip,
        x(span.firstCol) - outset,
        y(span.firstRow) - outset,
        x(span.endCol),
        y(span.endRow),
        width,
        lineWidth(dash[0], scale),
        lineWidth(dash[1], scale),
        color,
      );
    }
  }

  #outline(
    quadrant: Quadrant,
    span: Span,
    outset: number,
    width: number,
    color: PackedColor,
  ): void {
    if (overlaps(span, quadrant.span)) {
      const { clip, x, y } = quadrant;
      addOutline(
        this.#rects,
        clip,
        x(span.firstCol) - outset,
        y(span.firstRow) - outset,
        x(span.endCol),
        y(span.endRow),
        width,
        color,
      );
    }
  }

  #selectionOutlines(frame: Frame, look: Look, quadrant: Quadrant): void {
    const { selection, selectionRange, scale } = frame;
    const ranges = rangesOf(frame);
    // Over the grid lines around the cells, two of which are before the cells.
    const thin = lineWidth(1.5, scale);
    for (const range of ranges) {
      if (ranges.length > 1 || !isSingle(range)) {
        this.#outline(quadrant, range, thin, thin, look.selection);
      }
    }
    if (selection) {
      const { row, col } = selection;
      const thick = lineWidth(2, scale);
      const cell = { firstRow: row, endRow: row + 1, firstCol: col, endCol: col + 1 };
      this.#outline(quadrant, cell, thick, thick, look.selection);
    }
    if (
      frame.showFillHandle !== false &&
      selectionRange &&
      (!frame.selectionRanges || frame.selectionRanges.length <= 1) &&
      holds(quadrant.span, selectionRange.endRow - 1, selectionRange.endCol - 1)
    ) {
      const { clip, x, y } = quadrant;
      const size = lineWidth(FILL_HANDLE_SIZE, scale);
      const left = x(selectionRange.endCol) - Math.floor(size / 2);
      const top = y(selectionRange.endRow) - Math.floor(size / 2);
      const rim = look.hairline;
      this.#rects.add(
        clip,
        left - rim,
        top - rim,
        left + size + rim,
        top + size + rim,
        look.background,
      );
      this.#rects.add(clip, left, top, left + size, top + size, look.selection);
    }
  }

  #diffHighlights(frame: Frame, quadrant: Quadrant): void {
    const map = frame.diffHighlights;
    if (!map?.size) {
      return;
    }
    if (this.#diff?.map !== map || this.#diff.size !== map.size) {
      this.#diff = { map, size: map.size, cells: parseDiff(map) };
    }
    const { rows, cols, kinds } = this.#diff.cells;
    const { span, clip, x, y } = quadrant;
    const { scale } = frame;
    const end = lowerBound(rows, span.endRow);
    for (let index = lowerBound(rows, span.firstRow); index < end; index++) {
      const row = rows[index] ?? 0;
      const col = cols[index] ?? 0;
      if (col < span.firstCol || col >= span.endCol) {
        continue;
      }
      const kind = kinds[index];
      const [fill, stroke, weight] =
        kind === "conflict"
          ? ([FIXED.conflictFill, FIXED.conflict, 2] as const)
          : kind === "deleted"
            ? ([FIXED.deletedFill, FIXED.deleted, 1.5] as const)
            : ([FIXED.addedFill, FIXED.added, 1.5] as const);
      const left = x(col);
      const top = y(row);
      const right = x(col + 1);
      const bottom = y(row + 1);
      const width = lineWidth(weight, scale);
      this.#rects.add(clip, left, top, right, bottom, fill);
      addOutline(this.#rects, clip, left, top, right, bottom, width, stroke);
      if (kind === "deleted") {
        const inset = lineWidth(3, scale);
        const at = Math.round((top + bottom - width) / 2);
        this.#rects.add(clip, left + inset, at, right - inset, at + width, stroke);
      }
    }
  }

  #remotePresence(frame: Frame, look: Look, quadrant: Quadrant): void {
    const { scale } = frame;
    for (const overlay of frame.remotePresences ?? []) {
      const isAi = (overlay.colorSlot & 0x80) !== 0;
      const slot = overlay.colorSlot & 7;
      const color = isAi ? FIXED.ai : (look.refStrokes[slot] ?? look.selection);
      const frameOf = (span: Span, width: number) => {
        if (isAi) {
          this.#dashed(quadrant, span, 0, width, DASH, scale, color);
        } else {
          this.#outline(quadrant, span, 0, width, color);
        }
      };
      const range = overlay.selection;
      if (range && !isEmpty(range) && overlaps(range, quadrant.span)) {
        this.#fillSpan(
          quadrant,
          range,
          isAi ? FIXED.aiFill : (look.refFills[slot] ?? look.selectionFill),
        );
        frameOf(range, look.hairline);
      }
      const { cursorRow: row, cursorCol: col } = overlay;
      if (holds(quadrant.span, row, col)) {
        frameOf(
          { firstRow: row, endRow: row + 1, firstCol: col, endCol: col + 1 },
          lineWidth(2, scale),
        );
        this.#namePlate(look, quadrant, row, col, isAi ? "AI" : overlay.displayName, color, isAi);
      }
    }
  }

  #namePlate(
    look: Look,
    quadrant: Quadrant,
    row: number,
    col: number,
    displayName: string,
    stroke: PackedColor,
    isAi: boolean,
  ): void {
    const { scale } = look;
    const { clip, x, y } = quadrant;
    const pixels = (css: number) => Math.round(css * scale);
    const text = displayName.trim();
    const height = pixels(18);
    const padding = pixels(6);
    const rim = look.hairline;
    const clipWidth = clip.right - clip.left;
    if (!text || clipWidth < 2 * padding + pixels(8) || clip.bottom - clip.top < height + 2 * rim) {
      return;
    }
    const width = Math.min(
      pixels(140),
      clipWidth - 2 * rim,
      Math.ceil(this.measure.width(look.plateFont, text)) + 2 * padding,
    );
    const left = Math.max(clip.left + rim, Math.min(x(col), clip.right - width - rim));
    const over = y(row) - height;
    const preferred = over > clip.top ? over : y(row + 1);
    const top = Math.max(clip.top + rim, Math.min(preferred, clip.bottom - height - rim));
    this.#step({
      kind: "sprite",
      spec: {
        kind: "plate",
        width,
        height,
        scale,
        text,
        font: look.plateFont,
        padding,
        fill: isAi ? FIXED.ai : look.background,
        stroke,
        color: isAi ? FIXED.white : look.text,
      },
      x: left,
      y: top,
      clip,
    });
  }

  #headers(
    frame: Frame,
    look: Look,
    sheet: SheetSize,
    box: PixelBox,
    strips: readonly Strip[],
  ): void {
    const rects = this.#rects;
    const { left, top, right, bottom } = box;
    const { hairline } = look;
    const { selection, selectionRange } = frame;
    const ranges = rangesOf(frame);
    if (ranges.length === 0 && selection) {
      const { row, col } = selection;
      ranges.push({ firstRow: row, endRow: row + 1, firstCol: col, endCol: col + 1 });
    }

    rects.push(0, 0, right, top, look.headerBackground);
    rects.push(0, 0, left, bottom, look.headerBackground);

    for (const strip of strips) {
      const clip = { left: strip.left, top: 0, right: strip.right, bottom: top };
      if (isEmptyBox(clip) || strip.firstCol >= strip.endCol) {
        continue;
      }
      for (const range of ranges) {
        const first = Math.max(strip.firstCol, range.firstCol);
        const end = Math.min(strip.endCol, range.endCol);
        if (first < end) {
          rects.add(clip, strip.x(first), 0, strip.x(end), top, look.headerSelected);
        }
      }
      for (let col = strip.firstCol; col < strip.endCol; col++) {
        const from = strip.x(col);
        const to = strip.x(col + 1);
        this.#label(look, columnName(col), look.headerText, clip, from, 0, to, top);
      }
      for (let col = strip.firstCol + 1; col <= strip.endCol; col++) {
        const at = strip.x(col);
        rects.add(clip, at - hairline, 0, at, top, look.headerLine);
      }
    }

    const map = frame.filterRowMap ?? null;
    const filter = map?.hasHiddenRows ? (frame.filterState ?? null) : null;
    for (const strip of strips) {
      const clip = { left: 0, top: strip.top, right: left, bottom: strip.bottom };
      if (isEmptyBox(clip) || strip.firstRow >= strip.endRow) {
        continue;
      }
      for (const range of ranges) {
        const first = Math.max(strip.firstRow, range.firstRow);
        const end = Math.min(strip.endRow, range.endRow);
        if (first < end) {
          rects.add(clip, 0, strip.y(first), left, strip.y(end), look.headerSelected);
        }
      }
      for (let row = strip.firstRow; row < strip.endRow; row++) {
        const physical = map ? map.displayToPhysical(row) : row;
        const filtered =
          filter !== null &&
          map !== null &&
          ((physical > filter.headerRow && physical <= filter.lastRow) ||
            map.isGapBefore(row) ||
            map.isGapAfter(row));
        const color = filtered ? look.filterHeaderText : look.headerText;
        const from = strip.y(row);
        this.#label(look, String(physical + 1), color, clip, 0, from, left, strip.y(row + 1));
      }
      for (let row = strip.firstRow + 1; row <= strip.endRow; row++) {
        const at = strip.y(row);
        rects.add(clip, 0, at - hairline, left, at, look.headerLine);
      }
      if (filter !== null && map !== null) {
        const width = lineWidth(2, look.scale);
        const gap = (row: number) => {
          const at = strip.y(row) - Math.floor(width / 2);
          rects.add(clip, 0, at, left, at + width, look.filterHeaderText);
        };
        for (let row = strip.firstRow; row < strip.endRow; row++) {
          if (map.isGapBefore(row)) {
            gap(row);
          }
        }
        if (
          strip.endRow === sheet.rows &&
          map.totalRows > 0 &&
          map.physicalToDisplay(map.totalRows - 1) === null
        ) {
          gap(strip.endRow);
        }
      }
    }
    this.#step({ kind: "labels", runs: this.#labels });

    const allSelected =
      sheet.rows > 0 &&
      sheet.cols > 0 &&
      selectionRange?.firstRow === 0 &&
      selectionRange.endRow === sheet.rows &&
      selectionRange.firstCol === 0 &&
      selectionRange.endCol === sheet.cols;
    rects.push(0, 0, left, top, allSelected ? look.headerSelected : look.headerBackground);
    const size = lineWidth(8, look.scale);
    const inset = Math.round(5 * look.scale);
    this.#step({
      kind: "sprite",
      spec: { kind: "corner", width: size, height: size, color: look.headerLine },
      x: left - inset - size,
      y: top - inset - size,
      clip: { left: 0, top: 0, right: left, bottom: top },
    });
    rects.push(left - hairline, 0, hairline, bottom, look.headerLine);
    rects.push(0, top - hairline, right, hairline, look.headerLine);
  }

  /** A text in the middle of a header's cell, and nowhere else. */
  #label(
    look: Look,
    text: string,
    color: PackedColor,
    clip: PixelBox,
    left: number,
    top: number,
    right: number,
    bottom: number,
  ): void {
    // The last pixels of a header's cell are the line after it.
    const boxLeft = Math.max(clip.left, left);
    const boxTop = Math.max(clip.top, top);
    const boxRight = Math.min(clip.right, right - look.hairline);
    const boxBottom = Math.min(clip.bottom, bottom - look.hairline);
    if (boxLeft >= boxRight || boxTop >= boxBottom) {
      return;
    }
    const width = this.measure.width(look.fonts[0] ?? "", text);
    const x = (left + right - width) / 2;
    const lineHeight = look.lineHeights[0] ?? 0;
    const middle = (top + bottom) / 2;
    const baseline = Math.round(middle + (look.lifts[0] ?? 0));
    const run = this.#labels.push(text, x, width, baseline, 0, color, 0);
    if (
      x - 1 < boxLeft ||
      x + width + 1 > boxRight ||
      middle - lineHeight / 2 < boxTop ||
      middle + lineHeight / 2 > boxBottom
    ) {
      this.#labels.setClip(run, boxLeft, boxTop, boxRight, boxBottom);
    }
  }

  #dividers(
    look: Look,
    frozen: { readonly frozenRows: number; readonly frozenCols: number },
    clip: PixelBox,
    x: number,
    y: number,
  ): void {
    const width = lineWidth(2, look.scale);
    const half = Math.floor(width / 2);
    if (frozen.frozenRows > 0) {
      this.#rects.add(clip, clip.left, y - half, clip.right, y - half + width, look.freezeLine);
    }
    if (frozen.frozenCols > 0) {
      this.#rects.add(clip, x - half, clip.top, x - half + width, clip.bottom, look.freezeLine);
    }
  }

  #reorderGuide(
    frame: Frame,
    look: Look,
    sheet: SheetSize,
    frozen: { readonly frozenRows: number; readonly frozenCols: number },
    area: PixelBox,
    strips: readonly Strip[],
  ): void {
    const guide = frame.reorderGuide;
    const [pinned, scrolled] = strips;
    if (!guide || !pinned || !scrolled) {
      return;
    }
    const width = lineWidth(2.5, look.scale);
    const half = Math.floor(width / 2);
    const clip = { ...area, left: 0, top: 0 };
    if (guide.axis === "row") {
      const row = Math.min(sheet.rows, Math.max(0, guide.beforeRow));
      const at = (row <= frozen.frozenRows ? pinned : scrolled).y(row);
      if (at >= area.top && at <= area.bottom) {
        this.#rects.add(clip, 0, at - half, area.right, at - half + width, look.selection);
      }
    } else {
      const col = Math.min(sheet.cols, Math.max(0, guide.beforeCol));
      const at = (col <= frozen.frozenCols ? pinned : scrolled).x(col);
      if (at >= area.left && at <= area.right) {
        this.#rects.add(clip, at - half, 0, at - half + width, area.bottom, look.selection);
      }
    }
  }
}

/** The header cells of a quadrant on the diagonal: its rows on the left and its columns on top. */
type Strip = Span & PixelBox & Pick<Quadrant, "x" | "y">;

function rangesOf(frame: Frame): Span[] {
  if (frame.selectionRanges && frame.selectionRanges.length > 0) {
    return [...frame.selectionRanges];
  }
  return frame.selectionRange ? [frame.selectionRange] : [];
}

function spanOf(rect: {
  readonly firstRow: number;
  readonly lastRow: number;
  readonly firstCol: number;
  readonly lastCol: number;
}): Span {
  return {
    firstRow: rect.firstRow,
    endRow: rect.lastRow + 1,
    firstCol: rect.firstCol,
    endCol: rect.lastCol + 1,
  };
}

/** Bands of a page's text that a step shows. */
export function bandsOf(step: PageStep): { readonly first: number; readonly end: number } {
  return {
    first: Math.floor(step.firstRow / BAND_ROWS),
    end: Math.ceil(step.endRow / BAND_ROWS),
  };
}
