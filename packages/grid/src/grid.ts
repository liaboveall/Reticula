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
  EMPTY_SPAN,
  type Metrics,
  pagesToHold,
  type Scroll,
  type SheetSize,
  type Size,
  type Span,
  sameSpan,
  scrollLimits,
  scrollToReveal,
  visibleCells,
} from "./layout.ts";
import { decodePage } from "./page.ts";
import { Renderer } from "./renderer.ts";
import { Scrollbar } from "./scrollbar.ts";
import { readTheme, type Theme } from "./theme.ts";

/** Contiguous run of row or column identifiers (`firstId .. firstId + len`). */
export interface IdRun {
  readonly firstId: string;
  readonly len: number;
}

/** A range of cells the grid asks the engine for. */
export interface Subscription {
  /** Chosen by the grid; the engine returns it on every page sent for this range. */
  readonly epoch: number;
  readonly firstRow: number;
  readonly rowCount: number;
  readonly firstCol: number;
  readonly colCount: number;
}

export interface GridOptions {
  /**
   * Called whenever the range of cells the grid wants to hold changes. The
   * pages that answer it are handed to {@link Grid.acceptPage}.
   */
  readonly subscribe: (subscription: Subscription) => void;
  /**
   * Fixed prefetch margin in multiples of the visible range. When omitted, the
   * grid adapts row margin dynamically between `0.5` (idle / slow scroll) and
   * `2.0` (rapid continuous scroll).
   */
  readonly margin?: { readonly rows: number; readonly cols: number };
  readonly metrics?: Metrics;
  /** Called at the end of every frame, for measurements. */
  readonly onFrame?: (frame: FrameReport) => void;
  /** Called whenever the active selection or its resolved IDs/text change. */
  readonly onSelectionChange?: (selection: Selection | null) => void;
  /** Called when the user double-clicks a cell to open edit mode. */
  readonly onActivateEdit?: (cell: { readonly row: number; readonly col: number }) => void;
}

/** What one frame did. */
export interface FrameReport {
  /** When the frame's draw calls had all been issued, on the clock of `performance.now()`. */
  readonly time: number;
  /** Main-thread work of the frame in milliseconds, as in {@link GridStats.frameWork}. */
  readonly work: number;
  /** The cells that were at least partly in view. */
  readonly visible: Span;
  /** Cells whose text was drawn. */
  readonly cells: number;
  /** Cells in view that were still placeholders. */
  readonly missing: number;
}

/** The active cell and rectangular selection chosen by the user. */
export interface Selection {
  /** Active anchor cell row (`0`-based). */
  readonly row: number;
  /** Active anchor cell column (`0`-based). */
  readonly col: number;
  /** Half-open normalized selection bounds (`firstRow..endRow`, `firstCol..endCol`). */
  readonly range: Span;
  /** Canonical A1 label: `"B7"` for one cell, `"B7:D10"` for a multi-cell range. */
  readonly name: string;
  /**
   * Identifiers of the active cell's row and column (`null` until its page arrives).
   */
  readonly rowId: string | null;
  readonly colId: string | null;
  readonly text: string | null;
}

/** A snapshot of what the grid is doing, for the status bar, tests and measurements. */
export interface GridStats {
  readonly sheet: SheetSize;
  readonly metrics: Metrics;
  readonly scroll: Scroll;
  readonly scrollLimits: Scroll;
  /** The cells at least partly in view in the last frame. */
  readonly visible: Span;
  /** Frames drawn. */
  readonly frames: number;
  /** Frames in which at least one cell in view was still a placeholder. */
  readonly framesWithMissing: number;
  /** Cells in view that were placeholders in the last frame. */
  readonly missingCells: number;
  /** Cells whose text was drawn in the last frame. */
  readonly cellsDrawn: number;
  readonly pagesHeld: number;
  readonly pagesReceived: number;
  /** Pages that arrived after the grid had stopped wanting them. */
  readonly pagesDiscarded: number;
  readonly bytesReceived: number;
  /** The latest subscription. */
  readonly epoch: number;
  /**
   * Time the main thread spent per frame handling input, receiving pages and
   * issuing draw calls, in milliseconds, over the most recent frames.
   */
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

/** How many recent frames the frame-work statistics cover. */
const FRAME_SAMPLES = 600;

/** Pixels per line and per page for wheels that report those units. */
const WHEEL_LINE = 40;

/** Coalesces an array of `bigint` IDs into contiguous `{ firstId, len }` runs. */
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

/**
 * A spreadsheet grid drawn on a canvas.
 *
 * The grid owns no data. It tells its host which range of cells it wants,
 * draws the pages it is given, and shows a placeholder for whatever has not
 * arrived yet. It depends on nothing but the DOM.
 */
export class Grid {
  #host: HTMLElement;
  #canvas: HTMLCanvasElement;
  #renderer: Renderer;
  #vertical: Scrollbar;
  #horizontal: Scrollbar;
  #corner: HTMLDivElement;
  #subscribe: (subscription: Subscription) => void;
  #metrics: Metrics;
  #fixedMargin: { readonly rows: number; readonly cols: number } | null;
  #frameListeners = new Set<(frame: FrameReport) => void>();
  #onSelectionChange: ((selection: Selection | null) => void) | undefined;
  #onActivateEdit: ((cell: { readonly row: number; readonly col: number }) => void) | undefined;

