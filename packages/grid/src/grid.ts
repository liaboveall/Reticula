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
   * How much to hold beyond what is visible, in multiples of the visible
   * range, on each side. More means fewer placeholders when scrolling fast
   * and more data in flight.
   */
  readonly margin?: { readonly rows: number; readonly cols: number };
  readonly metrics?: Metrics;
  /** Called at the end of every frame, for measurements. */
  readonly onFrame?: (frame: FrameReport) => void;
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

/** The cell the user picked. */
export interface Selection {
  readonly row: number;
  readonly col: number;
  /** Its name, such as `B7`. */
  readonly name: string;
  /**
   * The identifiers of its row and column, which is how a command names a
   * cell, and its text; `null` until the page that holds the cell arrives.
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
   * issuing draw calls, in milliseconds, over the most recent frames. It does
   * not include the browser's own rasterisation and compositing.
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

const DEFAULT_MARGIN = { rows: 2, cols: 1 };

/** How many recent frames the frame-work statistics cover. */
const FRAME_SAMPLES = 600;

/** Pixels per line and per page for wheels that report those units. */
const WHEEL_LINE = 40;

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
  #margin: { readonly rows: number; readonly cols: number };
  #frameListeners = new Set<(frame: FrameReport) => void>();

  #theme: Theme;
  #sheet: SheetSize = { rows: 0, cols: 0 };
  #size: Size = { width: 0, height: 0 };
  #scale = 1;
  #scroll: Scroll = { top: 0, left: 0 };
  #selected: { row: number; col: number } | null = null;
  #cache = new PageCache();
  #epoch = 0;

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
    this.#margin = options.margin ?? DEFAULT_MARGIN;
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
    this.#observer = new ResizeObserver(() => {
      this.#resized = true;
      this.#invalidate();
    });
    this.#observer.observe(host);
    this.#invalidate();
  }

  /** Tells the grid how large the sheet is. Call it when a document opens. */
  setSheet(sheet: SheetSize): void {
    this.#sheet = { rows: sheet.rows, cols: sheet.cols };
    this.#scroll = { top: 0, left: 0 };
    this.#selected = null;
    this.#cache.clear();
    this.#invalidate();
  }

  /** Takes a viewport page the engine sent. The buffer is the grid's from here on. */
  acceptPage(buffer: ArrayBuffer): void {
    const started = performance.now();
    const page = decodePage(buffer);
    this.#pagesReceived++;
    this.#bytesReceived += page.byteLength;
    if (this.#cache.accept(page)) {
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
      this.#scroll = next;
      this.#invalidate();
    }
  }

  /** Selects a cell, or nothing. */
  select(cell: { row: number; col: number } | null): void {
    this.#selected = cell ? { row: cell.row, col: cell.col } : null;
    this.#invalidate();
  }

  /** The selected cell. */
  get selection(): { readonly row: number; readonly col: number } | null {
    return this.#selected;
  }

  /**
   * Moves the selection by a number of rows and columns, stopping at the
   * edges of the sheet, and scrolls it into view. Without a selection it does
   * nothing.
   */
  moveSelection(rows: number, cols: number): void {
    const from = this.#selected;
    if (!from || this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      return;
    }
    const row = Math.min(this.#sheet.rows - 1, Math.max(0, from.row + rows));
    const col = Math.min(this.#sheet.cols - 1, Math.max(0, from.col + cols));
    this.#selected = { row, col };
    this.reveal(row, col);
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
   * was drawn and before the browser shows it: whatever the listener moves
   * stays in step with the cells. Returns a function that stops the calls.
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
    // The engine would otherwise go on counting this grid's pages as held,
    // and send none of them to the next grid.
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

  #selection(): Selection | null {
    const selected = this.#selected;
    if (!selected) {
      return null;
    }
    const page = this.#cache.pageOf(selected.row, selected.col);
    const rowId = page?.rowIds[selected.row - page.firstRow];
    const colId = page?.colIds[selected.col - page.firstCol];
    return {
      row: selected.row,
      col: selected.col,
      name: `${columnName(selected.col)}${selected.row + 1}`,
      rowId: rowId === undefined ? null : String(rowId),
      colId: colId === undefined ? null : String(colId),
      text: this.textAt(selected.row, selected.col),
    };
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
    this.#hold(pagesToHold(visible, this.#sheet, this.#margin));

    const counts = this.#renderer.draw({
      size: this.#size,
      scale: this.#scale,
      metrics,
      theme: this.#theme,
      scroll: this.#drawnScroll(),
      visible,
      cache: this.#cache,
      selection: this.#selected,
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

  /**
   * The scroll position that is drawn: the nearest one on the device's pixel
   * grid, which keeps text and lines sharp while the scroll position itself
   * moves smoothly.
   */
  #drawnScroll(): Scroll {
    const scale = this.#scale;
    return {
      top: Math.round(this.#scroll.top * scale) / scale,
      left: Math.round(this.#scroll.left * scale) / scale,
    };
  }

  /** Subscribes to a new span of pages when the one to hold has changed. */
  #hold(pages: Span): void {
    if (sameSpan(pages, this.#cache.span)) {
      return;
    }
    // Dropping what fell out of the span and announcing the new one happen
    // together: that is what keeps the engine's idea of what is held here
    // true. An empty span is announced like any other, or the engine would go
    // on counting pages as held that were just dropped.
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
    // Setting the size clears the canvas, so only do it when it changes.
    const pixelWidth = Math.round(width * scale);
    const pixelHeight = Math.round(height * scale);
    if (this.#canvas.width !== pixelWidth || this.#canvas.height !== pixelHeight) {
      this.#canvas.width = pixelWidth;
      this.#canvas.height = pixelHeight;
    }
    this.#canvas.style.width = `${width}px`;
    this.#canvas.style.height = `${height}px`;
    // A larger area can leave the scroll position past its new limit.
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

  /** Redraws when the page is zoomed or moves to a screen of another density. */
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
      // Zooming the page is the browser's business.
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
      // A plain wheel scrolls sideways while Shift is held.
      [left, top] = [top, 0];
    }
    this.scrollTo({ top: this.#scroll.top + top, left: this.#scroll.left + left });
    this.#pendingWork += performance.now() - started;
  };

  #onPointerDown = (event: PointerEvent): void => {
    if (event.button !== 0) {
      return;
    }
    const started = performance.now();
    this.#host.focus({ preventScroll: true });
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
    }
    this.#pendingWork += performance.now() - started;
  };
}
