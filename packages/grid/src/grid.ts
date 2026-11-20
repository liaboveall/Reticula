import { PageCache } from "./cache.ts";
import {
  type Box,
  cellArea,
  cellAt,
  cellBox,
  cellsOfPages,
  clampScroll,
  columnName,
  contentSize,
  DEFAULT_METRICS,
  defaultLayout,
  EMPTY_SPAN,
  frozenPixels,
  headerHitAt,
  isEmpty,
  type Metrics,
  pagesToHold,
  type Scroll,
  type SheetLayout,
  type SheetSize,
  type Size,
  type Span,
  SparseAxis,
  sameSpan,
  scrollLimits,
  scrollToReveal,
  visibleCells,
} from "./layout.ts";
import { decodePage } from "./page.ts";
import {
  FILL_HANDLE_SIZE,
  type FormulaReferenceOverlay,
  type GridCellStyle,
  type RemotePresenceOverlay,
  Renderer,
  type ReorderGuide,
} from "./renderer.ts";
import { Scrollbar } from "./scrollbar.ts";
import { readTheme, type Theme } from "./theme.ts";

/** Contiguous run of row or column identifiers (`firstId .. firstId + len`). */
export interface IdRun {
  readonly firstId: string;
  readonly len: number;
}

/** Sheet dimensions + optional M1b sparse metrics, freeze counts, and style table. */
export interface SheetConfig extends SheetSize {
  readonly frozenRows?: number;
  readonly frozenCols?: number;
  readonly customRows?: readonly (readonly [number, number])[];
  readonly customCols?: readonly (readonly [number, number])[];
  readonly styles?: readonly GridCellStyle[];
}

/** A range of cells the grid asks the engine for. */
export interface Subscription {
  readonly epoch: number;
  readonly firstRow: number;
  readonly rowCount: number;
  readonly firstCol: number;
  readonly colCount: number;
}

export interface GridContextTarget {
  readonly clientX: number;
  readonly clientY: number;
  readonly zone: "cell" | "rowHeader" | "colHeader";
  readonly row: number;
  readonly col: number;
}

export interface GridOptions {
  readonly subscribe: (subscription: Subscription) => void;
  readonly margin?: { readonly rows: number; readonly cols: number };
  readonly metrics?: Metrics;
  readonly onFrame?: (frame: FrameReport) => void;
  readonly onSelectionChange?: (selection: Selection | null) => void;
  readonly onActivateEdit?: (cell: { readonly row: number; readonly col: number }) => void;
  readonly onResizeRows?: (sizes: readonly { run: IdRun; sizePx: number | null }[]) => void;
  readonly onResizeCols?: (sizes: readonly { run: IdRun; sizePx: number | null }[]) => void;
  readonly onMoveRows?: (rows: readonly IdRun[], afterRowId: string | null) => void;
  readonly onMoveCols?: (cols: readonly IdRun[], afterColId: string | null) => void;
  readonly onFillRange?: (src: Span, dst: Span) => void;
  readonly onAutoFillDown?: (src: Span) => void;
  /**
   * Called when the user clicks or drags on the grid while formula point-and-click
   * mode is active. Return `true` to consume the pointer gesture as a formula
   * reference pick without moving the active cell selection.
   */
  readonly onPointAndClickRange?: (range: Span) => boolean;
  readonly onContextMenu?: (target: GridContextTarget) => void;
}

/** What one frame did. */
export interface FrameReport {
  readonly time: number;
  readonly work: number;
  readonly visible: Span;
  readonly cells: number;
  readonly missing: number;
}

export type SelectionScope = "cells" | "rows" | "cols" | "all";

/** The active cell and selection chosen by the user. */
export interface Selection {
  readonly row: number;
  readonly col: number;
  readonly range: Span;
  readonly ranges: readonly Span[];
  readonly scope: SelectionScope;
  readonly name: string;
  readonly rowId: string | null;
  readonly colId: string | null;
  readonly text: string | null;
}

export interface GridStats {
  readonly sheet: SheetSize;
  readonly metrics: Metrics;
  readonly scroll: Scroll;
  readonly scrollLimits: Scroll;
  readonly visible: Span;
  readonly frames: number;
  readonly framesWithMissing: number;
  readonly missingCells: number;
  readonly cellsDrawn: number;
  readonly pagesHeld: number;
  readonly pagesReceived: number;
  readonly pagesDiscarded: number;
  readonly bytesReceived: number;
  readonly epoch: number;
  readonly frameWork: {
    readonly samples: number;
    readonly last: number;
    readonly p50: number;
    readonly p95: number;
    readonly p99: number;
    readonly max: number;
  };
  readonly selection: Selection | null;
}

const IDLE_MARGIN = { rows: 0.5, cols: 1 };
const FAST_MARGIN = { rows: 2, cols: 1 };
const FAST_SCROLL_HOLD_MS = 400;
const FRAME_SAMPLES = 600;
const WHEEL_LINE = 40;
const MIN_ROW_PX = 16;
const MAX_ROW_PX = 600;
const MIN_COL_PX = 24;
const MAX_COL_PX = 1200;

/** Coalesces sorted/ordered `bigint` IDs into contiguous `{ firstId, len }` runs. */
export function coalesceIdRuns(ids: readonly bigint[]): IdRun[] {
  const runs: IdRun[] = [];
  for (const id of ids) {
    const prev = runs[runs.length - 1];
    if (prev && BigInt(prev.firstId) + BigInt(prev.len) === id) {
      runs[runs.length - 1] = { firstId: prev.firstId, len: prev.len + 1 };
    } else {
      runs.push({ firstId: String(id), len: 1 });
    }
  }
  return runs;
}

type DragInteraction =
  | { readonly kind: "select"; readonly mode: SelectionScope }
  | { readonly kind: "pointRef"; readonly anchor: { row: number; col: number } }
  | { readonly kind: "fill"; readonly src: Span }
  | {
      readonly kind: "colResize";
      readonly col: number;
      readonly startClientX: number;
      readonly startWidth: number;
    }
  | {
      readonly kind: "rowResize";
      readonly row: number;
      readonly startClientY: number;
      readonly startHeight: number;
    }
  | { readonly kind: "rowReorder"; readonly startRow: number; moved: boolean }
  | { readonly kind: "colReorder"; readonly startCol: number; moved: boolean };

/**
 * A spreadsheet grid drawn on a canvas (`13-m1a.md` & `14-m1b.md` 6.2–6.3).
 */
export class Grid {
  #host: HTMLElement;
  #canvas: HTMLCanvasElement;
  #renderer: Renderer;
  #vertical: Scrollbar;
  #horizontal: Scrollbar;
  #corner: HTMLDivElement;
  #options: GridOptions;
  #metrics: Metrics;
  #fixedMargin: { readonly rows: number; readonly cols: number } | null;
  #frameListeners = new Set<(frame: FrameReport) => void>();

  #theme: Theme;
  #sheet: SheetSize = { rows: 0, cols: 0 };
  #customRows: [number, number][] = [];
  #customCols: [number, number][] = [];
  #frozenRows = 0;
  #frozenCols = 0;
  #layout: SheetLayout;
  #styles: readonly GridCellStyle[] = [];
  #formulaRefs: readonly FormulaReferenceOverlay[] = [];
  #remotePresences: readonly RemotePresenceOverlay[] = [];
  #clipboardRange: { readonly span: Span; readonly cut: boolean } | null = null;
  #fillPreview: Span | null = null;
  #reorderGuide: ReorderGuide | null = null;