  #theme: Theme;
  #sheet: SheetSize = { rows: 0, cols: 0 };
  #size: Size = { width: 0, height: 0 };
  #scale = 1;
  #scroll: Scroll = { top: 0, left: 0 };
  #anchor: { row: number; col: number } | null = null;
  #focus: { row: number; col: number } | null = null;
  #cache = new PageCache();
  #epoch = 0;

  #lastVelocityTop = 0;
  #lastVelocityTime = 0;
  #fastScrollUntil = 0;

  #draggingPointerId: number | null = null;
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
  /** Work done since the last frame outside the frame itself. */
  #pendingWork = 0;
  #work = new Float64Array(FRAME_SAMPLES);
  #workCount = 0;
  #lastWork = 0;

  constructor(host: HTMLElement, options: GridOptions) {
    this.#host = host;
    this.#subscribe = options.subscribe;
    this.#metrics = options.metrics ?? DEFAULT_METRICS;
    this.#fixedMargin = options.margin ?? null;
    this.#onSelectionChange = options.onSelectionChange;
    this.#onActivateEdit = options.onActivateEdit;
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
    this.#observer = new ResizeObserver(() => {
      this.#resized = true;
      this.#invalidate();
    });
    this.#observer.observe(host);
    this.#invalidate();
  }

  /** Tells the grid how large a newly opened sheet is, resetting scroll and selection. */
  setSheet(sheet: SheetSize): void {
    this.#sheet = { rows: sheet.rows, cols: sheet.cols };
    this.#scroll = { top: 0, left: 0 };
    this.#anchor = null;
    this.#focus = null;
    this.#cache.clear();
    this.#notifySelection();
    this.#invalidate();
  }

  /**
   * Updates sheet dimensions after a structural operation (`InsertRows` /
   * `DeleteRows`) while preserving scroll position and clamping selection.
   */
  updateSheet(sheet: SheetSize): void {
    this.#sheet = { rows: sheet.rows, cols: sheet.cols };
    this.#scroll = clampScroll(this.#scroll, scrollLimits(this.#metrics, this.#sheet, this.#size));
    if (this.#anchor && (this.#sheet.rows <= 0 || this.#sheet.cols <= 0)) {
      this.#anchor = null;
      this.#focus = null;
    } else if (this.#anchor && this.#focus) {
      this.#anchor = {
        row: Math.min(this.#sheet.rows - 1, this.#anchor.row),
        col: Math.min(this.#sheet.cols - 1, this.#anchor.col),
      };
      this.#focus = {
        row: Math.min(this.#sheet.rows - 1, this.#focus.row),
        col: Math.min(this.#sheet.cols - 1, this.#focus.col),
      };
    }
    this.#notifySelection();
    this.#invalidate();
  }

  /** Takes a viewport page the engine sent. The buffer is the grid's from here on. */
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
    const limits = scrollLimits(this.#metrics, this.#sheet, this.#size);
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

  /** Selects a single cell, or clears selection when `null`. */
  select(cell: { row: number; col: number } | null): void {
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
  selectRange(anchor: { row: number; col: number }, focus: { row: number; col: number }): void {
    if (this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      return;
    }
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

  /** The active anchor cell (`null` when nothing is selected). */
  get selection(): { readonly row: number; readonly col: number } | null {
    return this.#anchor;
  }

  /** Normalized half-open span covering the rectangular selection. */
  get selectionRange(): Span | null {
    if (!this.#anchor || !this.#focus) {
      return null;
    }
    return {
      firstRow: Math.min(this.#anchor.row, this.#focus.row),
      endRow: Math.max(this.#anchor.row, this.#focus.row) + 1,
      firstCol: Math.min(this.#anchor.col, this.#focus.col),
      endCol: Math.max(this.#anchor.col, this.#focus.col) + 1,
    };
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

  /** Resolves contiguous `IdRun`s for all rows in the current selection, or `null` if uncached. */
  selectedRowRuns(): IdRun[] | null {
    const range = this.selectionRange;
    if (!range) {
      return null;
    }
    const ids: bigint[] = [];
    for (let r = range.firstRow; r < range.endRow; r++) {
      const id = this.#cache.rowIdAt(r);
      if (id === undefined) {
        return null;
      }
      ids.push(id);
    }
    return coalesceIdRuns(ids);
  }

  /** Resolves contiguous row and column `IdRun`s for clearing the current selection. */
  selectedCellRuns(): { readonly rows: IdRun[]; readonly cols: IdRun[] } | null {
    const range = this.selectionRange;
    if (!range) {
      return null;
    }
    const rows = this.selectedRowRuns();
    if (!rows) {
      return null;
    }
    const colIds: bigint[] = [];
    for (let c = range.firstCol; c < range.endCol; c++) {
      const id = this.#cache.colIdAt(c);
      if (id === undefined) {
        return null;
      }
      colIds.push(id);
    }
    return { rows, cols: coalesceIdRuns(colIds) };
  }

  /**
   * Moves the active selection by `(rows, cols)` collapsing any multi-cell
   * range, clamping to sheet edges, and scrolling into view.
   */
  moveSelection(rows: number, cols: number): void {
    const from = this.#anchor;
    if (!from || this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      return;
    }
    const row = Math.min(this.#sheet.rows - 1, Math.max(0, from.row + rows));
    const col = Math.min(this.#sheet.cols - 1, Math.max(0, from.col + cols));
    this.#anchor = { row, col };
    this.#focus = { row, col };
    this.reveal(row, col);
    this.#notifySelection();
    this.#invalidate();
  }

  /** Extends the rectangular selection's focus corner by `(rows, cols)` (`Shift+Arrow`). */
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

  /** Scrolls just far enough for a cell to be fully in view. */
  reveal(row: number, col: number): void {
    this.scrollTo(scrollToReveal(this.#metrics, this.#size, this.#scroll, row, col));
  }

  /**
   * Where a cell is drawn, in CSS pixels from the top left corner of the
   * grid's element. A cell that is scrolled out of view has a box outside it.
   */
  boxOf(row: number, col: number): Box {
    return cellBox(this.#metrics, this.#drawnScroll(), row, col);
  }

  /**
   * Calls `listener` at the end of every frame from now on, after the canvas
   * was drawn and before the browser shows it. Returns an unsubscribe function.
   */
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
      scrollLimits: scrollLimits(this.#metrics, this.#sheet, this.#size),
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

  /** Removes everything the grid added to its host and gives up its subscription. */
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
    this.#onSelectionChange?.(this.#selection());
  }

  #selection(): Selection | null {
    const anchor = this.#anchor;
    const range = this.selectionRange;
    if (!anchor || !range) {
      return null;
    }
    const rowId = this.#cache.rowIdAt(anchor.row);
    const colId = this.#cache.colIdAt(anchor.col);
    const startName = `${columnName(range.firstCol)}${range.firstRow + 1}`;
    const single = range.endRow - range.firstRow === 1 && range.endCol - range.firstCol === 1;
    const name = single
      ? `${columnName(anchor.col)}${anchor.row + 1}`
      : `${startName}:${columnName(range.endCol - 1)}${range.endRow}`;
    return {
      row: anchor.row,
      col: anchor.col,
      range,
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
    const visible = visibleCells(metrics, this.#sheet, this.#scroll, this.#size);
    this.#visible = visible;
    const wantedPages = pagesToHold(visible, this.#sheet, this.#effectiveMargin(started));
    // While decaying from fast-scroll margin back to idle margin without moving
    // outside held pages, avoid thrashing subscriptions until visible nears an edge.
    if (
      !this.#fixedMargin &&
      started >= this.#fastScrollUntil &&
      !isEmptySpanSubset(wantedPages, this.#cache.span)
    ) {
      this.#hold(wantedPages);
    } else if (this.#fixedMargin || started < this.#fastScrollUntil || this.#cache.size === 0) {
      this.#hold(wantedPages);
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
    });

    const content = contentSize(metrics, this.#sheet);
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

  #hold(pages: Span): void {
    if (sameSpan(pages, this.#cache.span)) {
      return;
    }
    this.#cache.retain(pages);
    const cells = cellsOfPages(pages, this.#sheet);
    this.#epoch = (this.#epoch + 1) >>> 0;
    this.#subscribe({
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
    this.#scroll = clampScroll(this.#scroll, scrollLimits(this.#metrics, this.#sheet, this.#size));
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
    const row = Math.min(
      this.#sheet.rows - 1,
      Math.max(0, Math.floor((this.#scroll.top + clampedY) / this.#metrics.rowHeight)),
    );
    const col = Math.min(
      this.#sheet.cols - 1,
      Math.max(0, Math.floor((this.#scroll.left + clampedX) / this.#metrics.colWidth)),
    );
    return { row, col };
  }

  #onPointerDown = (event: PointerEvent): void => {
    if (event.button !== 0) {
      return;
    }
    const started = performance.now();
    this.#host.focus({ preventScroll: true });
    const bounds = this.#canvas.getBoundingClientRect();
    const x = event.clientX - bounds.left;
    const y = event.clientY - bounds.top;
    const cell = cellAt(this.#metrics, this.#sheet, this.#scroll, this.#size, x, y);
    if (cell) {
      if (event.shiftKey && this.#anchor) {
        this.#focus = cell;
        this.#notifySelection();
        this.#invalidate();
      } else {
        this.select(cell);
      }
      this.#draggingPointerId = event.pointerId;
      this.#dragClientX = event.clientX;
      this.#dragClientY = event.clientY;
      try {
        this.#canvas.setPointerCapture(event.pointerId);
      } catch {
        // Synthetic pointer events in unit environments may lack capture.
      }
    }
    this.#pendingWork += performance.now() - started;
  };

  #onPointerMove = (event: PointerEvent): void => {
    if (this.#draggingPointerId !== event.pointerId || !this.#anchor) {
      return;
    }
    this.#dragClientX = event.clientX;
    this.#dragClientY = event.clientY;
    this.#updateDragFocus();
    this.#syncAutoScroll();
  };

  #onPointerUp = (event: PointerEvent): void => {
    if (this.#draggingPointerId !== event.pointerId) {
      return;
    }
    this.#draggingPointerId = null;
    this.#stopAutoScroll();
    try {
      if (this.#canvas.hasPointerCapture(event.pointerId)) {
        this.#canvas.releasePointerCapture(event.pointerId);
      }
    } catch {
      // Ignore synthetic pointer release errors.
    }
  };

  #onDoubleClick = (event: MouseEvent): void => {
    const bounds = this.#canvas.getBoundingClientRect();
    const cell = cellAt(
      this.#metrics,
      this.#sheet,
      this.#scroll,
      this.#size,
      event.clientX - bounds.left,
      event.clientY - bounds.top,
    );
    if (cell) {
      this.select(cell);
      this.#onActivateEdit?.(cell);
    }
  };

  #updateDragFocus(): void {
    if (!this.#anchor) {
      return;
    }
    const bounds = this.#canvas.getBoundingClientRect();
    const next = this.#cellAtClamped(
      this.#dragClientX - bounds.left,
      this.#dragClientY - bounds.top,
    );
    if (next && (!this.#focus || next.row !== this.#focus.row || next.col !== this.#focus.col)) {
      this.#focus = next;
      this.#notifySelection();
      this.#invalidate();
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
      const dTop =
        ly < 0 ? -this.#metrics.rowHeight * 2 : ly >= a.height ? this.#metrics.rowHeight * 2 : 0;
      const dLeft = lx < 0 ? -this.#metrics.colWidth : lx >= a.width ? this.#metrics.colWidth : 0;
      if (dTop !== 0 || dLeft !== 0) {
        this.scrollTo({ top: this.#scroll.top + dTop, left: this.#scroll.left + dLeft });
        this.#updateDragFocus();
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
