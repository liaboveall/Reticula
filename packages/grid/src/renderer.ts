import type { RemotePresence, WireCellStyle } from "@reticula/protocol";
import { PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import type { PageCache } from "./cache.ts";
import {
  type CellRect,
  type FilterDisplayState,
  type FilterRowMap,
  filterButtonBox,
} from "./filter.ts";
import {
  type Box,
  columnName,
  defaultLayout,
  frozenPixels,
  isEmpty,
  type Metrics,
  type Scroll,
  type SheetLayout,
  type SheetSize,
  type Size,
  type Span,
} from "./layout.ts";
import { isBool, isError, isNumber } from "./page.ts";
import { rgbToHex, type Theme } from "./theme.ts";

export type GridCellStyle = WireCellStyle;

/** Colored formula reference highlight overlay (`0..7`). */
export interface FormulaReferenceOverlay {
  readonly colorIndex: number;
  readonly firstRow: number;
  readonly lastRow: number;
  readonly firstCol: number;
  readonly lastCol: number;
}

/** Three-state diff overlay cell highlight rendered during AI branch review. */
export type DiffHighlightKind = "added" | "conflict" | "deleted";
export type DiffHighlightMap = ReadonlyMap<string, DiffHighlightKind>;

/** A collaborator's cursor and selection in displayed positions on the active sheet. */
export type RemotePresenceOverlay = Readonly<Omit<RemotePresence, "selection">> & {
  readonly selection?: RemotePresence["selection"] | null;
};

/** Active row or column drag-reorder drop indicator. */
export type ReorderGuide =
  | { readonly axis: "row"; readonly beforeRow: number }
  | { readonly axis: "col"; readonly beforeCol: number };

/** Everything one frame is drawn from. */
export interface Frame {
  /** Size of the grid's area in CSS pixels. */
  readonly size: Size;
  /** Device pixels per CSS pixel. */
  readonly scale: number;
  readonly metrics: Metrics;
  readonly theme: Theme;
  /** Scroll position, already aligned to device pixels. */
  readonly scroll: Scroll;
  /** The scrollable cells at least partly in view. */
  readonly visible: Span;
  readonly cache: PageCache;
  readonly selection: { readonly row: number; readonly col: number } | null;
  /** Primary rectangular selection span (`firstRow..endRow`, `firstCol..endCol`). */
  readonly selectionRange?: Span | null;
  /** All selected spans when Ctrl multi-selection is active (includes `selectionRange`). */
  readonly selectionRanges?: readonly Span[];
  /** Sheet dimensions (optional for standalone microbenchmarks). */
  readonly sheet?: SheetSize;
  /** Sparse row/col sizes and freeze counts. */
  readonly layout?: SheetLayout;
  /** Sheet style table indexed by `styleId`. */
  readonly styles?: readonly GridCellStyle[];
  /** Colored formula reference rectangles on the active sheet. */
  readonly formulaReferences?: readonly FormulaReferenceOverlay[];
  /** Remote collaborators on the active sheet. */
  readonly remotePresences?: readonly RemotePresenceOverlay[];
  /** Optional three-state diff highlights keyed by `"row:col"`. */
  readonly diffHighlights?: DiffHighlightMap | null;
  /** Dashed preview span while dragging the fill handle. */
  readonly fillPreview?: Span | null;
  /** Copied/cut source span (drawn with dashed border when active). */
  readonly clipboardRange?: { readonly span: Span; readonly cut: boolean } | null;
  /** Blue insertion line while dragging selected rows/columns to reorder. */
  readonly reorderGuide?: ReorderGuide | null;
  /** Whether to draw the `6×6` fill handle at the bottom-right of `selectionRange`. */
  readonly showFillHandle?: boolean;
  /** Active worker-local AutoFilter display bounds and active columns. */
  readonly filterState?: FilterDisplayState | null;
  /** O(log H) display-to-physical row map for AutoFilter header labels & gaps. */
  readonly filterRowMap?: FilterRowMap | null;
  /** Active dynamic-array spill footprint displayed with a dashed border. */
  readonly activeSpillRange?: CellRect | null;
}

export interface FrameCounts {
  /** Cells in view whose text was drawn. */
  cells: number;
  /** Of those, the cells whose text was drawn through a clip. */
  clipped: number;
  /** Cells in view whose page has not arrived; they are shown as placeholders. */
  missing: number;
}

export interface Shortcuts {
  readonly cacheWidths: boolean;
  readonly clipOnlyOverwide: boolean;
  readonly batchState: boolean;
}

export const ALL_SHORTCUTS: Shortcuts = {
  cacheWidths: true,
  clipOnlyOverwide: true,
  batchState: true,
};

/** Space between a cell's edge and its text. */
const PADDING = 6;
/** Fill handle square side in CSS pixels (`14-m1b.md` 6.3). */
export const FILL_HANDLE_SIZE = 6;

class TextWidths {
  #shortcuts: Shortcuts;
  #font = "";
  #widths = new Map<string, number>();

  constructor(shortcuts: Shortcuts) {
    this.#shortcuts = shortcuts;
  }

  use(font: string): void {
    if (font === this.#font) {
      return;
    }
    this.#font = font;
    this.#widths.clear();
  }

  measure(context: CanvasRenderingContext2D, text: string): number {
    if (!this.#shortcuts.cacheWidths) {
      return context.measureText(text).width;
    }
    let width = this.#widths.get(text);
    if (width === undefined) {
      width = context.measureText(text).width;
      if (this.#widths.size >= 8192) {
        this.#widths.clear();
      }
      this.#widths.set(text, width);
    }
    return width;
  }

  fits(context: CanvasRenderingContext2D, text: string, room: number): boolean {
    return this.measure(context, text) <= room;
  }
}

interface Part {
  readonly pageRow: number;
  readonly pageCol: number;
  readonly firstRow: number;
  readonly endRow: number;
  readonly firstCol: number;
  readonly endCol: number;
}

interface Quadrant {
  readonly clip: Box;
  readonly span: Span;
  readonly scrollTop: number;
  readonly scrollLeft: number;
}

function partsOf(visible: Span): Part[] {
  if (isEmpty(visible)) {
    return [];
  }
  const parts: Part[] = [];
  const lastPageRow = Math.floor((visible.endRow - 1) / PAGE_ROWS);
  const lastPageCol = Math.floor((visible.endCol - 1) / PAGE_COLS);
  for (let pageRow = Math.floor(visible.firstRow / PAGE_ROWS); pageRow <= lastPageRow; pageRow++) {
    for (
      let pageCol = Math.floor(visible.firstCol / PAGE_COLS);
      pageCol <= lastPageCol;
      pageCol++
    ) {
      parts.push({
        pageRow,
        pageCol,
        firstRow: Math.max(visible.firstRow, pageRow * PAGE_ROWS),
        endRow: Math.min(visible.endRow, (pageRow + 1) * PAGE_ROWS),
        firstCol: Math.max(visible.firstCol, pageCol * PAGE_COLS),
        endCol: Math.min(visible.endCol, (pageCol + 1) * PAGE_COLS),
      });
    }
  }
  return parts;
}

function resolveColor(
  color:
    | { readonly kind: "palette"; readonly slot: number }
    | { readonly kind: "rgb"; readonly rgb: number }
    | null
    | undefined,
  palette: readonly string[],
): string | null {
  if (!color) {
    return null;
  }
  if (color.kind === "rgb") {
    return rgbToHex(color.rgb);
  }
  return palette[color.slot & 7] ?? null;
}

/** Draws frames onto one canvas. */
export class Renderer {
  #context: CanvasRenderingContext2D;
  #shortcuts: Shortcuts;
  #widths: TextWidths;

  constructor(canvas: HTMLCanvasElement, shortcuts: Shortcuts = ALL_SHORTCUTS) {
    const context = canvas.getContext("2d", { alpha: false });
    if (!context) {
      throw new Error("this browser cannot draw on a 2D canvas");
    }
    this.#context = context;
    this.#shortcuts = shortcuts;
    this.#widths = new TextWidths(shortcuts);
  }

  /** Measures the widest cached text in `col` across `rows` for double-click auto-fit. */
  measureColumnFitWidth(
    theme: Theme,
    cache: PageCache,
    styles: readonly GridCellStyle[] | undefined,
    col: number,
    firstRow: number,
    endRow: number,
  ): number {
    const ctx = this.#context;
    let maxW = 48;
    const headerLabel = columnName(col);
    ctx.font = `600 ${theme.fontSize}px ${theme.fontFamily}`;
    maxW = Math.max(maxW, Math.ceil(ctx.measureText(headerLabel).width + 2 * PADDING + 12));

    for (let r = firstRow; r < endRow; r++) {
      const page = cache.pageOf(r, col);
      if (!page || r >= page.firstRow + page.rowCount || col >= page.firstCol + page.colCount) {
        continue;
      }
      const idx = (r - page.firstRow) * page.colCount + (col - page.firstCol);
      const text = page.texts[idx];
      if (!text) {
        continue;
      }
      const styleId = page.styleIds[idx] ?? 0;
      const st = styleId > 0 ? styles?.[styleId] : undefined;
      ctx.font = fontForStyle(theme, st);
      const w = Math.ceil(ctx.measureText(text).width + 2 * PADDING + 6);
      if (w > maxW) {
        maxW = Math.min(600, w);
      }
    }
    return maxW;
  }

  draw(frame: Frame): FrameCounts {
    const context = this.#context;
    const { size, scale, metrics, theme, scroll, visible } = frame;
    const counts: FrameCounts = { cells: 0, clipped: 0, missing: 0 };

    context.setTransform(scale, 0, 0, scale, 0, 0);
    context.fillStyle = theme.background;
    context.fillRect(0, 0, size.width, size.height);

    const layout = frame.layout ?? defaultLayout(metrics);
    const sheet: SheetSize = frame.sheet ?? {
      rows: Math.max(visible.endRow, layout.frozenRows),
      cols: Math.max(visible.endCol, layout.frozenCols),
    };
    const fz = frozenPixels(metrics, sheet, size, layout);
    const right = size.width - metrics.scrollbarSize;
    const bottom = size.height - metrics.scrollbarSize;

    const quadrants = buildQuadrants(metrics, size, scroll, visible, fz);
    for (const q of quadrants) {
      if (q.clip.width <= 0 || q.clip.height <= 0 || isEmpty(q.span)) {
        continue;
      }
      const needOuterClip = quadrants.length > 1;
      if (needOuterClip) {
        context.save();
        context.beginPath();
        context.rect(q.clip.left, q.clip.top, q.clip.width, q.clip.height);
        context.clip();
      }

      const originX = metrics.headerWidth - q.scrollLeft;
      const originY = metrics.headerHeight - q.scrollTop;
      const parts = partsOf(q.span);

      this.#placeholders(frame, layout, parts, originX, originY, counts);
      this.#cellBackgrounds(frame, layout, parts, originX, originY);
      this.#selectionFills(frame, layout, q, originX, originY);
      this.#lines(frame, layout, q, originX, originY);
      this.#cellBorders(frame, layout, parts, originX, originY);
      this.#textAndOverflows(frame, layout, sheet, q, parts, originX, originY, counts);
      this.#filterDropdownButtons(frame, layout, q, originX, originY);
      this.#formulaRefs(frame, layout, q, originX, originY);
      this.#spillBorderOutline(frame, layout, q, originX, originY);
      this.#clipboardOutline(frame, layout, q, originX, originY);
      this.#selectionOutlines(frame, layout, q, originX, originY);
      if (frame.diffHighlights?.size) {
        this.#diffHighlights(frame.diffHighlights, layout, q, originX, originY);
      }
      if (frame.remotePresences?.length) {
        this.#remotePresence(frame.remotePresences, theme, layout, q, originX, originY);
      }
      this.#fillPreviewOutline(frame, layout, q, originX, originY);

      if (needOuterClip) {
        context.restore();
      }
    }

    this.#headers(frame, layout, sheet, fz, right, bottom);
    this.#freezeDividers(frame, fz, right, bottom);
    this.#reorderGuide(frame, layout, sheet, fz, right, bottom);
    return counts;
  }

  #diffHighlights(
    highlights: DiffHighlightMap,
    layout: SheetLayout,
    quadrant: Quadrant,
    originX: number,
    originY: number,
  ): void {
    const { clip, span } = quadrant;
    const { rowAxis, colAxis } = layout;
    const context = this.#context;
    context.save();
    context.beginPath();
    context.rect(clip.left, clip.top, clip.width, clip.height);
    context.clip();
    for (const [key, kind] of highlights) {
      const sep = key.indexOf(":");
      if (sep <= 0) continue;
      const row = Number(key.slice(0, sep));
      const col = Number(key.slice(sep + 1));
      if (row < span.firstRow || row >= span.endRow || col < span.firstCol || col >= span.endCol) {
        continue;
      }
      const x = originX + colAxis.offsetOf(col);
      const y = originY + rowAxis.offsetOf(row);
      const w = colAxis.sizeOf(col);
      const h = rowAxis.sizeOf(row);
      if (kind === "conflict") {
        context.fillStyle = "rgba(245,158,11,0.22)";
        context.strokeStyle = "#d97706";
        context.lineWidth = 2;
      } else if (kind === "deleted") {
        context.fillStyle = "rgba(239,68,68,0.12)";
        context.strokeStyle = "#dc2626";
        context.lineWidth = 1.5;
      } else {
        context.fillStyle = "rgba(34,197,94,0.10)";
        context.strokeStyle = "#16a34a";
        context.lineWidth = 1.5;
      }
      context.fillRect(x, y, w, h);
      context.strokeRect(x + 1, y + 1, Math.max(0, w - 2), Math.max(0, h - 2));
      if (kind === "deleted") {
        context.beginPath();
        context.moveTo(x + 3, y + h / 2);
        context.lineTo(x + w - 3, y + h / 2);
        context.stroke();
      }
    }
    context.restore();
  }

  #remotePresence(
    overlays: readonly RemotePresenceOverlay[],
    theme: Theme,
    layout: SheetLayout,
    quadrant: Quadrant,
    originX: number,
    originY: number,
  ): void {
    const { clip, span } = quadrant;
    const { rowAxis, colAxis } = layout;
    const context = this.#context;
    context.save();
    context.beginPath();
    context.rect(clip.left, clip.top, clip.width, clip.height);
    context.clip();

    for (const overlay of overlays) {
      const isAi = (overlay.colorSlot & 0x80) !== 0;
      const slot = overlay.colorSlot & 7;
      const color = isAi ? "#7c3aed" : (theme.refStrokes[slot] ?? theme.selection);
      const range = overlay.selection;
      context.setLineDash(isAi ? [4, 3] : []);
      if (
        range &&
        !isEmpty(range) &&
        range.endRow > span.firstRow &&
        range.firstRow < span.endRow &&
        range.endCol > span.firstCol &&
        range.firstCol < span.endCol
      ) {
        const x = originX + colAxis.offsetOf(range.firstCol);
        const y = originY + rowAxis.offsetOf(range.firstRow);
        const width = originX + colAxis.offsetOf(range.endCol) - x;
        const height = originY + rowAxis.offsetOf(range.endRow) - y;
        context.fillStyle = isAi
          ? "rgba(124,58,237,0.12)"
          : (theme.refFills[slot] ?? theme.selectionFill);
        context.fillRect(x, y, width, height);
        context.strokeStyle = color;
        context.lineWidth = 1;
        context.strokeRect(x + 0.5, y + 0.5, Math.max(0, width - 1), Math.max(0, height - 1));
      }
      const { cursorRow: row, cursorCol: col } = overlay;
      if (row < span.firstRow || row >= span.endRow || col < span.firstCol || col >= span.endCol) {
        continue;
      }
      const x = originX + colAxis.offsetOf(col);
      const y = originY + rowAxis.offsetOf(row);
      const width = colAxis.sizeOf(col);
      const height = rowAxis.sizeOf(row);
      context.strokeStyle = color;
      context.lineWidth = 2;
      context.strokeRect(x + 1, y + 1, Math.max(0, width - 2), Math.max(0, height - 2));
      context.setLineDash([]);
      this.#drawPresenceName(theme, clip, x, y, height, isAi ? "AI" : overlay.displayName, isAi);
    }
    context.restore();
  }

  #drawPresenceName(
    theme: Theme,
    clip: Box,
    cursorX: number,
    cursorY: number,
    cursorHeight: number,
    displayName: string,
    isAi = false,
  ): void {
    const height = 18;
    const pad = 6;
    const name = displayName.trim();
    if (!name || clip.width < 2 * pad + 8 || clip.height < height + 2) {
      return;
    }
    const context = this.#context;
    context.font = `600 11px ${theme.fontFamily}`;
    context.textAlign = "left";
    context.textBaseline = "middle";
    const width = Math.min(
      140,
      clip.width - 2,
      Math.ceil(context.measureText(name).width) + 2 * pad,
    );
    const left = Math.max(clip.left + 1, Math.min(cursorX, clip.left + clip.width - width - 1));
    const above = cursorY - height;
    const preferredY = above > clip.top ? above : cursorY + cursorHeight;
    const top = Math.max(clip.top + 1, Math.min(preferredY, clip.top + clip.height - height - 1));
    context.beginPath();
    context.roundRect(left, top, width, height, 5);
    context.fillStyle = isAi ? "#7c3aed" : theme.background;
    context.fill();
    context.lineWidth = 1;
    context.stroke();
    context.fillStyle = isAi ? "#ffffff" : theme.text;
    context.fillText(name, left + pad, top + height / 2, width - 2 * pad);
  }

  #placeholders(
    frame: Frame,
    layout: SheetLayout,
    parts: readonly Part[],
    originX: number,
    originY: number,
    counts: FrameCounts,
  ): void {
    const context = this.#context;
    context.fillStyle = frame.theme.placeholder;
    for (const part of parts) {
      if (frame.cache.get(part.pageRow, part.pageCol)) {
        continue;
      }
      const x0 = originX + layout.colAxis.offsetOf(part.firstCol);
      const x1 = originX + layout.colAxis.offsetOf(part.endCol);
      const y0 = originY + layout.rowAxis.offsetOf(part.firstRow);
      const y1 = originY + layout.rowAxis.offsetOf(part.endRow);
      context.fillRect(x0, y0, x1 - x0, y1 - y0);
      counts.missing += (part.endRow - part.firstRow) * (part.endCol - part.firstCol);
    }
  }

  #cellBackgrounds(
    frame: Frame,
    layout: SheetLayout,
    parts: readonly Part[],
    originX: number,
    originY: number,
  ): void {
    const styles = frame.styles;
    if (!styles || styles.length <= 1) {
      return;
    }
    const context = this.#context;
    const palette = frame.theme.paletteFill;
    let currentFill = "";

    for (const part of parts) {
      const page = frame.cache.get(part.pageRow, part.pageCol);
      if (!page) {
        continue;
      }
      for (let col = part.firstCol; col < part.endCol; col++) {
        const x = originX + layout.colAxis.offsetOf(col);
        const w = layout.colAxis.sizeOf(col);
        for (let row = part.firstRow; row < part.endRow; row++) {
          const cellIndex: number = (row - page.firstRow) * page.colCount + (col - page.firstCol);
          const styleId = page.styleIds[cellIndex] ?? 0;
          if (styleId === 0) {
            continue;
          }
          const fill = resolveColor(styles[styleId]?.fillColor, palette);
          if (!fill) {
            continue;
          }
          if (fill !== currentFill) {
            context.fillStyle = fill;
            currentFill = fill;
          }
          const y = originY + layout.rowAxis.offsetOf(row);
          const h = layout.rowAxis.sizeOf(row);
          context.fillRect(x, y, w, h);
        }
      }
    }
  }

  #selectionFills(
    frame: Frame,
    layout: SheetLayout,
    q: Quadrant,
    originX: number,
    originY: number,
  ): void {
    const ranges =
      frame.selectionRanges && frame.selectionRanges.length > 0
        ? frame.selectionRanges
        : frame.selectionRange
          ? [frame.selectionRange]
          : [];
    if (ranges.length === 0) {
      return;
    }
    const context = this.#context;
    const { theme, selection } = frame;
    const clipRight = q.clip.left + q.clip.width;
    const clipBottom = q.clip.top + q.clip.height;

    for (const range of ranges) {
      const multi =
        ranges.length > 1 || range.endRow - range.firstRow > 1 || range.endCol - range.firstCol > 1;
      if (!multi) {
        continue;
      }
      const firstRow = Math.max(q.span.firstRow, range.firstRow);
      const endRow = Math.min(q.span.endRow, range.endRow);
      const firstCol = Math.max(q.span.firstCol, range.firstCol);
      const endCol = Math.min(q.span.endCol, range.endCol);
      if (firstRow >= endRow || firstCol >= endCol) {
        continue;
      }
      const x0 = Math.max(q.clip.left, originX + layout.colAxis.offsetOf(firstCol));
      const y0 = Math.max(q.clip.top, originY + layout.rowAxis.offsetOf(firstRow));
      const x1 = Math.min(clipRight, originX + layout.colAxis.offsetOf(endCol));
      const y1 = Math.min(clipBottom, originY + layout.rowAxis.offsetOf(endRow));
      if (x1 > x0 && y1 > y0) {
        context.fillStyle = theme.selectionFill;
        context.fillRect(x0, y0, x1 - x0, y1 - y0);
      }
    }

    if (
      selection &&
      (ranges.length > 1 ||
        (frame.selectionRange &&
          (frame.selectionRange.endRow - frame.selectionRange.firstRow > 1 ||
            frame.selectionRange.endCol - frame.selectionRange.firstCol > 1))) &&
      selection.row >= q.span.firstRow &&
      selection.row < q.span.endRow &&
      selection.col >= q.span.firstCol &&
      selection.col < q.span.endCol
    ) {
      const ax0 = Math.max(q.clip.left, originX + layout.colAxis.offsetOf(selection.col));
      const ay0 = Math.max(q.clip.top, originY + layout.rowAxis.offsetOf(selection.row));
      const ax1 = Math.min(clipRight, originX + layout.colAxis.offsetOf(selection.col + 1));
      const ay1 = Math.min(clipBottom, originY + layout.rowAxis.offsetOf(selection.row + 1));
      if (ax1 > ax0 && ay1 > ay0) {
        // Restore either the cell's own background fill or sheet background on active anchor cell.
        let activeBg = theme.background;
        const page = frame.cache.pageOf(selection.row, selection.col);
        if (
          page &&
          frame.styles &&
          selection.row < page.firstRow + page.rowCount &&
          selection.col < page.firstCol + page.colCount
        ) {
          const idx =
            (selection.row - page.firstRow) * page.colCount + (selection.col - page.firstCol);
          const sid = page.styleIds[idx] ?? 0;
          const resolved =
            sid > 0 ? resolveColor(frame.styles[sid]?.fillColor, theme.paletteFill) : null;
          if (resolved) {
            activeBg = resolved;
          }
        }
        context.fillStyle = activeBg;
        context.fillRect(ax0, ay0, ax1 - ax0, ay1 - ay0);
      }
    }
  }

  #lines(frame: Frame, layout: SheetLayout, q: Quadrant, originX: number, originY: number): void {
    const context = this.#context;
    const { theme, scale } = frame;
    const width = Math.max(1, Math.round(scale)) / scale;
    const snap = (position: number) => Math.round(position * scale) / scale - width / 2;

    const clipRight = q.clip.left + q.clip.width;
    const clipBottom = q.clip.top + q.clip.height;
    const endX = Math.min(clipRight, originX + layout.colAxis.offsetOf(q.span.endCol));
    const endY = Math.min(clipBottom, originY + layout.rowAxis.offsetOf(q.span.endRow));

    context.beginPath();
    for (let row = q.span.firstRow + 1; row <= q.span.endRow; row++) {
      const y = snap(originY + layout.rowAxis.offsetOf(row));
      context.moveTo(q.clip.left, y);
      context.lineTo(endX, y);
    }
    for (let col = q.span.firstCol + 1; col <= q.span.endCol; col++) {
      const x = snap(originX + layout.colAxis.offsetOf(col));
      context.moveTo(x, q.clip.top);
      context.lineTo(x, endY);
    }
    context.lineWidth = width;
    context.strokeStyle = theme.line;
    context.stroke();
  }

  #cellBorders(
    frame: Frame,
    layout: SheetLayout,
    parts: readonly Part[],
    originX: number,
    originY: number,
  ): void {
    const styles = frame.styles;
    if (!styles || styles.length <= 1) {
      return;
    }
    const context = this.#context;
    const { theme, scale } = frame;
    const thinPx = Math.max(1, Math.round(scale)) / scale;
    const snap = (p: number) => Math.round(p * scale) / scale - thinPx / 2;

    let hasThin = false;
    let hasMed = false;
    const thinSegs: number[] = [];
    const medSegs: number[] = [];

    const pushSeg = (
      weight: "none" | "thin" | "medium",
      x1: number,
      y1: number,
      x2: number,
      y2: number,
    ) => {
      if (weight === "thin") {
        hasThin = true;
        thinSegs.push(x1, y1, x2, y2);
      } else if (weight === "medium") {
        hasMed = true;
        medSegs.push(x1, y1, x2, y2);
      }
    };

    for (const part of parts) {
      const page = frame.cache.get(part.pageRow, part.pageCol);
      if (!page) {
        continue;
      }
      for (let col = part.firstCol; col < part.endCol; col++) {
        const x0 = originX + layout.colAxis.offsetOf(col);
        const x1 = x0 + layout.colAxis.sizeOf(col);
        for (let row = part.firstRow; row < part.endRow; row++) {
          const cell = (row - page.firstRow) * page.colCount + (col - page.firstCol);
          const sid = page.styleIds[cell] ?? 0;
          if (sid === 0) {
            continue;
          }
          const st = styles[sid];
          if (!st) {
            continue;
          }
          if (
            st.borderTop === "none" &&
            st.borderRight === "none" &&
            st.borderBottom === "none" &&
            st.borderLeft === "none"
          ) {
            continue;
          }
          const y0 = originY + layout.rowAxis.offsetOf(row);
          const y1 = y0 + layout.rowAxis.sizeOf(row);
          pushSeg(st.borderTop, x0, snap(y0), x1, snap(y0));
          pushSeg(st.borderBottom, x0, snap(y1), x1, snap(y1));
          pushSeg(st.borderLeft, snap(x0), y0, snap(x0), y1);
          pushSeg(st.borderRight, snap(x1), y0, snap(x1), y1);
        }
      }
    }

    if (hasThin) {
      context.beginPath();
      for (let i = 0; i < thinSegs.length; i += 4) {
        context.moveTo(thinSegs[i] ?? 0, thinSegs[i + 1] ?? 0);
        context.lineTo(thinSegs[i + 2] ?? 0, thinSegs[i + 3] ?? 0);
      }
      context.lineWidth = 1;
      context.strokeStyle = theme.text;
      context.stroke();
    }
    if (hasMed) {
      context.beginPath();
      for (let i = 0; i < medSegs.length; i += 4) {
        context.moveTo(medSegs[i] ?? 0, medSegs[i + 1] ?? 0);
        context.lineTo(medSegs[i + 2] ?? 0, medSegs[i + 3] ?? 0);
      }
      context.lineWidth = 2;
      context.strokeStyle = theme.text;
      context.stroke();
    }
  }

  #textAndOverflows(
    frame: Frame,
    layout: SheetLayout,
    sheet: SheetSize,
    q: Quadrant,
    parts: readonly Part[],
    originX: number,
    originY: number,
    counts: FrameCounts,
  ): void {
    const context = this.#context;
    const { theme, cache, styles } = frame;
    const { clipOnlyOverwide, batchState } = this.#shortcuts;

    let currentFont = theme.font;
    context.font = currentFont;
    context.textBaseline = "middle";
    let currentFill = theme.text;
    context.fillStyle = currentFill;
    this.#widths.use(currentFont);
    let aligned: CanvasTextAlign | null = null;

    const isCellOccupied = (r: number, c: number): boolean => {
      const p = cache.pageOf(r, c);
      if (!p || r >= p.firstRow + p.rowCount || c >= p.firstCol + p.colCount) {
        return false;
      }
      return (p.texts[(r - p.firstRow) * p.colCount + (c - p.firstCol)] ?? "") !== "";
    };

    for (const part of parts) {
      const page = cache.get(part.pageRow, part.pageCol);
      if (!page) {
        continue;
      }

      // 1. Cross-page left-overflow candidates (`14-m1b.md` 6.2) entering `part.firstCol`:
      if (part.firstCol === page.firstCol && page.overflows.length > 0) {
        for (const ov of page.overflows) {
          const row = page.firstRow + ov.rowOffset;
          if (row < part.firstRow || row >= part.endRow || ov.originCol >= q.span.firstCol) {
            continue;
          }
          // Only enters if `(row, part.firstCol)` itself is empty!
          if (isCellOccupied(row, part.firstCol)) {
            continue;
          }
          const st = ov.styleId > 0 ? styles?.[ov.styleId] : undefined;
          const font = fontForStyle(theme, st);
          if (currentFont !== font) {
            context.font = font;
            currentFont = font;
            this.#widths.use(font);
          }
          const originStart = originX + layout.colAxis.offsetOf(ov.originCol);
          const textW = this.#widths.measure(context, ov.text);
          const pageEdgeX = originX + layout.colAxis.offsetOf(part.firstCol);
          if (originStart + PADDING + textW <= pageEdgeX) {
            continue;
          }
          // Find how many empty columns to the right `ov.text` can spill across.
          let stopCol = part.firstCol + 1;
          const maxLookRight = Math.min(sheet.cols, part.endCol + 8);
          while (
            stopCol < maxLookRight &&
            originX + layout.colAxis.offsetOf(stopCol) < originStart + PADDING + textW &&
            !isCellOccupied(row, stopCol)
          ) {
            stopCol++;
          }
          const stopX = originX + layout.colAxis.offsetOf(stopCol);
          const clipStart = Math.max(q.clip.left, pageEdgeX);
          const clipEnd = Math.min(q.clip.left + q.clip.width, stopX - PADDING);
          if (clipEnd <= clipStart) {
            continue;
          }
          const fill = resolveColor(st?.textColor, theme.paletteText) ?? theme.text;
          if (currentFill !== fill) {
            context.fillStyle = fill;
            currentFill = fill;
          }
          if (aligned !== "left") {
            context.textAlign = "left";
            aligned = "left";
          }
          const y = originY + layout.rowAxis.offsetOf(row);
          const h = layout.rowAxis.sizeOf(row);
          context.save();
          context.beginPath();
          context.rect(clipStart, y, clipEnd - clipStart, h);
          context.clip();
          context.fillText(ov.text, originStart + PADDING, y + h / 2);
          context.restore();
          counts.clipped++;
        }
      }

      // 2. Cells inside `part` (column by column for canvas state locality).
      for (let col = part.firstCol; col < part.endCol; col++) {
        const x = originX + layout.colAxis.offsetOf(col);
        const colW = layout.colAxis.sizeOf(col);
        const baseRoom = colW - 2 * PADDING;
        if (baseRoom <= 0) {
          continue;
        }
        for (let row = part.firstRow; row < part.endRow; row++) {
          const cell = (row - page.firstRow) * page.colCount + (col - page.firstCol);
          const text = page.texts[cell];
          if (!text) {
            continue;
          }
          const flags = page.flags[cell] ?? 0;
          const styleId = page.styleIds[cell] ?? 0;
          const st = styleId > 0 ? styles?.[styleId] : undefined;

          const number = isNumber(flags);
          const error = isError(flags);
          const centered = error || isBool(flags);

          const font = fontForStyle(theme, st);
          const align: CanvasTextAlign =
            st && st.align !== "auto" ? st.align : number ? "right" : centered ? "center" : "left";
          const fill = error
            ? theme.errorText
            : (resolveColor(st?.textColor, theme.paletteText) ?? theme.text);

          if (!batchState) {
            context.font = font;
            currentFont = font;
            this.#widths.use(font);
            context.textBaseline = "middle";
            context.fillStyle = fill;
            currentFill = fill;
            context.textAlign = align;
            aligned = align;
          } else {
            if (currentFont !== font) {
              context.font = font;
              currentFont = font;
              this.#widths.use(font);
            }
            if (aligned !== align) {
              context.textAlign = align;
              aligned = align;
            }
            if (currentFill !== fill) {
              context.fillStyle = fill;
              currentFill = fill;
            }
          }

          const y = originY + layout.rowAxis.offsetOf(row);
          const rowH = layout.rowAxis.sizeOf(row);
          const midY = y + rowH / 2;
          const textX =
            align === "right"
              ? x + colW - PADDING
              : align === "center"
                ? x + colW / 2
                : x + PADDING;

          // Expand available room into rightward empty neighbors for left-aligned text!
          let effectiveRoom = baseRoom;
          if (
            align === "left" &&
            !number &&
            !centered &&
            !this.#widths.fits(context, text, baseRoom)
          ) {
            const neededW = this.#widths.measure(context, text);
            let stopCol = col + 1;
            const maxCol = Math.min(sheet.cols, q.span.endCol + 8);
            while (
              stopCol < maxCol &&
              layout.colAxis.offsetOf(stopCol) - layout.colAxis.offsetOf(col) - 2 * PADDING <
                neededW &&
              !isCellOccupied(row, stopCol)
            ) {
              stopCol++;
            }
            effectiveRoom =
              layout.colAxis.offsetOf(stopCol) - layout.colAxis.offsetOf(col) - 2 * PADDING;
          }

          if (clipOnlyOverwide && this.#widths.fits(context, text, effectiveRoom)) {
            context.fillText(text, textX, midY);
          } else {
            context.save();
            context.beginPath();
            context.rect(x + PADDING, y, Math.max(0, effectiveRoom), rowH);
            context.clip();
            context.fillText(text, textX, midY);
            context.restore();
            counts.clipped++;
          }

          if (st?.strikethrough) {
            const w = Math.min(effectiveRoom, this.#widths.measure(context, text));
            const lineX0 =
              align === "right" ? textX - w : align === "center" ? textX - w / 2 : textX;
            context.fillRect(lineX0, Math.round(midY), w, 1);
          }

          counts.cells++;
        }
      }
    }
  }

  #formulaRefs(
    frame: Frame,
    layout: SheetLayout,
    q: Quadrant,
    originX: number,
    originY: number,
  ): void {
    const refs = frame.formulaReferences;
    if (!refs || refs.length === 0) {
      return;
    }
    const context = this.#context;
    const { theme } = frame;
    context.save();
    context.beginPath();
    context.rect(q.clip.left, q.clip.top, q.clip.width, q.clip.height);
    context.clip();
    context.lineWidth = 1.5;
    context.setLineDash([4, 3]);

    for (const ref of refs) {
      const endRow = ref.lastRow + 1;
      const endCol = ref.lastCol + 1;
      if (
        endRow <= q.span.firstRow ||
        ref.firstRow >= q.span.endRow ||
        endCol <= q.span.firstCol ||
        ref.firstCol >= q.span.endCol
      ) {
        continue;
      }
      const x0 = originX + layout.colAxis.offsetOf(ref.firstCol);
      const y0 = originY + layout.rowAxis.offsetOf(ref.firstRow);
      const w = layout.colAxis.offsetOf(endCol) - layout.colAxis.offsetOf(ref.firstCol);
      const h = layout.rowAxis.offsetOf(endRow) - layout.rowAxis.offsetOf(ref.firstRow);
      const slot = ref.colorIndex & 7;
      context.fillStyle = theme.refFills[slot] ?? theme.selectionFill;
      context.fillRect(x0 + 1, y0 + 1, Math.max(0, w - 2), Math.max(0, h - 2));
      context.strokeStyle = theme.refStrokes[slot] ?? theme.selection;
      context.strokeRect(x0 + 0.75, y0 + 0.75, Math.max(0, w - 1.5), Math.max(0, h - 1.5));
    }

    context.restore();
  }

  #filterDropdownButtons(
    frame: Frame,
    layout: SheetLayout,
    q: Quadrant,
    originX: number,
    originY: number,
  ): void {
    const filter = frame.filterState;
    if (!filter) {
      return;
    }
    const headerDisp = frame.filterRowMap
      ? frame.filterRowMap.physicalToDisplay(filter.headerRow)
      : filter.headerRow;
    if (headerDisp === null || headerDisp < q.span.firstRow || headerDisp >= q.span.endRow) {
      return;
    }
    const cStart = Math.max(q.span.firstCol, filter.firstCol);
    const cEnd = Math.min(q.span.endCol - 1, filter.lastCol);
    if (cStart > cEnd) {
      return;
    }
    const activeSet = new Set(filter.activeCols);
    const context = this.#context;
    const { theme } = frame;
    const rowY = originY + layout.rowAxis.offsetOf(headerDisp);
    const rowH = layout.rowAxis.sizeOf(headerDisp);

    context.save();
    context.beginPath();
    context.rect(q.clip.left, q.clip.top, q.clip.width, q.clip.height);
    context.clip();

    for (let c = cStart; c <= cEnd; c++) {
      const colX = originX + layout.colAxis.offsetOf(c);
      const colW = layout.colAxis.sizeOf(c);
      const btn = filterButtonBox(colX, rowY, colW, rowH);
      if (!btn) {
        continue;
      }
      const isActive = activeSet.has(c);
      context.fillStyle = isActive ? theme.selectionFill : theme.headerBackground;
      context.fillRect(btn.x, btn.y, btn.size, btn.size);
      context.strokeStyle = isActive ? theme.filterIconActiveColor : theme.headerLine;
      context.lineWidth = 1;
      context.strokeRect(btn.x + 0.5, btn.y + 0.5, btn.size - 1, btn.size - 1);

      const cx = btn.x + btn.size / 2;
      const cy = btn.y + btn.size / 2;
      context.fillStyle = isActive ? theme.filterIconActiveColor : theme.filterIconColor;
      context.beginPath();
      if (isActive) {
        context.moveTo(cx - 4, cy - 3);
        context.lineTo(cx + 4, cy - 3);
        context.lineTo(cx + 1.2, cy + 0.5);
        context.lineTo(cx + 1.2, cy + 3.5);
        context.lineTo(cx - 1.2, cy + 3.5);
        context.lineTo(cx - 1.2, cy + 0.5);
      } else {
        context.moveTo(cx - 3.5, cy - 2);
        context.lineTo(cx + 3.5, cy - 2);
        context.lineTo(cx, cy + 2.5);
      }
      context.closePath();
      context.fill();
    }

    context.restore();
  }

  #spillBorderOutline(
    frame: Frame,
    layout: SheetLayout,
    q: Quadrant,
    originX: number,
    originY: number,
  ): void {
    const spill = frame.activeSpillRange;
    if (!spill) {
      return;
    }
    const endRow = spill.lastRow + 1;
    const endCol = spill.lastCol + 1;
    if (
      endRow <= q.span.firstRow ||
      spill.firstRow >= q.span.endRow ||
      endCol <= q.span.firstCol ||
      spill.firstCol >= q.span.endCol
    ) {
      return;
    }
    const context = this.#context;
    const x0 = originX + layout.colAxis.offsetOf(spill.firstCol);
    const y0 = originY + layout.rowAxis.offsetOf(spill.firstRow);
    const w = layout.colAxis.offsetOf(endCol) - layout.colAxis.offsetOf(spill.firstCol);
    const h = layout.rowAxis.offsetOf(endRow) - layout.rowAxis.offsetOf(spill.firstRow);

    context.save();
    context.beginPath();
    context.rect(q.clip.left, q.clip.top, q.clip.width, q.clip.height);
    context.clip();
    context.lineWidth = 1.5;
    context.setLineDash([4, 3]);
    context.strokeStyle = frame.theme.spillBorderColor;
    context.strokeRect(x0 + 0.75, y0 + 0.75, Math.max(0, w - 1.5), Math.max(0, h - 1.5));
    context.restore();
  }

  #clipboardOutline(
    frame: Frame,
    layout: SheetLayout,
    q: Quadrant,
    originX: number,
    originY: number,
  ): void {
    const cb = frame.clipboardRange;
    if (!cb) {
      return;
    }
    const r = cb.span;
    if (
      r.endRow <= q.span.firstRow ||
      r.firstRow >= q.span.endRow ||
      r.endCol <= q.span.firstCol ||
      r.firstCol >= q.span.endCol
    ) {
      return;
    }
    const context = this.#context;
    const x0 = originX + layout.colAxis.offsetOf(r.firstCol);
    const y0 = originY + layout.rowAxis.offsetOf(r.firstRow);
    const w = layout.colAxis.offsetOf(r.endCol) - layout.colAxis.offsetOf(r.firstCol);
    const h = layout.rowAxis.offsetOf(r.endRow) - layout.rowAxis.offsetOf(r.firstRow);

    context.save();
    context.beginPath();
    context.rect(q.clip.left, q.clip.top, q.clip.width, q.clip.height);
    context.clip();
    context.lineWidth = 1.75;
    context.setLineDash(cb.cut ? [5, 3] : [3, 3]);
    context.strokeStyle = frame.theme.selection;
    context.strokeRect(x0 + 0.5, y0 + 0.5, Math.max(0, w - 1), Math.max(0, h - 1));
    context.restore();
  }

  #selectionOutlines(
    frame: Frame,
    layout: SheetLayout,
    q: Quadrant,
    originX: number,
    originY: number,
  ): void {
    const { theme, selection, selectionRange } = frame;
    const ranges =
      frame.selectionRanges && frame.selectionRanges.length > 0
        ? frame.selectionRanges
        : selectionRange
          ? [selectionRange]
          : [];
    const context = this.#context;

    context.save();
    context.beginPath();
    context.rect(q.clip.left, q.clip.top, q.clip.width, q.clip.height);
    context.clip();

    for (let i = 0; i < ranges.length; i++) {
      const range = ranges[i];
      if (!range) {
        continue;
      }
      const isPrimary = i === ranges.length - 1;
      const singlePrimary =
        isPrimary &&
        ranges.length === 1 &&
        range.endRow - range.firstRow === 1 &&
        range.endCol - range.firstCol === 1;
      if (singlePrimary) {
        continue;
      }
      if (
        range.endRow <= q.span.firstRow ||
        range.firstRow >= q.span.endRow ||
        range.endCol <= q.span.firstCol ||
        range.firstCol >= q.span.endCol
      ) {
        continue;
      }
      const rx0 = originX + layout.colAxis.offsetOf(range.firstCol);
      const ry0 = originY + layout.rowAxis.offsetOf(range.firstRow);
      const rw = layout.colAxis.offsetOf(range.endCol) - layout.colAxis.offsetOf(range.firstCol);
      const rh = layout.rowAxis.offsetOf(range.endRow) - layout.rowAxis.offsetOf(range.firstRow);
      context.lineWidth = 1.5;
      context.strokeStyle = theme.selection;
      context.strokeRect(rx0 - 0.5, ry0 - 0.5, rw, rh);
    }

    if (
      selection &&
      selection.row >= q.span.firstRow &&
      selection.row < q.span.endRow &&
      selection.col >= q.span.firstCol &&
      selection.col < q.span.endCol
    ) {
      context.lineWidth = 2;
      context.strokeStyle = theme.selection;
      context.strokeRect(
        originX + layout.colAxis.offsetOf(selection.col) - 1,
        originY + layout.rowAxis.offsetOf(selection.row) - 1,
        layout.colAxis.sizeOf(selection.col),
        layout.rowAxis.sizeOf(selection.row),
      );
    }

    // Draw `6×6` fill handle at the bottom-right corner of the primary `selectionRange`!
    if (
      frame.showFillHandle !== false &&
      selectionRange &&
      (!frame.selectionRanges || frame.selectionRanges.length <= 1)
    ) {
      const brRow = selectionRange.endRow - 1;
      const brCol = selectionRange.endCol - 1;
      if (
        brRow >= q.span.firstRow &&
        brRow < q.span.endRow &&
        brCol >= q.span.firstCol &&
        brCol < q.span.endCol
      ) {
        const hx = originX + layout.colAxis.offsetOf(selectionRange.endCol);
        const hy = originY + layout.rowAxis.offsetOf(selectionRange.endRow);
        const half = FILL_HANDLE_SIZE / 2;
        context.fillStyle = theme.background;
        context.fillRect(hx - half - 1, hy - half - 1, FILL_HANDLE_SIZE + 2, FILL_HANDLE_SIZE + 2);
        context.fillStyle = theme.selection;
        context.fillRect(hx - half, hy - half, FILL_HANDLE_SIZE, FILL_HANDLE_SIZE);
      }
    }

    context.restore();
  }

  #fillPreviewOutline(
    frame: Frame,
    layout: SheetLayout,
    q: Quadrant,
    originX: number,
    originY: number,
  ): void {
    const preview = frame.fillPreview;
    if (!preview || isEmpty(preview)) {
      return;
    }
    if (
      preview.endRow <= q.span.firstRow ||
      preview.firstRow >= q.span.endRow ||
      preview.endCol <= q.span.firstCol ||
      preview.firstCol >= q.span.endCol
    ) {
      return;
    }
    const context = this.#context;
    const x0 = originX + layout.colAxis.offsetOf(preview.firstCol);
    const y0 = originY + layout.rowAxis.offsetOf(preview.firstRow);
    const w = layout.colAxis.offsetOf(preview.endCol) - layout.colAxis.offsetOf(preview.firstCol);
    const h = layout.rowAxis.offsetOf(preview.endRow) - layout.rowAxis.offsetOf(preview.firstRow);

    context.save();
    context.beginPath();
    context.rect(q.clip.left, q.clip.top, q.clip.width, q.clip.height);
    context.clip();
    context.lineWidth = 1.5;
    context.setLineDash([4, 3]);
    context.strokeStyle = frame.theme.selection;
    context.strokeRect(x0 - 0.5, y0 - 0.5, w, h);
    context.restore();
  }

  #headers(
    frame: Frame,
    layout: SheetLayout,
    sheet: SheetSize,
    fz: { frozenRows: number; frozenCols: number; height: number; width: number },
    right: number,
    bottom: number,
  ): void {
    const context = this.#context;
    const { size, metrics, theme, visible, selection, selectionRange, scale } = frame;
    const { headerHeight, headerWidth } = metrics;
    const ranges =
      frame.selectionRanges && frame.selectionRanges.length > 0
        ? frame.selectionRanges
        : selectionRange
          ? [selectionRange]
          : selection
            ? [
                {
                  firstRow: selection.row,
                  endRow: selection.row + 1,
                  firstCol: selection.col,
                  endCol: selection.col + 1,
                },
              ]
            : [];

    context.fillStyle = theme.headerBackground;
    context.fillRect(0, 0, size.width, headerHeight);
    context.fillRect(0, 0, headerWidth, size.height);

    const width = Math.max(1, Math.round(scale)) / scale;
    const snap = (position: number) => Math.round(position * scale) / scale - width / 2;

    const drawColStrip = (
      firstCol: number,
      endCol: number,
      scrollLeft: number,
      clipLeft: number,
      clipRight: number,
    ) => {
      if (firstCol >= endCol || clipRight <= clipLeft) {
        return;
      }
      const ox = headerWidth - scrollLeft;
      context.save();
      context.beginPath();
      context.rect(clipLeft, 0, clipRight - clipLeft, headerHeight);
      context.clip();

      context.fillStyle = theme.headerSelected;
      for (const r of ranges) {
        const c0 = Math.max(firstCol, r.firstCol);
        const c1 = Math.min(endCol, r.endCol);
        if (c0 < c1) {
          const x = Math.max(clipLeft, ox + layout.colAxis.offsetOf(c0));
          const end = Math.min(clipRight, ox + layout.colAxis.offsetOf(c1));
          if (end > x) {
            context.fillRect(x, 0, end - x, headerHeight);
          }
        }
      }

      context.font = theme.font;
      context.textBaseline = "middle";
      context.textAlign = "center";
      context.fillStyle = theme.headerText;
      for (let col = firstCol; col < endCol; col++) {
        const cx = ox + layout.colAxis.offsetOf(col);
        const cw = layout.colAxis.sizeOf(col);
        context.fillText(columnName(col), cx + cw / 2, headerHeight / 2);
      }

      context.beginPath();
      for (let col = firstCol + 1; col <= endCol; col++) {
        const x = snap(ox + layout.colAxis.offsetOf(col));
        if (x > clipLeft) {
          context.moveTo(x, 0);
          context.lineTo(x, headerHeight);
        }
      }
      context.lineWidth = width;
      context.strokeStyle = theme.headerLine;
      context.stroke();
      context.restore();
    };

    const drawRowStrip = (
      firstRow: number,
      endRow: number,
      scrollTop: number,
      clipTop: number,
      clipBottom: number,
    ) => {
      if (firstRow >= endRow || clipBottom <= clipTop) {
        return;
      }
      const oy = headerHeight - scrollTop;
      context.save();
      context.beginPath();
      context.rect(0, clipTop, headerWidth, clipBottom - clipTop);
      context.clip();

      context.fillStyle = theme.headerSelected;
      for (const r of ranges) {
        const r0 = Math.max(firstRow, r.firstRow);
        const r1 = Math.min(endRow, r.endRow);
        if (r0 < r1) {
          const y = Math.max(clipTop, oy + layout.rowAxis.offsetOf(r0));
          const end = Math.min(clipBottom, oy + layout.rowAxis.offsetOf(r1));
          if (end > y) {
            context.fillRect(0, y, headerWidth, end - y);
          }
        }
      }

      const filterState = frame.filterState ?? null;
      const filterRowMap = frame.filterRowMap ?? null;
      const hasFilteredHidden = Boolean(filterState && filterRowMap?.hasHiddenRows);

      context.font = theme.font;
      context.textBaseline = "middle";
      context.textAlign = "center";
      for (let row = firstRow; row < endRow; row++) {
        const physRow = filterRowMap ? filterRowMap.displayToPhysical(row) : row;
        const ry = oy + layout.rowAxis.offsetOf(row);
        const rh = layout.rowAxis.sizeOf(row);
        const isFilterBlue =
          hasFilteredHidden &&
          filterState !== null &&
          ((physRow > filterState.headerRow && physRow <= filterState.lastRow) ||
            (filterRowMap?.isGapBefore(row) ?? false) ||
            (filterRowMap?.isGapAfter(row) ?? false));
        context.fillStyle = isFilterBlue ? theme.filterHeaderText : theme.headerText;
        context.fillText(String(physRow + 1), headerWidth / 2, ry + rh / 2);
      }

      context.beginPath();
      for (let row = firstRow + 1; row <= endRow; row++) {
        const y = snap(oy + layout.rowAxis.offsetOf(row));
        if (y > clipTop) {
          context.moveTo(0, y);
          context.lineTo(headerWidth, y);
        }
      }
      context.lineWidth = width;
      context.strokeStyle = theme.headerLine;
      context.stroke();

      if (hasFilteredHidden && filterRowMap) {
        context.beginPath();
        for (let row = firstRow; row < endRow; row++) {
          if (filterRowMap.isGapBefore(row)) {
            const y = Math.round(oy + layout.rowAxis.offsetOf(row));
            if (y >= clipTop && y <= clipBottom) {
              context.moveTo(0, y);
              context.lineTo(headerWidth, y);
            }
          }
        }
        if (
          endRow === sheet.rows &&
          filterRowMap.totalRows > 0 &&
          filterRowMap.physicalToDisplay(filterRowMap.totalRows - 1) === null
        ) {
          const yBottom = Math.round(oy + layout.rowAxis.offsetOf(endRow));
          if (yBottom >= clipTop && yBottom <= clipBottom) {
            context.moveTo(0, yBottom);
            context.lineTo(headerWidth, yBottom);
          }
        }
        context.lineWidth = 2;
        context.strokeStyle = theme.filterHeaderText;
        context.stroke();
      }
      context.restore();
    };

    if (fz.frozenCols > 0) {
      drawColStrip(0, fz.frozenCols, 0, headerWidth, Math.min(right, headerWidth + fz.width));
    }
    drawColStrip(
      visible.firstCol,
      visible.endCol,
      frame.scroll.left,
      headerWidth + fz.width,
      right,
    );

    if (fz.frozenRows > 0) {
      drawRowStrip(0, fz.frozenRows, 0, headerHeight, Math.min(bottom, headerHeight + fz.height));
    }
    drawRowStrip(
      visible.firstRow,
      visible.endRow,
      frame.scroll.top,
      headerHeight + fz.height,
      bottom,
    );

    // Top-left select-all corner button + subtle triangle affordance.
    const allSelected =
      sheet.rows > 0 &&
      sheet.cols > 0 &&
      selectionRange?.firstRow === 0 &&
      selectionRange.endRow === sheet.rows &&
      selectionRange.firstCol === 0 &&
      selectionRange.endCol === sheet.cols;
    context.fillStyle = allSelected ? theme.headerSelected : theme.headerBackground;
    context.fillRect(0, 0, headerWidth, headerHeight);
    context.fillStyle = theme.headerLine;
    context.beginPath();
    context.moveTo(headerWidth - 5, headerHeight - 13);
    context.lineTo(headerWidth - 5, headerHeight - 5);
    context.lineTo(headerWidth - 13, headerHeight - 5);
    context.closePath();
    context.fill();

    context.beginPath();
    context.moveTo(snap(headerWidth), 0);
    context.lineTo(snap(headerWidth), size.height);
    context.moveTo(0, snap(headerHeight));
    context.lineTo(size.width, snap(headerHeight));
    context.lineWidth = width;
    context.strokeStyle = theme.headerLine;
    context.stroke();
  }

  #freezeDividers(
    frame: Frame,
    fz: { frozenRows: number; frozenCols: number; height: number; width: number },
    right: number,
    bottom: number,
  ): void {
    if (fz.frozenRows <= 0 && fz.frozenCols <= 0) {
      return;
    }
    const context = this.#context;
    const { metrics, theme } = frame;
    context.save();
    context.lineWidth = 2;
    context.strokeStyle = theme.freezeLine;
    context.beginPath();
    if (fz.frozenRows > 0) {
      const y = Math.round(metrics.headerHeight + fz.height);
      context.moveTo(0, y);
      context.lineTo(right, y);
    }
    if (fz.frozenCols > 0) {
      const x = Math.round(metrics.headerWidth + fz.width);
      context.moveTo(x, 0);
      context.lineTo(x, bottom);
    }
    context.stroke();
    context.restore();
  }

  #reorderGuide(
    frame: Frame,
    layout: SheetLayout,
    sheet: SheetSize,
    fz: { frozenRows: number; frozenCols: number; height: number; width: number },
    right: number,
    bottom: number,
  ): void {
    const guide = frame.reorderGuide;
    if (!guide) {
      return;
    }
    const context = this.#context;
    const { metrics, theme, scroll } = frame;
    context.save();
    context.lineWidth = 2.5;
    context.strokeStyle = theme.selection;
    context.beginPath();
    if (guide.axis === "row") {
      const r = Math.min(sheet.rows, Math.max(0, guide.beforeRow));
      const effTop = r <= fz.frozenRows ? 0 : scroll.top;
      const y = Math.round(metrics.headerHeight + layout.rowAxis.offsetOf(r) - effTop);
      if (y >= metrics.headerHeight && y <= bottom) {
        context.moveTo(0, y);
        context.lineTo(right, y);
      }
    } else {
      const c = Math.min(sheet.cols, Math.max(0, guide.beforeCol));
      const effLeft = c <= fz.frozenCols ? 0 : scroll.left;
      const x = Math.round(metrics.headerWidth + layout.colAxis.offsetOf(c) - effLeft);
      if (x >= metrics.headerWidth && x <= right) {
        context.moveTo(x, 0);
        context.lineTo(x, bottom);
      }
    }
    context.stroke();
    context.restore();
  }
}