  #size: Size = { width: 0, height: 0 };
  #scale = 1;
  #scroll: Scroll = { top: 0, left: 0 };
  #anchor: { row: number; col: number } | null = null;
  #focus: { row: number; col: number } | null = null;
  #extraRanges: Span[] = [];
  #scope: SelectionScope = "cells";
  #cache = new PageCache();
  #epoch = 0;
  #lastFreezeKey = "0:0";

  #lastVelocityTop = 0;
  #lastVelocityTime = 0;
  #fastScrollUntil = 0;

  #draggingPointerId: number | null = null;
  #drag: DragInteraction | null = null;
  #dragClientX = 0;
  #dragClientY = 0;
  #autoScrollTimer = 0;

  #frameRequest = 0;
  #resized = true;
  #destroyed = false;
  #observer: ResizeObserver;
  #scheme: MediaQueryList;
  #density: MediaQueryList | null = null;

  #visible: Span = EMPTY_SPAN;
  #frames = 0;
  #framesWithMissing = 0;
  #missingCells = 0;
  #cellsDrawn = 0;
  #pagesReceived = 0;
  #pagesDiscarded = 0;
  #bytesReceived = 0;
  #pendingWork = 0;
  #work = new Float64Array(FRAME_SAMPLES);
  #workCount = 0;
  #lastWork = 0;

  constructor(host: HTMLElement, options: GridOptions) {
    this.#host = host;
    this.#options = options;
    this.#metrics = options.metrics ?? DEFAULT_METRICS;
    this.#layout = defaultLayout(this.#metrics);
    this.#fixedMargin = options.margin ?? null;
    if (options.onFrame) {
      this.#frameListeners.add(options.onFrame);
    }

    host.classList.add("reticula-grid");
    host.tabIndex = 0;
    this.#canvas = document.createElement("canvas");
    this.#renderer = new Renderer(this.#canvas);
    this.#vertical = new Scrollbar("vertical", (top) => this.scrollTo({ top }));
    this.#horizontal = new Scrollbar("horizontal", (left) => this.scrollTo({ left }));
    this.#corner = document.createElement("div");
    this.#corner.className = "reticula-scrollbar-corner";
    this.#placeScrollbars();
    host.append(this.#canvas, this.#vertical.element, this.#horizontal.element, this.#corner);

    this.#theme = readTheme(host);
    this.#scheme = matchMedia("(prefers-color-scheme: dark)");
    this.#scheme.addEventListener("change", this.#restyle);
    this.#watchDensity();

    host.addEventListener("wheel", this.#onWheel, { passive: false });
    this.#canvas.addEventListener("pointerdown", this.#onPointerDown);
    this.#canvas.addEventListener("pointermove", this.#onPointerMove);
    this.#canvas.addEventListener("pointerup", this.#onPointerUp);
    this.#canvas.addEventListener("pointercancel", this.#onPointerUp);
    this.#canvas.addEventListener("dblclick", this.#onDoubleClick);
    this.#canvas.addEventListener("contextmenu", this.#onContextMenu);
    this.#observer = new ResizeObserver(() => {
      this.#resized = true;
      this.#invalidate();
    });
    this.#observer.observe(host);
    this.#invalidate();
  }

  /** Opens a new sheet, resetting scroll, selection, and page cache. */
  setSheet(sheet: SheetConfig): void {
    this.#sheet = { rows: sheet.rows, cols: sheet.cols };
    this.#applySheetConfig(sheet);
    this.#scroll = { top: 0, left: 0 };
    this.#anchor = null;
    this.#focus = null;
    this.#extraRanges = [];
    this.#remotePresences = [];
    this.#scope = "cells";
    this.#clipboardRange = null;
    this.#fillPreview = null;
    this.#reorderGuide = null;
    this.#cache.clear();
    this.#notifySelection();
    this.#invalidate();
  }