function buildQuadrants(
  metrics: Metrics,
  size: Size,
  scroll: Scroll,
  visible: Span,
  fz: { frozenRows: number; frozenCols: number; height: number; width: number },
): Quadrant[] {
  const right = size.width - metrics.scrollbarSize;
  const bottom = size.height - metrics.scrollbarSize;
  const hw = metrics.headerWidth;
  const hh = metrics.headerHeight;
  const splitX = Math.min(right, hw + fz.width);
  const splitY = Math.min(bottom, hh + fz.height);

  const out: Quadrant[] = [];
  if (fz.frozenRows > 0 && fz.frozenCols > 0) {
    out.push({
      clip: {
        left: hw,
        top: hh,
        width: Math.max(0, splitX - hw),
        height: Math.max(0, splitY - hh),
      },
      span: { firstRow: 0, endRow: fz.frozenRows, firstCol: 0, endCol: fz.frozenCols },
      scrollTop: 0,
      scrollLeft: 0,
    });
  }
  if (fz.frozenRows > 0) {
    out.push({
      clip: {
        left: splitX,
        top: hh,
        width: Math.max(0, right - splitX),
        height: Math.max(0, splitY - hh),
      },
      span: {
        firstRow: 0,
        endRow: fz.frozenRows,
        firstCol: visible.firstCol,
        endCol: visible.endCol,
      },
      scrollTop: 0,
      scrollLeft: scroll.left,
    });
  }
  if (fz.frozenCols > 0) {
    out.push({
      clip: {
        left: hw,
        top: splitY,
        width: Math.max(0, splitX - hw),
        height: Math.max(0, bottom - splitY),
      },
      span: {
        firstRow: visible.firstRow,
        endRow: visible.endRow,
        firstCol: 0,
        endCol: fz.frozenCols,
      },
      scrollTop: scroll.top,
      scrollLeft: 0,
    });
  }
  out.push({
    clip: {
      left: splitX,
      top: splitY,
      width: Math.max(0, right - splitX),
      height: Math.max(0, bottom - splitY),
    },
    span: visible,
    scrollTop: scroll.top,
    scrollLeft: scroll.left,
  });
  return out;
}

function fontForStyle(theme: Theme, style: GridCellStyle | undefined): string {
  if (!style || (!style.bold && !style.italic)) {
    return theme.font;
  }
  const weight = style.bold ? "700 " : "";
  const slant = style.italic ? "italic " : "";
  return `${slant}${weight}${theme.fontSize}px ${theme.fontFamily}`;
}