  /** Updates sheet dimensions, metrics, freeze panes, and styles while preserving scroll & selection. */
  updateSheet(sheet: SheetConfig): void {
    const prevFreezeKey = `${this.#frozenRows}:${this.#frozenCols}`;
    this.#sheet = { rows: sheet.rows, cols: sheet.cols };
    this.#applySheetConfig(sheet);
    const nextFreezeKey = `${this.#frozenRows}:${this.#frozenCols}`;
    if (prevFreezeKey !== nextFreezeKey) {
      // Force subscription refresh when freeze counts change so pinned pages arrive immediately.
      this.#lastFreezeKey = "";
    }
    this.#scroll = clampScroll(
      this.#scroll,
      scrollLimits(this.#metrics, this.#sheet, this.#size, this.#layout),
    );
    if (this.#anchor && (this.#sheet.rows <= 0 || this.#sheet.cols <= 0)) {
      this.#anchor = null;
      this.#focus = null;
      this.#extraRanges = [];
    } else if (this.#anchor && this.#focus) {
      this.#anchor = {
        row: Math.min(this.#sheet.rows - 1, this.#anchor.row),
        col: Math.min(this.#sheet.cols - 1, this.#anchor.col),
      };
      this.#focus = {
        row: Math.min(this.#sheet.rows - 1, this.#focus.row),
        col: Math.min(this.#sheet.cols - 1, this.#focus.col),
      };
      this.#extraRanges = this.#extraRanges
        .map((r) => ({
          firstRow: Math.min(this.#sheet.rows, r.firstRow),
          endRow: Math.min(this.#sheet.rows, r.endRow),
          firstCol: Math.min(this.#sheet.cols, r.firstCol),
          endCol: Math.min(this.#sheet.cols, r.endCol),
        }))
        .filter((r) => !isEmpty(r));
    }
    this.#notifySelection();
    this.#invalidate();
  }

  /** Updates the sheet style table without touching scroll or selection. */
  setStyles(styles: readonly GridCellStyle[]): void {
    this.#styles = styles;
    this.#invalidate();
  }

  /** Sets colored formula reference highlights (`0..7`) drawn on the active sheet. */
  setFormulaReferences(refs: readonly FormulaReferenceOverlay[]): void {
    this.#formulaRefs = refs;
    this.#invalidate();
  }

  /** Replaces the visible remote collaborators on the active sheet. */
  setRemotePresences(presences: readonly RemotePresenceOverlay[]): void {
    this.#remotePresences = presences;
    this.#invalidate();
  }

  /** Sets or clears the copied/cut marching-ants indicator rectangle. */
  setClipboardRange(range: Span | null, cut = false): void {
    this.#clipboardRange = range ? { span: range, cut } : null;
    this.#invalidate();
  }

  #applySheetConfig(sheet: SheetConfig): void {
    if (sheet.frozenRows !== undefined) {
      this.#frozenRows = sheet.frozenRows;
    }
    if (sheet.frozenCols !== undefined) {
      this.#frozenCols = sheet.frozenCols;
    }
    if (sheet.customRows !== undefined) {
      this.#customRows = sheet.customRows.map(([r, px]) => [r, px]);
    }
    if (sheet.customCols !== undefined) {
      this.#customCols = sheet.customCols.map(([c, px]) => [c, px]);
    }
    if (sheet.styles !== undefined) {
      this.#styles = sheet.styles;
    }
    this.#rebuildLayout();
  }

  #rebuildLayout(): void {
    this.#layout = {
      rowAxis: new SparseAxis(this.#metrics.rowHeight, this.#customRows),
      colAxis: new SparseAxis(this.#metrics.colWidth, this.#customCols),
      frozenRows: this.#frozenRows,
      frozenCols: this.#frozenCols,
    };
  }

  /** Takes a viewport page the engine sent. */
  acceptPage(buffer: ArrayBuffer): void {
    const started = performance.now();
    const page = decodePage(buffer);
    this.#pagesReceived++;
    this.#bytesReceived += page.byteLength;
    if (this.#cache.accept(page)) {
      if (this.#anchor) {
        const range = this.selectionRange;
        if (
          range &&
          page.firstRow < range.endRow &&
          page.firstRow + page.rowCount > range.firstRow &&
          page.firstCol < range.endCol &&
          page.firstCol + page.colCount > range.firstCol
        ) {
          this.#notifySelection();
        }
      }
      this.#invalidate();
    } else {
      this.#pagesDiscarded++;
    }
    this.#pendingWork += performance.now() - started;
  }

  /** Scrolls to a position in CSS pixels, as far as the sheet allows. */
  scrollTo(position: Partial<Scroll>): void {
    const limits = scrollLimits(this.#metrics, this.#sheet, this.#size, this.#layout);
    const next = clampScroll({ ...this.#scroll, ...position }, limits);
    if (next.top !== this.#scroll.top || next.left !== this.#scroll.left) {
      const now = performance.now();
      const dt = now - this.#lastVelocityTime;
      const dTop = Math.abs(next.top - this.#lastVelocityTop);
      const areaHeight = Math.max(1, cellArea(this.#metrics, this.#size).height);
      if (dt > 0 && dt < 250 && (dTop / dt) * 1000 > areaHeight * 1.5) {
        this.#fastScrollUntil = now + FAST_SCROLL_HOLD_MS;
      } else if (dTop > areaHeight * 1.5) {
        this.#fastScrollUntil = now + FAST_SCROLL_HOLD_MS;
      }
      this.#lastVelocityTop = next.top;
      this.#lastVelocityTime = now;
      this.#scroll = next;
      this.#invalidate();
    }
  }

  /** Selects a single cell, clearing extra ranges. */
  select(cell: { row: number; col: number } | null): void {
    this.#extraRanges = [];
    this.#scope = "cells";
    if (!cell || this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      this.#anchor = null;
      this.#focus = null;
    } else {
      const clamped = {
        row: Math.min(this.#sheet.rows - 1, Math.max(0, cell.row)),
        col: Math.min(this.#sheet.cols - 1, Math.max(0, cell.col)),
      };
      this.#anchor = clamped;
      this.#focus = clamped;
    }
    this.#notifySelection();
    this.#invalidate();
  }

  /** Selects a rectangular range from `anchor` to `focus`. */
  selectRange(
    anchor: { row: number; col: number },
    focus: { row: number; col: number },
    scope: SelectionScope = "cells",
  ): void {
    if (this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      return;
    }
    this.#extraRanges = [];
    this.#scope = scope;
    this.#anchor = {
      row: Math.min(this.#sheet.rows - 1, Math.max(0, anchor.row)),
      col: Math.min(this.#sheet.cols - 1, Math.max(0, anchor.col)),
    };
    this.#focus = {
      row: Math.min(this.#sheet.rows - 1, Math.max(0, focus.row)),
      col: Math.min(this.#sheet.cols - 1, Math.max(0, focus.col)),
    };
    this.#notifySelection();
    this.#invalidate();
  }

  /** Selects the entire worksheet (`Ctrl+A` or top-left corner click). */
  selectAll(): void {
    if (this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      return;
    }
    this.#extraRanges = [];
    this.#scope = "all";
    const cur = this.#anchor ?? { row: 0, col: 0 };
    this.#anchor = cur;
    this.#focus = { row: this.#sheet.rows - 1, col: this.#sheet.cols - 1 };
    this.#notifySelection();
    this.#invalidate();
  }

  /** Active anchor cell (`null` when nothing is selected). */
  get selection(): { readonly row: number; readonly col: number } | null {
    return this.#anchor;
  }

  /** Primary selection span (`firstRow..endRow`, `firstCol..endCol`). */
  get selectionRange(): Span | null {
    if (!this.#anchor || !this.#focus) {
      return null;
    }
    if (this.#scope === "all") {
      return { firstRow: 0, endRow: this.#sheet.rows, firstCol: 0, endCol: this.#sheet.cols };
    }
    if (this.#scope === "rows") {
      return {
        firstRow: Math.min(this.#anchor.row, this.#focus.row),
        endRow: Math.max(this.#anchor.row, this.#focus.row) + 1,
        firstCol: 0,
        endCol: this.#sheet.cols,
      };
    }
    if (this.#scope === "cols") {
      return {
        firstRow: 0,
        endRow: this.#sheet.rows,
        firstCol: Math.min(this.#anchor.col, this.#focus.col),
        endCol: Math.max(this.#anchor.col, this.#focus.col) + 1,
      };
    }
    return {
      firstRow: Math.min(this.#anchor.row, this.#focus.row),
      endRow: Math.max(this.#anchor.row, this.#focus.row) + 1,
      firstCol: Math.min(this.#anchor.col, this.#focus.col),
      endCol: Math.max(this.#anchor.col, this.#focus.col) + 1,
    };
  }

  /** All selected spans (`Ctrl+Click` multi-selection + primary selection). */
  get selectionRanges(): readonly Span[] {
    const primary = this.selectionRange;
    if (!primary) {
      return [];
    }
    return this.#extraRanges.length === 0 ? [primary] : [...this.#extraRanges, primary];
  }

  get selectionScope(): SelectionScope {
    return this.#scope;
  }

  get sheetSize(): SheetSize {
    return this.#sheet;
  }

  /** Full selection descriptor with resolved IDs and A1 name. */
  selectionDetails(): Selection | null {
    return this.#selection();
  }

  /** Resolves a `0`-based row position's 64-bit ID as decimal string, if cached. */
  rowIdAt(row: number): string | null {
    const id = this.#cache.rowIdAt(row);
    return id === undefined ? null : String(id);
  }

  /** Resolves a `0`-based column position's 64-bit ID as decimal string, if cached. */
  colIdAt(col: number): string | null {
    const id = this.#cache.colIdAt(col);
    return id === undefined ? null : String(id);
  }

  /** Resolves contiguous `IdRun`s for all unique rows across all selected ranges. */
  selectedRowRuns(): IdRun[] | null {
    const ranges = this.selectionRanges;
    if (ranges.length === 0) {
      return null;
    }
    const rowPositions = new Set<number>();
    for (const r of ranges) {
      for (let pos = r.firstRow; pos < r.endRow; pos++) {
        rowPositions.add(pos);
      }
    }
    const sorted = [...rowPositions].sort((a, b) => a - b);
    const ids: bigint[] = [];
    for (const pos of sorted) {
      const id = this.#cache.rowIdAt(pos);
      if (id === undefined) {
        return null;
      }
      ids.push(id);
    }
    return coalesceIdRuns(ids);
  }

  /** Resolves contiguous `IdRun`s for all unique columns across all selected ranges. */
  selectedColRuns(): IdRun[] | null {
    const ranges = this.selectionRanges;
    if (ranges.length === 0) {
      return null;
    }
    const colPositions = new Set<number>();
    for (const r of ranges) {
      for (let pos = r.firstCol; pos < r.endCol; pos++) {
        colPositions.add(pos);
      }
    }
    const sorted = [...colPositions].sort((a, b) => a - b);
    const ids: bigint[] = [];
    for (const pos of sorted) {
      const id = this.#cache.colIdAt(pos);
      if (id === undefined) {
        return null;
      }
      ids.push(id);
    }
    return coalesceIdRuns(ids);
  }

  /** Resolves contiguous row and column `IdRun`s for clearing/formatting cells. */
  selectedCellRuns(): { readonly rows: IdRun[]; readonly cols: IdRun[] } | null {
    const rows = this.selectedRowRuns();
    if (!rows) {
      return null;
    }
    const cols = this.selectedColRuns();
    if (!cols) {
      return null;
    }
    return { rows, cols };
  }

  /**
   * Resolves the optimal `SetFormat` target scope (`cols` for whole-column or
   * select-all selections so 1M-row sheets format in `O(1)` without needing
   * uncached row IDs; `rows` for whole-row selections; `cells` otherwise).
   */
  selectedFormatTarget():
    | { readonly scope: "cols"; readonly cols: IdRun[] }
    | { readonly scope: "rows"; readonly rows: IdRun[] }
    | { readonly scope: "cells"; readonly rows: IdRun[]; readonly cols: IdRun[] }
    | null {
    if (this.#scope === "cols" || this.#scope === "all") {
      const cols = this.selectedColRuns();
      return cols ? { scope: "cols", cols } : null;
    }
    if (this.#scope === "rows") {
      const rows = this.selectedRowRuns();
      return rows ? { scope: "rows", rows } : null;
    }
    const cells = this.selectedCellRuns();
    return cells ? { scope: "cells", rows: cells.rows, cols: cells.cols } : null;
  }

  /** Moves the active selection by `(rows, cols)` collapsing multi-selection. */
  moveSelection(rows: number, cols: number): void {
    const from = this.#anchor;
    if (!from || this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      return;
    }
    const row = Math.min(this.#sheet.rows - 1, Math.max(0, from.row + rows));
    const col = Math.min(this.#sheet.cols - 1, Math.max(0, from.col + cols));
    this.#extraRanges = [];
    this.#scope = "cells";
    this.#anchor = { row, col };
    this.#focus = { row, col };
    this.reveal(row, col);
    this.#notifySelection();
    this.#invalidate();
  }

  /** Extends the active selection's focus corner by `(rows, cols)` (`Shift+Arrow`). */
  extendSelection(rows: number, cols: number): void {
    const from = this.#focus ?? this.#anchor;
    if (!from || !this.#anchor || this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      return;
    }
    const row = Math.min(this.#sheet.rows - 1, Math.max(0, from.row + rows));
    const col = Math.min(this.#sheet.cols - 1, Math.max(0, from.col + cols));
    this.#focus = { row, col };
    this.reveal(row, col);
    this.#notifySelection();
    this.#invalidate();
  }

  /** Moves or extends selection by one visible page height (`PageUp` / `PageDown`). */
  pageVertical(direction: 1 | -1, extend: boolean): void {
    const area = cellArea(this.#metrics, this.#size);
    const fz = frozenPixels(this.#metrics, this.#sheet, this.#size, this.#layout);
    const netHeight = Math.max(this.#metrics.rowHeight, area.height - fz.height);
    const approxRows = Math.max(1, Math.floor(netHeight / this.#metrics.rowHeight));
    if (extend) {
      this.extendSelection(direction * approxRows, 0);
    } else {
      this.moveSelection(direction * approxRows, 0);
    }
  }

  /** Applies an exact target coordinate jump (`Home`, `Ctrl+Home`, `End`, `Ctrl+Arrow`, `Ctrl+End`). */
  jumpTo(row: number, col: number, extend: boolean): void {
    if (this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      return;
    }
    const r = Math.min(this.#sheet.rows - 1, Math.max(0, row));
    const c = Math.min(this.#sheet.cols - 1, Math.max(0, col));
    if (extend && this.#anchor) {
      this.#scope = "cells";
      this.#focus = { row: r, col: c };
    } else {
      this.#extraRanges = [];
      this.#scope = "cells";
      this.#anchor = { row: r, col: c };
      this.#focus = { row: r, col: c };
    }
    this.reveal(r, c);
    this.#notifySelection();
    this.#invalidate();
  }

  /** Scrolls just far enough for a cell to be fully in view. */
  reveal(row: number, col: number): void {
    this.scrollTo(
      scrollToReveal(this.#metrics, this.#size, this.#scroll, row, col, this.#sheet, this.#layout),
    );
  }

  /** Where a cell is drawn in CSS pixels from the top-left of the grid element. */
  boxOf(row: number, col: number): Box {
    return cellBox(
      this.#metrics,
      this.#drawnScroll(),
      row,
      col,
      this.#size,
      this.#sheet,
      this.#layout,
    );
  }

  watchFrames(listener: (frame: FrameReport) => void): () => void {
    this.#frameListeners.add(listener);
    return () => this.#frameListeners.delete(listener);
  }

  /** The text of a cell, if the page that holds it has arrived. */
  textAt(row: number, col: number): string | null {
    const page = this.#cache.pageOf(row, col);
    if (!page || row >= page.firstRow + page.rowCount || col >= page.firstCol + page.colCount) {
      return null;
    }
    return page.texts[(row - page.firstRow) * page.colCount + (col - page.firstCol)] ?? null;
  }

  stats(): GridStats {
    const samples = Math.min(this.#workCount, FRAME_SAMPLES);
    const sorted = this.#work.slice(0, samples).sort();
    const quantile = (q: number) => sorted[Math.min(samples - 1, Math.floor(q * samples))] ?? 0;
    return {
      sheet: this.#sheet,
      metrics: this.#metrics,
      scroll: this.#scroll,
      scrollLimits: scrollLimits(this.#metrics, this.#sheet, this.#size, this.#layout),
      visible: this.#visible,
      frames: this.#frames,
      framesWithMissing: this.#framesWithMissing,
      missingCells: this.#missingCells,
      cellsDrawn: this.#cellsDrawn,
      pagesHeld: this.#cache.size,
      pagesReceived: this.#pagesReceived,
      pagesDiscarded: this.#pagesDiscarded,
      bytesReceived: this.#bytesReceived,
      epoch: this.#epoch,
      frameWork: {
        samples,
        last: this.#lastWork,
        p50: quantile(0.5),
        p95: quantile(0.95),
        p99: quantile(0.99),
        max: sorted[samples - 1] ?? 0,
      },
      selection: this.#selection(),
    };
  }

  destroy(): void {
    this.#stopAutoScroll();
    this.#hold(EMPTY_SPAN);
    this.#destroyed = true;
    this.#frameListeners.clear();
    cancelAnimationFrame(this.#frameRequest);
    this.#observer.disconnect();
    this.#scheme.removeEventListener("change", this.#restyle);
    this.#density?.removeEventListener("change", this.#onDensityChange);
    this.#host.removeEventListener("wheel", this.#onWheel);
    this.#canvas.remove();
    this.#vertical.element.remove();
    this.#horizontal.element.remove();
    this.#corner.remove();
    this.#host.classList.remove("reticula-grid");
  }

  #notifySelection(): void {
    this.#options.onSelectionChange?.(this.#selection());
  }

  #selection(): Selection | null {
    const anchor = this.#anchor;
    const range = this.selectionRange;
    if (!anchor || !range) {
      return null;
    }
    const ranges = this.selectionRanges;
    const rowId = this.#cache.rowIdAt(anchor.row);
    const colId = this.#cache.colIdAt(anchor.col);
    const startName = `${columnName(range.firstCol)}${range.firstRow + 1}`;
    const single =
      ranges.length === 1 &&
      range.endRow - range.firstRow === 1 &&
      range.endCol - range.firstCol === 1;
    const baseName = single
      ? `${columnName(anchor.col)}${anchor.row + 1}`
      : `${startName}:${columnName(range.endCol - 1)}${range.endRow}`;
    const name = ranges.length > 1 ? `${baseName} (+${ranges.length - 1})` : baseName;
    return {
      row: anchor.row,
      col: anchor.col,
      range,
      ranges,
      scope: this.#scope,
      name,
      rowId: rowId === undefined ? null : String(rowId),
      colId: colId === undefined ? null : String(colId),
      text: this.textAt(anchor.row, anchor.col),
    };
  }

  #effectiveMargin(now: number): { readonly rows: number; readonly cols: number } {
    if (this.#fixedMargin) {
      return this.#fixedMargin;
    }
    return now < this.#fastScrollUntil ? FAST_MARGIN : IDLE_MARGIN;
  }

  #invalidate(): void {
    if (this.#frameRequest === 0 && !this.#destroyed) {
      this.#frameRequest = requestAnimationFrame(this.#frame);
    }
  }

  #frame = (): void => {
    this.#frameRequest = 0;
    const started = performance.now();
    if (this.#resized) {
      this.#resize();
    }
    if (this.#size.width <= 0 || this.#size.height <= 0) {
      return;
    }

    const metrics = this.#metrics;
    const visible = visibleCells(metrics, this.#sheet, this.#scroll, this.#size, this.#layout);
    this.#visible = visible;
    const wantedPages = pagesToHold(visible, this.#sheet, this.#effectiveMargin(started));
    const freezeKey = `${this.#frozenRows}:${this.#frozenCols}`;
    const freezeChanged = freezeKey !== this.#lastFreezeKey;
    if (
      freezeChanged ||
      (!this.#fixedMargin &&
        started >= this.#fastScrollUntil &&
        !isEmptySpanSubset(wantedPages, this.#cache.span))
    ) {
      this.#hold(wantedPages, freezeChanged);
    } else if (this.#fixedMargin || started < this.#fastScrollUntil || this.#cache.size === 0) {
      this.#hold(wantedPages, freezeChanged);
    }

    const counts = this.#renderer.draw({
      size: this.#size,
      scale: this.#scale,
      metrics,
      theme: this.#theme,
      scroll: this.#drawnScroll(),
      visible,
      cache: this.#cache,
      selection: this.#anchor,
      selectionRange: this.selectionRange,
      selectionRanges: this.selectionRanges,
      sheet: this.#sheet,
      layout: this.#layout,
      styles: this.#styles,
      formulaReferences: this.#formulaRefs,
      remotePresences: this.#remotePresences,
      fillPreview: this.#fillPreview,
      clipboardRange: this.#clipboardRange,
      reorderGuide: this.#reorderGuide,
      showFillHandle: this.#extraRanges.length === 0,
    });

    const content = contentSize(metrics, this.#sheet, this.#layout);
    const area = cellArea(metrics, this.#size);
    this.#vertical.update(content.height, area.height, this.#scroll.top);
    this.#horizontal.update(content.width, area.width, this.#scroll.left);

    this.#frames++;
    this.#cellsDrawn = counts.cells;
    this.#missingCells = counts.missing;
    if (counts.missing > 0) {
      this.#framesWithMissing++;
    }
    const finished = performance.now();
    this.#lastWork = finished - started + this.#pendingWork;
    this.#pendingWork = 0;
    this.#work[this.#workCount % FRAME_SAMPLES] = this.#lastWork;
    this.#workCount++;
    if (this.#frameListeners.size > 0) {
      const report: FrameReport = {
        time: finished,
        work: this.#lastWork,
        visible,
        cells: counts.cells,
        missing: counts.missing,
      };
      for (const listener of this.#frameListeners) {
        listener(report);
      }
    }
  };

  #drawnScroll(): Scroll {
    const scale = this.#scale;
    return {
      top: Math.round(this.#scroll.top * scale) / scale,
      left: Math.round(this.#scroll.left * scale) / scale,
    };
  }

  #hold(pages: Span, force = false): void {
    const freezeKey = `${this.#frozenRows}:${this.#frozenCols}`;
    if (!force && sameSpan(pages, this.#cache.span)) {
      return;
    }
    this.#lastFreezeKey = freezeKey;
    this.#cache.retain(pages, this.#frozenRows, this.#frozenCols);
    const cells = cellsOfPages(pages, this.#sheet);
    this.#epoch = (this.#epoch + 1) >>> 0;
    this.#options.subscribe({
      epoch: this.#epoch,
      firstRow: cells.firstRow,
      rowCount: cells.endRow - cells.firstRow,
      firstCol: cells.firstCol,
      colCount: cells.endCol - cells.firstCol,
    });
  }

  #resize(): void {
    this.#resized = false;
    const width = this.#host.clientWidth;
    const height = this.#host.clientHeight;
    const scale = window.devicePixelRatio || 1;
    this.#size = { width, height };
    this.#scale = scale;
    const pixelWidth = Math.round(width * scale);
    const pixelHeight = Math.round(height * scale);
    if (this.#canvas.width !== pixelWidth || this.#canvas.height !== pixelHeight) {
      this.#canvas.width = pixelWidth;
      this.#canvas.height = pixelHeight;
    }
    this.#canvas.style.width = `${width}px`;
    this.#canvas.style.height = `${height}px`;
    this.#scroll = clampScroll(
      this.#scroll,
      scrollLimits(this.#metrics, this.#sheet, this.#size, this.#layout),
    );
  }

  #placeScrollbars(): void {
    const thickness = `${this.#metrics.scrollbarSize}px`;
    Object.assign(this.#vertical.element.style, {
      top: "0",
      right: "0",
      bottom: thickness,
      width: thickness,
    });
    Object.assign(this.#horizontal.element.style, {
      left: "0",
      right: thickness,
      bottom: "0",
      height: thickness,
    });
    Object.assign(this.#corner.style, { width: thickness, height: thickness });
  }

  #restyle = (): void => {
    this.#theme = readTheme(this.#host);
    this.#invalidate();
  };

  #watchDensity(): void {
    this.#density?.removeEventListener("change", this.#onDensityChange);
    this.#density = matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
    this.#density.addEventListener("change", this.#onDensityChange);
  }

  #onDensityChange = (): void => {
    this.#watchDensity();
    this.#resized = true;
    this.#invalidate();
  };

  #onWheel = (event: WheelEvent): void => {
    if (event.ctrlKey) {
      return;
    }
    const started = performance.now();
    event.preventDefault();
    const area = cellArea(this.#metrics, this.#size);
    const unit = (extent: number) =>
      event.deltaMode === WheelEvent.DOM_DELTA_LINE
        ? WHEEL_LINE
        : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
          ? extent
          : 1;
    let left = event.deltaX * unit(area.width);
    let top = event.deltaY * unit(area.height);
    if (event.shiftKey && left === 0) {
      [left, top] = [top, 0];
    }
    this.scrollTo({ top: this.#scroll.top + top, left: this.#scroll.left + left });
    this.#pendingWork += performance.now() - started;
  };

  #isOverFillHandle(localX: number, localY: number): boolean {
    const range = this.selectionRange;
    if (!range || this.#extraRanges.length > 0) {
      return false;
    }
    const br = this.boxOf(range.endRow - 1, range.endCol - 1);
    const hx = br.left + br.width;
    const hy = br.top + br.height;
    const hit = FILL_HANDLE_SIZE + 2;
    return Math.abs(localX - hx) <= hit && Math.abs(localY - hy) <= hit;
  }

  #cellAtClamped(localX: number, localY: number): { row: number; col: number } | null {
    if (this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      return null;
    }
    const area = cellArea(this.#metrics, this.#size);
    if (area.width <= 0 || area.height <= 0) {
      return null;
    }
    const clampedX = Math.min(area.width - 1, Math.max(0, localX - this.#metrics.headerWidth));
    const clampedY = Math.min(area.height - 1, Math.max(0, localY - this.#metrics.headerHeight));
    const fz = frozenPixels(this.#metrics, this.#sheet, this.#size, this.#layout);
    const sheetY = clampedY < fz.height ? clampedY : this.#scroll.top + clampedY;
    const sheetX = clampedX < fz.width ? clampedX : this.#scroll.left + clampedX;
    const row = Math.min(
      this.#sheet.rows - 1,
      Math.max(0, this.#layout.rowAxis.indexAtOffset(sheetY)),
    );
    const col = Math.min(
      this.#sheet.cols - 1,
      Math.max(0, this.#layout.colAxis.indexAtOffset(sheetX)),
    );
    return { row, col };
  }

  #onPointerDown = (event: PointerEvent): void => {
    if (event.button !== 0) {
      return;
    }
    const started = performance.now();
    const bounds = this.#canvas.getBoundingClientRect();
    const x = event.clientX - bounds.left;
    const y = event.clientY - bounds.top;
    const multiMod = event.ctrlKey || event.metaKey;

    // 1. Check fill handle first!
    const curRange = this.selectionRange;
    if (curRange && this.#isOverFillHandle(x, y)) {
      this.#host.focus({ preventScroll: true });
      this.#drag = { kind: "fill", src: curRange };
      this.#fillPreview = curRange;
      this.#beginPointerCapture(event);
      this.#invalidate();
      this.#pendingWork += performance.now() - started;
      return;
    }

    // 2. Check header strips (corner, resize handles, whole-row/col select or drag reorder).
    const hit = headerHitAt(
      this.#metrics,
      this.#sheet,
      this.#scroll,
      this.#size,
      x,
      y,
      this.#layout,
    );
    if (hit) {
      this.#host.focus({ preventScroll: true });
      if (hit.kind === "corner") {
        this.selectAll();
      } else if (hit.kind === "colResize") {
        this.#drag = {
          kind: "colResize",
          col: hit.col,
          startClientX: event.clientX,
          startWidth: this.#layout.colAxis.sizeOf(hit.col),
        };
        this.#beginPointerCapture(event);
      } else if (hit.kind === "rowResize") {
        this.#drag = {
          kind: "rowResize",
          row: hit.row,
          startClientY: event.clientY,
          startHeight: this.#layout.rowAxis.sizeOf(hit.row),
        };
        this.#beginPointerCapture(event);
      } else if (hit.kind === "col") {
        // If clicking an already-selected whole column without Shift/Ctrl, allow dragging to reorder!
        if (
          !event.shiftKey &&
          !multiMod &&
          this.#scope === "cols" &&
          curRange &&
          hit.col >= curRange.firstCol &&
          hit.col < curRange.endCol
        ) {
          this.#drag = { kind: "colReorder", startCol: hit.col, moved: false };
          this.#beginPointerCapture(event);
        } else {
          if (multiMod && curRange) {
            this.#extraRanges.push(curRange);
          } else if (!event.shiftKey) {
            this.#extraRanges = [];
          }
          this.#scope = "cols";
          if (event.shiftKey && this.#anchor) {
            this.#focus = { row: this.#sheet.rows - 1, col: hit.col };
          } else {
            this.#anchor = { row: 0, col: hit.col };
            this.#focus = { row: this.#sheet.rows - 1, col: hit.col };
          }
          this.#drag = { kind: "select", mode: "cols" };
          this.#beginPointerCapture(event);
          this.#notifySelection();
          this.#invalidate();
        }
      } else if (hit.kind === "row") {
        if (
          !event.shiftKey &&
          !multiMod &&
          this.#scope === "rows" &&
          curRange &&
          hit.row >= curRange.firstRow &&
          hit.row < curRange.endRow
        ) {
          this.#drag = { kind: "rowReorder", startRow: hit.row, moved: false };
          this.#beginPointerCapture(event);
        } else {
          if (multiMod && curRange) {
            this.#extraRanges.push(curRange);
          } else if (!event.shiftKey) {
            this.#extraRanges = [];
          }
          this.#scope = "rows";
          if (event.shiftKey && this.#anchor) {
            this.#focus = { row: hit.row, col: this.#sheet.cols - 1 };
          } else {
            this.#anchor = { row: hit.row, col: 0 };
            this.#focus = { row: hit.row, col: this.#sheet.cols - 1 };
          }
          this.#drag = { kind: "select", mode: "rows" };
          this.#beginPointerCapture(event);
          this.#notifySelection();
          this.#invalidate();
        }
      }
      this.#pendingWork += performance.now() - started;
      return;
    }

    // 3. Cell body click / drag.
    const cell = cellAt(this.#metrics, this.#sheet, this.#scroll, this.#size, x, y, this.#layout);
    if (cell) {
      // Formula point-and-click reference insertion check (`14-m1b.md` 6.1):
      const initialSpan: Span = {
        firstRow: cell.row,
        endRow: cell.row + 1,
        firstCol: cell.col,
        endCol: cell.col + 1,
      };
      if (this.#options.onPointAndClickRange?.(initialSpan)) {
        event.preventDefault();
        this.#drag = { kind: "pointRef", anchor: cell };
        this.#beginPointerCapture(event);
        this.#pendingWork += performance.now() - started;
        return;
      }

      this.#host.focus({ preventScroll: true });
      if (event.shiftKey && this.#anchor) {
        this.#scope = "cells";
        this.#focus = cell;
        this.#notifySelection();
        this.#invalidate();
      } else if (multiMod && curRange) {
        this.#extraRanges.push(curRange);
        this.#scope = "cells";
        this.#anchor = cell;
        this.#focus = cell;
        this.#notifySelection();
        this.#invalidate();
      } else {
        this.select(cell);
      }
      this.#drag = { kind: "select", mode: "cells" };
      this.#beginPointerCapture(event);
    }
    this.#pendingWork += performance.now() - started;
  };

  #beginPointerCapture(event: PointerEvent): void {
    this.#draggingPointerId = event.pointerId;
    this.#dragClientX = event.clientX;
    this.#dragClientY = event.clientY;
    try {
      this.#canvas.setPointerCapture(event.pointerId);
    } catch {
      // Synthetic pointer events in unit tests may lack capture.
    }
  }

  #onPointerMove = (event: PointerEvent): void => {
    const bounds = this.#canvas.getBoundingClientRect();
    const localX = event.clientX - bounds.left;
    const localY = event.clientY - bounds.top;

    if (this.#draggingPointerId !== event.pointerId || !this.#drag) {
      this.#updateHoverCursor(localX, localY);
      return;
    }

    this.#dragClientX = event.clientX;
    this.#dragClientY = event.clientY;
    const drag = this.#drag;

    if (drag.kind === "colResize") {
      const nextW = Math.min(
        MAX_COL_PX,
        Math.max(MIN_COL_PX, Math.round(drag.startWidth + (event.clientX - drag.startClientX))),
      );
      this.#setPreviewColSize(drag.col, nextW);
      this.#invalidate();
      return;
    }
    if (drag.kind === "rowResize") {
      const nextH = Math.min(
        MAX_ROW_PX,
        Math.max(MIN_ROW_PX, Math.round(drag.startHeight + (event.clientY - drag.startClientY))),
      );
      this.#setPreviewRowSize(drag.row, nextH);
      this.#invalidate();
      return;
    }
    if (drag.kind === "rowReorder") {
      drag.moved = true;
      const target = this.#cellAtClamped(this.#metrics.headerWidth + 4, localY);
      if (target) {
        const box = this.boxOf(target.row, 0);
        const beforeRow = localY > box.top + box.height / 2 ? target.row + 1 : target.row;
        this.#reorderGuide = { axis: "row", beforeRow };
        this.#invalidate();
      }
      this.#syncAutoScroll();
      return;
    }
    if (drag.kind === "colReorder") {
      drag.moved = true;
      const target = this.#cellAtClamped(localX, this.#metrics.headerHeight + 4);
      if (target) {
        const box = this.boxOf(0, target.col);
        const beforeCol = localX > box.left + box.width / 2 ? target.col + 1 : target.col;
        this.#reorderGuide = { axis: "col", beforeCol };
        this.#invalidate();
      }
      this.#syncAutoScroll();
      return;
    }

    this.#updateDragTarget();
    this.#syncAutoScroll();
  };

  #onPointerUp = (event: PointerEvent): void => {
    if (this.#draggingPointerId !== event.pointerId) {
      return;
    }
    const finishedDrag = this.#drag;
    this.#draggingPointerId = null;
    this.#drag = null;
    this.#stopAutoScroll();
    try {
      if (this.#canvas.hasPointerCapture(event.pointerId)) {
        this.#canvas.releasePointerCapture(event.pointerId);
      }
    } catch {
      // Ignore synthetic pointer release errors.
    }

    if (!finishedDrag) {
      return;
    }

    if (finishedDrag.kind === "colResize") {
      const px = this.#layout.colAxis.sizeOf(finishedDrag.col);
      const colId = this.#cache.colIdAt(finishedDrag.col);
      if (colId !== undefined) {
        this.#options.onResizeCols?.([
          {
            run: { firstId: String(colId), len: 1 },
            sizePx: px === this.#metrics.colWidth ? null : px,
          },
        ]);
      }
    } else if (finishedDrag.kind === "rowResize") {
      const px = this.#layout.rowAxis.sizeOf(finishedDrag.row);
      const rowId = this.#cache.rowIdAt(finishedDrag.row);
      if (rowId !== undefined) {
        this.#options.onResizeRows?.([
          {
            run: { firstId: String(rowId), len: 1 },
            sizePx: px === this.#metrics.rowHeight ? null : px,
          },
        ]);
      }
    } else if (finishedDrag.kind === "fill") {
      const preview = this.#fillPreview;
      this.#fillPreview = null;
      this.#invalidate();
      if (preview && !sameSpan(preview, finishedDrag.src)) {
        this.#options.onFillRange?.(finishedDrag.src, preview);
      }
    } else if (finishedDrag.kind === "rowReorder") {
      const guide = this.#reorderGuide;
      this.#reorderGuide = null;
      this.#invalidate();
      if (!finishedDrag.moved) {
        this.#extraRanges = [];
        this.#scope = "rows";
        this.#anchor = { row: finishedDrag.startRow, col: 0 };
        this.#focus = { row: finishedDrag.startRow, col: this.#sheet.cols - 1 };
        this.#notifySelection();
      } else if (guide && guide.axis === "row") {
        const range = this.selectionRange;
        const runs = this.selectedRowRuns();
        if (range && runs && (guide.beforeRow < range.firstRow || guide.beforeRow > range.endRow)) {
          const afterId = guide.beforeRow === 0 ? null : this.rowIdAt(guide.beforeRow - 1);
          if (guide.beforeRow === 0 || afterId !== null) {
            this.#options.onMoveRows?.(runs, afterId);
          }
        }
      }
    } else if (finishedDrag.kind === "colReorder") {
      const guide = this.#reorderGuide;
      this.#reorderGuide = null;
      this.#invalidate();
      if (!finishedDrag.moved) {
        this.#extraRanges = [];
        this.#scope = "cols";
        this.#anchor = { row: 0, col: finishedDrag.startCol };
        this.#focus = { row: this.#sheet.rows - 1, col: finishedDrag.startCol };
        this.#notifySelection();
      } else if (guide && guide.axis === "col") {
        const range = this.selectionRange;
        const runs = this.selectedColRuns();
        if (range && runs && (guide.beforeCol < range.firstCol || guide.beforeCol > range.endCol)) {
          const afterId = guide.beforeCol === 0 ? null : this.colIdAt(guide.beforeCol - 1);
          if (guide.beforeCol === 0 || afterId !== null) {
            this.#options.onMoveCols?.(runs, afterId);
          }
        }
      }
    }
  };

  #onDoubleClick = (event: MouseEvent): void => {
    const bounds = this.#canvas.getBoundingClientRect();
    const x = event.clientX - bounds.left;
    const y = event.clientY - bounds.top;

    // 1. Double-click fill handle -> auto-fill down to adjacent table extent!
    const curRange = this.selectionRange;
    if (curRange && this.#isOverFillHandle(x, y)) {
      this.#options.onAutoFillDown?.(curRange);
      return;
    }

    // 2. Double-click column resize border -> auto-fit column width to visible text!
    const hit = headerHitAt(
      this.#metrics,
      this.#sheet,
      this.#scroll,
      this.#size,
      x,
      y,
      this.#layout,
    );
    if (hit && hit.kind === "colResize") {
      const colId = this.#cache.colIdAt(hit.col);
      if (colId !== undefined) {
        const fz = frozenPixels(this.#metrics, this.#sheet, this.#size, this.#layout);
        const startR = fz.frozenRows > 0 ? 0 : this.#visible.firstRow;
        const endR = Math.max(fz.frozenRows, this.#visible.endRow);
        const fitPx = this.#renderer.measureColumnFitWidth(
          this.#theme,
          this.#cache,
          this.#styles,
          hit.col,
          startR,
          endR,
        );
        this.#setPreviewColSize(hit.col, fitPx);
        this.#invalidate();
        this.#options.onResizeCols?.([
          {
            run: { firstId: String(colId), len: 1 },
            sizePx: fitPx === this.#metrics.colWidth ? null : fitPx,
          },
        ]);
      }
      return;
    }

    // 3. Double-click cell -> open editor!
    const cell = cellAt(this.#metrics, this.#sheet, this.#scroll, this.#size, x, y, this.#layout);
    if (cell) {
      this.select(cell);
      this.#options.onActivateEdit?.(cell);
    }
  };

  #onContextMenu = (event: MouseEvent): void => {
    if (!this.#options.onContextMenu) {
      return;
    }
    const bounds = this.#canvas.getBoundingClientRect();
    const x = event.clientX - bounds.left;
    const y = event.clientY - bounds.top;
    const hit = headerHitAt(
      this.#metrics,
      this.#sheet,
      this.#scroll,
      this.#size,
      x,
      y,
      this.#layout,
    );
    if (hit && hit.kind === "row") {
      event.preventDefault();
      const range = this.selectionRange;
      if (!range || this.#scope !== "rows" || hit.row < range.firstRow || hit.row >= range.endRow) {
        this.selectRange(
          { row: hit.row, col: 0 },
          { row: hit.row, col: this.#sheet.cols - 1 },
          "rows",
        );
      }
      this.#options.onContextMenu({
        clientX: event.clientX,
        clientY: event.clientY,
        zone: "rowHeader",
        row: hit.row,
        col: 0,
      });
      return;
    }
    if (hit && hit.kind === "col") {
      event.preventDefault();
      const range = this.selectionRange;
      if (!range || this.#scope !== "cols" || hit.col < range.firstCol || hit.col >= range.endCol) {
        this.selectRange(
          { row: 0, col: hit.col },
          { row: this.#sheet.rows - 1, col: hit.col },
          "cols",
        );
      }
      this.#options.onContextMenu({
        clientX: event.clientX,
        clientY: event.clientY,
        zone: "colHeader",
        row: 0,
        col: hit.col,
      });
      return;
    }
    const cell = cellAt(this.#metrics, this.#sheet, this.#scroll, this.#size, x, y, this.#layout);
    if (cell) {
      event.preventDefault();
      const range = this.selectionRange;
      const inside =
        range &&
        cell.row >= range.firstRow &&
        cell.row < range.endRow &&
        cell.col >= range.firstCol &&
        cell.col < range.endCol;
      if (!inside) {
        this.select(cell);
      }
      this.#options.onContextMenu({
        clientX: event.clientX,
        clientY: event.clientY,
        zone: "cell",
        row: cell.row,
        col: cell.col,
      });
    }
  };

  #updateHoverCursor(localX: number, localY: number): void {
    let cursor = "cell";
    if (this.#isOverFillHandle(localX, localY)) {
      cursor = "crosshair";
    } else {
      const hit = headerHitAt(
        this.#metrics,
        this.#sheet,
        this.#scroll,
        this.#size,
        localX,
        localY,
        this.#layout,
      );
      if (hit) {
        if (hit.kind === "colResize") {
          cursor = "col-resize";
        } else if (hit.kind === "rowResize") {
          cursor = "row-resize";
        } else if (
          hit.kind === "col" &&
          this.#scope === "cols" &&
          this.selectionRange &&
          hit.col >= this.selectionRange.firstCol &&
          hit.col < this.selectionRange.endCol
        ) {
          cursor = "grab";
        } else if (
          hit.kind === "row" &&
          this.#scope === "rows" &&
          this.selectionRange &&
          hit.row >= this.selectionRange.firstRow &&
          hit.row < this.selectionRange.endRow
        ) {
          cursor = "grab";
        } else {
          cursor = "pointer";
        }
      }
    }
    if (this.#canvas.style.cursor !== cursor) {
      this.#canvas.style.cursor = cursor;
    }
  }

  #setPreviewColSize(col: number, px: number): void {
    const filtered = this.#customCols.filter(([c]) => c !== col);
    if (px !== this.#metrics.colWidth) {
      filtered.push([col, px]);
      filtered.sort((a, b) => a[0] - b[0]);
    }
    this.#customCols = filtered;
    this.#rebuildLayout();
  }

  #setPreviewRowSize(row: number, px: number): void {
    const filtered = this.#customRows.filter(([r]) => r !== row);
    if (px !== this.#metrics.rowHeight) {
      filtered.push([row, px]);
      filtered.sort((a, b) => a[0] - b[0]);
    }
    this.#customRows = filtered;
    this.#rebuildLayout();
  }

  #updateDragTarget(): void {
    const drag = this.#drag;
    if (!drag) {
      return;
    }
    const bounds = this.#canvas.getBoundingClientRect();
    const lx = this.#dragClientX - bounds.left;
    const ly = this.#dragClientY - bounds.top;
    const next = this.#cellAtClamped(
      drag.kind === "select" && drag.mode === "rows" ? this.#metrics.headerWidth + 4 : lx,
      drag.kind === "select" && drag.mode === "cols" ? this.#metrics.headerHeight + 4 : ly,
    );
    if (!next) {
      return;
    }

    if (drag.kind === "pointRef") {
      const span: Span = {
        firstRow: Math.min(drag.anchor.row, next.row),
        endRow: Math.max(drag.anchor.row, next.row) + 1,
        firstCol: Math.min(drag.anchor.col, next.col),
        endCol: Math.max(drag.anchor.col, next.col) + 1,
      };
      this.#options.onPointAndClickRange?.(span);
      return;
    }

    if (drag.kind === "fill") {
      const src = drag.src;
      const dRow =
        next.row < src.firstRow
          ? src.firstRow - next.row
          : next.row >= src.endRow
            ? next.row - (src.endRow - 1)
            : 0;
      const dCol =
        next.col < src.firstCol
          ? src.firstCol - next.col
          : next.col >= src.endCol
            ? next.col - (src.endCol - 1)
            : 0;
      let preview = src;
      if (dRow > 0 || dCol > 0) {
        if (dRow >= dCol) {
          preview = {
            firstRow: Math.min(src.firstRow, next.row),
            endRow: Math.max(src.endRow, next.row + 1),
            firstCol: src.firstCol,
            endCol: src.endCol,
          };
        } else {
          preview = {
            firstRow: src.firstRow,
            endRow: src.endRow,
            firstCol: Math.min(src.firstCol, next.col),
            endCol: Math.max(src.endCol, next.col + 1),
          };
        }
      }
      if (!this.#fillPreview || !sameSpan(preview, this.#fillPreview)) {
        this.#fillPreview = preview;
        this.#invalidate();
      }
      return;
    }

    if (drag.kind === "select" && this.#anchor) {
      const targetFocus =
        drag.mode === "cols"
          ? { row: this.#sheet.rows - 1, col: next.col }
          : drag.mode === "rows"
            ? { row: next.row, col: this.#sheet.cols - 1 }
            : next;
      if (
        !this.#focus ||
        targetFocus.row !== this.#focus.row ||
        targetFocus.col !== this.#focus.col
      ) {
        this.#focus = targetFocus;
        this.#notifySelection();
        this.#invalidate();
      }
    }
  }

  #syncAutoScroll(): void {
    const bounds = this.#canvas.getBoundingClientRect();
    const area = cellArea(this.#metrics, this.#size);
    const localX = this.#dragClientX - bounds.left - this.#metrics.headerWidth;
    const localY = this.#dragClientY - bounds.top - this.#metrics.headerHeight;
    const outside = localX < 0 || localX >= area.width || localY < 0 || localY >= area.height;
    if (!outside) {
      this.#stopAutoScroll();
      return;
    }
    if (this.#autoScrollTimer !== 0) {
      return;
    }
    this.#autoScrollTimer = window.setInterval(() => {
      if (this.#draggingPointerId === null) {
        this.#stopAutoScroll();
        return;
      }
      const b = this.#canvas.getBoundingClientRect();
      const a = cellArea(this.#metrics, this.#size);
      const lx = this.#dragClientX - b.left - this.#metrics.headerWidth;
      const ly = this.#dragClientY - b.top - this.#metrics.headerHeight;
      const ignoreX = this.#drag?.kind === "select" && this.#drag.mode === "rows";
      const ignoreY = this.#drag?.kind === "select" && this.#drag.mode === "cols";
      const dTop = ignoreY
        ? 0
        : ly < 0
          ? -this.#metrics.rowHeight * 2
          : ly >= a.height
            ? this.#metrics.rowHeight * 2
            : 0;
      const dLeft = ignoreX
        ? 0
        : lx < 0
          ? -this.#metrics.colWidth
          : lx >= a.width
            ? this.#metrics.colWidth
            : 0;
      if (dTop !== 0 || dLeft !== 0) {
        this.scrollTo({ top: this.#scroll.top + dTop, left: this.#scroll.left + dLeft });
        this.#updateDragTarget();
      }
    }, 40);
  }

  #stopAutoScroll(): void {
    if (this.#autoScrollTimer !== 0) {
      window.clearInterval(this.#autoScrollTimer);
      this.#autoScrollTimer = 0;
    }
  }
}

function isEmptySpanSubset(inner: Span, outer: Span): boolean {
  if (inner.firstRow >= inner.endRow || inner.firstCol >= inner.endCol) {
    return outer.firstRow >= outer.endRow || outer.firstCol >= outer.endCol;
  }
  return (
    inner.firstRow >= outer.firstRow &&
    inner.endRow <= outer.endRow &&
    inner.firstCol >= outer.firstCol &&
    inner.endCol <= outer.endCol
  );
}
