import { PageCache } from "./cache.ts";
import { type CellRect, type FilterDisplayState, FilterRowMap, filterButtonBox } from "./filter.ts";
import {
  type Box,
  type Corner,
  cellArea,
  cellAt,
  cellBox,
  cellsOfPages,
  clampScroll,
  columnName,
  contentSize,
  cornerOf,
  DEFAULT_METRICS,
  type EditorBox,
  EMPTY_SPAN,
  editorBox,
  frozenPixels,
  type HeaderHit,
  headerHitAt,
  isEmpty,
  isOverFillHandle,
  type Metrics,
  middleOf,
  type Point,
  pagesToHold,
  pointOfCell,
  reanchor,
  type Scroll,
  type SheetLayout,
  type SheetSize,
  type Size,
  type Span,
  SparseAxis,
  sameSpan,
  scrollLimits,
  scrollToCorner,
  scrollToReveal,
  type View,
  viewMetrics,
  visibleCells,
} from "./layout.ts";
import { decodePage } from "./page.ts";
import { Canvas2dRenderer } from "./render/canvas2d.ts";
import type {
  DiffHighlightMap,
  FormulaReferenceOverlay,
  Frame,
  GridCellStyle,
  RemotePresenceOverlay,
  ReorderGuide,
} from "./render/frame.ts";
import { TextMeasurer } from "./render/text-measure.ts";
import type { GridRenderer, RendererCounters, RendererKind } from "./render/types.ts";
import { type RendererPreference, type RendererReason, startUpgrade } from "./render/upgrade.ts";
import { Scrollbar } from "./scrollbar.ts";
import { readTheme, type Theme } from "./theme.ts";
import { clampZoom, nextZoom, ZoomGesture } from "./zoom.ts";

/** Contiguous run of row or column identifiers (`firstId .. firstId + len`). */
export interface IdRun {
  readonly firstId: string;
  readonly len: number;
}

/** Sheet dimensions + optional M1b sparse metrics, freeze counts, filter state, and style table. */
export interface SheetConfig extends SheetSize {
  readonly totalRows?: number;
  readonly filter?: FilterDisplayState | null | undefined;
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
  /** The unfrozen cells on screen, whose pages are wanted first. */
  readonly visible: Span;
  /** Whether the grid may miss any of the pages, so that all of them have to be sent again. */
  readonly resend: boolean;
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
  /** Sizes at a zoom of 1. */
  readonly metrics?: Metrics;
  /** Whether Ctrl with the wheel and pinches zoom the sheet, and not the page. */
  readonly zoom?: boolean;
  readonly onZoomChange?: (zoom: number) => void;
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
  readonly onFilterButtonClick?: (col: number, anchorRect: DOMRect) => void;
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

/** What of a sheet is in view and selected: what there is to remember of it. */
export interface GridView {
  readonly zoom: number;
  /** The first row and column in view that are not frozen, and how much of them is out of view. */
  readonly row: number;
  readonly rowFraction: number;
  readonly col: number;
  readonly colFraction: number;
  readonly selection: {
    readonly anchor: { readonly row: number; readonly col: number };
    readonly focus: { readonly row: number; readonly col: number };
    readonly scope: SelectionScope;
  } | null;
}

export interface GridStats {
  readonly sheet: SheetSize;
  readonly zoom: number;
  /** The sizes on the screen: those of the sheet, enlarged by the zoom. */
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
  readonly renderer: {
    readonly kind: RendererKind;
    /** Why it is not the one for WebGPU. */
    readonly reason: RendererReason | null;
    readonly adapter: string | null;
    readonly counters: RendererCounters | null;
  };
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
/** Turns of the wheel that follow each other within this time are one gesture. */
const ZOOM_GESTURE_MS = 200;
/** The smallest font that cells are edited in, in CSS pixels. */
const LEAST_EDITED_FONT = 11;

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

/** Rows or columns with a size of their own, in order. */
type CustomSizes = readonly (readonly [number, number])[];

function withSize(sizes: CustomSizes, index: number, px: number, standard: number): CustomSizes {
  const others = sizes.filter(([other]) => other !== index);
  return px === standard ? others : [...others, [index, px] as const].sort((a, b) => a[0] - b[0]);
}

function sizeIn(sizes: CustomSizes, index: number, standard: number): number {
  return sizes.find(([other]) => other === index)?.[1] ?? standard;
}

/**
 * The size an edge has been dragged to, as it is kept: at a zoom of 1. An edge
 * that has not moved has not changed it.
 */
function dragged(size: number, by: number, zoom: number, least: number, most: number): number {
  return by === 0 ? size : Math.min(most, Math.max(least, Math.round(size + by / zoom)));
}

interface Cell {
  readonly row: number;
  readonly col: number;
}

const PINCH_EVENTS = ["gesturestart", "gesturechange", "gestureend"] as const;
const KEY_EVENTS = ["keydown", "keyup", "blur"] as const;

type DragInteraction =
  | { readonly kind: "select"; readonly mode: SelectionScope }
  | {
      readonly kind: "pointRef";
      readonly anchor: { row: number; col: number };
      lastSpan: Span;
    }
  | { readonly kind: "fill"; readonly src: Span }
  | {
      readonly kind: "colResize";
      readonly col: number;
      readonly startClientX: number;
      readonly startBase: number;
    }
  | {
      readonly kind: "rowResize";
      readonly row: number;
      readonly startClientY: number;
      readonly startBase: number;
    }
  | { readonly kind: "rowReorder"; readonly startRow: number; moved: boolean }
  | { readonly kind: "colReorder"; readonly startCol: number; moved: boolean };

/**
 * A spreadsheet grid drawn on a canvas (`13-m1a.md` & `14-m1b.md` 6.2–6.3).
 */
export class Grid {
  #host: HTMLElement;
  #measurer = new TextMeasurer();
  #renderer: GridRenderer;
  /** The renderer that was replaced, shown until the one in its place has drawn. */
  #retired: GridRenderer | null = null;
  #rendererPreference: RendererPreference = "canvas2d";
  #rendererState: Pick<GridStats["renderer"], "reason" | "adapter"> = {
    reason: "preference",
    adapter: null,
  };
  #endUpgrade = () => {};
  /** What was drawn last. */
  #drawn: Frame | null = null;
  /** Where the pointer is listened to, whatever draws the picture below. */
  #input: HTMLDivElement;
  #vertical: Scrollbar;
  #horizontal: Scrollbar;
  #corner: HTMLDivElement;
  #options: GridOptions;
  /** The sizes of the workbook, and what they come to on the screen. */
  #base: Metrics;
  #metrics: Metrics;
  #zoom = 1;
  #gesture = new ZoomGesture();
  /** What a gesture has come to since the last frame, and where it is. */
  #zoomGoal: { readonly zoom: number; readonly point: Point } | null = null;
  #zoomTimer = 0;
  /** Whether fingers are on the trackpad, as Safari has it. */
  #pinching = false;
  /** Whether the key is held, which a pinch that comes as a turn of the wheel only claims. */
  #controlHeld = false;
  #fixedMargin: { readonly rows: number; readonly cols: number } | null;
  #frameListeners = new Set<(frame: FrameReport) => void>();

  #theme: Theme;
  #sheet: SheetSize = { rows: 0, cols: 0 };
  #customRows: CustomSizes = [];
  #customCols: CustomSizes = [];
  /** The size shown while a row's or column's edge is dragged, which nobody else knows of. */
  #sizePreview: {
    readonly axis: "row" | "col";
    readonly index: number;
    readonly px: number;
  } | null = null;
  #frozenRows = 0;
  #frozenCols = 0;
  #layout: SheetLayout;
  #styles: readonly GridCellStyle[] = [];
  #formulaRefs: readonly FormulaReferenceOverlay[] = [];
  #remotePresences: readonly RemotePresenceOverlay[] = [];
  #diffHighlights: DiffHighlightMap | null = null;
  #clipboardRange: { readonly span: Span; readonly cut: boolean } | null = null;
  #fillPreview: Span | null = null;
  #reorderGuide: ReorderGuide | null = null;
  #filterState: FilterDisplayState | null = null;
  #filterRowMap: FilterRowMap | null = null;
  #activeSpillRange: CellRect | null = null;

  #size: Size = { width: 0, height: 0 };
  #scale = 1;
  #scroll: Scroll = { top: 0, left: 0 };
  /** Where to scroll once there is a size to go by. */
  #scrollGoal: Corner | null = null;
  /** The place a sheet was opened at, while nothing has been scrolled, zoomed or changed since. */
  #openedAt: Corner | null = null;
  #anchor: { row: number; col: number } | null = null;
  #focus: { row: number; col: number } | null = null;
  #extraRanges: Span[] = [];
  #scope: SelectionScope = "cells";
  #cache = new PageCache();
  #epoch = 0;
  #lastFreezeKey = "0:0";
  /** Whether more pages than are wanted may be held. */
  #holdsTooMuch = false;

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
    this.#base = options.metrics ?? DEFAULT_METRICS;
    this.#scale = window.devicePixelRatio || 1;
    this.#metrics = viewMetrics(this.#base, this.#zoom, this.#scale);
    this.#layout = this.#makeLayout();
    this.#fixedMargin = options.margin ?? null;
    if (options.onFrame) {
      this.#frameListeners.add(options.onFrame);
    }

    host.classList.add("reticula-grid");
    host.tabIndex = 0;
    this.#renderer = this.#canvas2d();
    this.#renderer.canvas.setAttribute("aria-hidden", "true");
    this.#input = document.createElement("div");
    this.#input.className = "reticula-input";
    this.#vertical = new Scrollbar("vertical", (top) => this.scrollTo({ top }));
    this.#horizontal = new Scrollbar("horizontal", (left) => this.scrollTo({ left }));
    this.#corner = document.createElement("div");
    this.#corner.className = "reticula-scrollbar-corner";
    this.#placeScrollbars();
    const bars = [this.#vertical.element, this.#horizontal.element, this.#corner];
    for (const bar of bars) {
      bar.setAttribute("aria-hidden", "true");
    }
    host.append(this.#renderer.canvas, this.#input, ...bars);

    this.#theme = readTheme(host);
    this.#scheme = matchMedia("(prefers-color-scheme: dark)");
    this.#scheme.addEventListener("change", this.#restyle);
    this.#watchDensity();

    host.addEventListener("wheel", this.#onWheel, { passive: false });
    if (options.zoom) {
      for (const type of PINCH_EVENTS) {
        host.addEventListener(type, this.#onPinch, { passive: false });
      }
      for (const type of KEY_EVENTS) {
        window.addEventListener(type, this.#onControlKey, { capture: true });
      }
    }
    this.#input.addEventListener("pointerdown", this.#onPointerDown);
    this.#input.addEventListener("pointermove", this.#onPointerMove);
    this.#input.addEventListener("pointerup", this.#onPointerUp);
    this.#input.addEventListener("pointercancel", this.#onPointerLost);
    // Follows every release too, by when there is no drag left to abort.
    this.#input.addEventListener("lostpointercapture", this.#onPointerLost);
    this.#input.addEventListener("dblclick", this.#onDoubleClick);
    this.#input.addEventListener("contextmenu", this.#onContextMenu);
    this.#observer = new ResizeObserver(() => {
      this.#resized = true;
      this.#invalidate();
    });
    this.#observer.observe(host);
    this.#invalidate();
  }

  /**
   * Opens a new sheet, where it was left if that is known. Something is
   * selected then if there is anything to select: without a selection, the
   * keyboard would be of no use.
   */
  setSheet(sheet: SheetConfig, view: Partial<GridView> = {}): void {
    this.cancelDrag();
    this.#zoomGoal = null;
    this.#endZoomGesture();
    const zoom = clampZoom(view.zoom ?? this.#zoom, this.#zoom);
    const isZoomed = zoom !== this.#zoom;
    this.#zoom = zoom;
    this.#metrics = viewMetrics(this.#base, zoom, this.#scale);
    this.#sheet = { rows: sheet.rows, cols: sheet.cols };
    this.#applySheetConfig(sheet);
    this.#placeScroll({ top: 0, left: 0 });
    const { row = 0, rowFraction = 0, col = 0, colFraction = 0, selection } = view;
    const first = this.#cellWithin({ row, col });
    this.#scrollGoal = first && {
      row: { index: first.row, fraction: first.row === row ? rowFraction : 0 },
      col: { index: first.col, fraction: first.col === col ? colFraction : 0 },
    };
    this.#settleScroll();
    this.#anchor = selection ? this.#cellWithin(selection.anchor) : first;
    this.#focus = selection ? this.#cellWithin(selection.focus) : first;
    this.#extraRanges = [];
    this.#remotePresences = [];
    this.#scope = selection?.scope ?? "cells";
    this.#clipboardRange = null;
    this.#fillPreview = null;
    this.#reorderGuide = null;
    this.#activeSpillRange = null;
    if (sheet.filter === undefined) {
      this.#filterState = null;
      this.#filterRowMap = null;
    }
    this.#cache.clear();
    if (isZoomed) {
      this.#options.onZoomChange?.(zoom);
    }
    this.#notifySelection();
    this.#invalidate();
  }

  /** What `setSheet` takes to show the sheet the way it is shown now. */
  view(): GridView {
    const { row, col } = this.#scrollGoal ?? cornerOf(this.#viewNow());
    return {
      zoom: this.#zoom,
      row: row.index,
      rowFraction: row.fraction,
      col: col.index,
      colFraction: col.fraction,
      selection:
        this.#anchor && this.#focus
          ? { anchor: this.#anchor, focus: this.#focus, scope: this.#scope }
          : null,
    };
  }

  /** Updates sheet dimensions, metrics, freeze panes, and styles while preserving scroll & selection. */
  updateSheet(sheet: SheetConfig): void {
    this.#openedAt = null;
    const hadCells = this.#sheet.rows > 0 && this.#sheet.cols > 0;
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
    } else if (!this.#anchor && !hadCells) {
      this.#anchor = this.#cellWithin({ row: 0, col: 0 });
      this.#focus = this.#anchor;
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

  /** Sets or clears three-state diff highlights keyed by `"row:col"`. */
  setDiffHighlights(highlights: DiffHighlightMap | null): void {
    this.#diffHighlights = highlights;
    this.#invalidate();
  }

  /** Sets or clears the copied/cut marching-ants indicator rectangle. */
  setClipboardRange(range: Span | null, cut = false): void {
    this.#clipboardRange = range ? { span: range, cut } : null;
    this.#invalidate();
  }

  /** Sets or clears the worker-local AutoFilter display state and O(log H) row map. */
  setFilterState(filter: FilterDisplayState | null, totalRows?: number): void {
    this.#applyFilterState(filter, totalRows);
    this.#invalidate();
  }

  /** Sets or clears the dashed outline shown around an inspected dynamic spill array. */
  setActiveSpillRange(range: CellRect | null): void {
    this.#activeSpillRange = range;
    this.#invalidate();
  }

  /** Maps a 0-based visible display row index to its physical 0-based row index. */
  displayRowToPhysicalRow(displayRow: number): number {
    return this.#filterRowMap ? this.#filterRowMap.displayToPhysical(displayRow) : displayRow;
  }

  #applyFilterState(filter: FilterDisplayState | null, totalRows?: number): void {
    this.#filterState = filter;
    this.#filterRowMap = filter
      ? new FilterRowMap(totalRows ?? this.#sheet.rows, filter.hiddenRowSpans)
      : null;
  }

  #applySheetConfig(sheet: SheetConfig): void {
    if ("filter" in sheet) {
      this.#applyFilterState(sheet.filter ?? null, sheet.totalRows ?? sheet.rows);
    }
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

  #makeLayout(): SheetLayout {
    const { rowHeight, colWidth } = this.#base;
    const preview = this.#sizePreview;
    const axisOf = (axis: "row" | "col", sizes: CustomSizes, standard: number) => {
      const shown =
        preview?.axis === axis ? withSize(sizes, preview.index, preview.px, standard) : sizes;
      return new SparseAxis(standard, shown, this.#scale, this.#zoom);
    };
    return {
      rowAxis: axisOf("row", this.#customRows, rowHeight),
      colAxis: axisOf("col", this.#customCols, colWidth),
      frozenRows: this.#frozenRows,
      frozenCols: this.#frozenCols,
    };
  }

  #rebuildLayout(): void {
    this.#layout = this.#makeLayout();
  }

  /** What the pointer is over, and what is drawn, is in the view as it was last drawn. */
  #viewNow(scroll: Scroll = this.#drawnScroll()): View {
    return {
      metrics: this.#metrics,
      sheet: this.#sheet,
      size: this.#size,
      layout: this.#layout,
      scroll,
    };
  }

  #cellWithin(cell: { readonly row: number; readonly col: number }): Cell | null {
    if (this.#sheet.rows <= 0 || this.#sheet.cols <= 0) {
      return null;
    }
    return {
      row: Math.min(this.#sheet.rows - 1, Math.max(0, cell.row)),
      col: Math.min(this.#sheet.cols - 1, Math.max(0, cell.col)),
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
    this.#settleScroll();
    this.#scrollGoal = null;
    this.#openedAt = null;
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

  /** Scrolls to where the sheet was left, if that has not been possible before. */
  #settleScroll(): void {
    const goal = this.#scrollGoal;
    if (!goal) {
      return;
    }
    if (this.#resized) {
      this.#resize();
    }
    if (this.#size.width > 0 && this.#size.height > 0) {
      this.#scrollGoal = null;
      this.#placeScroll(scrollToCorner(this.#viewNow(), goal));
      this.#openedAt = goal;
    }
  }

  /** Scrolls without that being a movement: nothing says that it will go on. */
  #placeScroll(scroll: Scroll): void {
    this.#openedAt = null;
    this.#scroll = scroll;
    this.#lastVelocityTop = scroll.top;
    this.#lastVelocityTime = performance.now();
  }

  /** How much the sheet is enlarged. */
  get zoom(): number {
    return this.#zoom;
  }

  /**
   * Enlarges the sheet, with what is at a point of the grid's area staying
   * there. By default that is the selected cell, which stays in view as well,
   * or the middle if the cell is not in view. Returns whether it did: while
   * something is dragged, it does not.
   */
  setZoom(zoom: number, anchor?: Point): boolean {
    if (this.#drag || !Number.isFinite(zoom)) {
      return false;
    }
    this.#zoomGoal = null;
    this.#endZoomGesture();
    this.#settleScroll();
    const view = this.#viewNow();
    const cell = anchor ? null : this.#anchor;
    const point = cell && pointOfCell(view, cell);
    const wanted = clampZoom(Math.round(zoom * 100) / 100);
    if (this.#changeZoom(wanted, anchor ?? point ?? middleOf(view)) && cell && point) {
      const { metrics, size, sheet, layout, scroll } = this.#viewNow(this.#scroll);
      this.#placeScroll(scrollToReveal(metrics, size, scroll, cell.row, cell.col, sheet, layout));
    }
    return true;
  }

  zoomIn(): boolean {
    return this.setZoom(nextZoom(this.#zoom, 1));
  }

  zoomOut(): boolean {
    return this.setZoom(nextZoom(this.#zoom, -1));
  }

  zoomReset(): boolean {
    return this.setZoom(1);
  }

  #changeZoom(zoom: number, point: Point): boolean {
    if (zoom === this.#zoom) {
      return false;
    }
    this.#rescale(zoom, this.#scale, this.#size, point);
    this.#invalidate();
    this.#options.onZoomChange?.(zoom);
    return true;
  }

  /** Takes on other sizes, with the sheet staying where it is at a point or at its corner. */
  #rescale(zoom: number, scale: number, size: Size, point: Point | null): void {
    // Rounded, the place of a point would move with every step of a gesture.
    const before = this.#viewNow(point ? this.#scroll : this.#drawnScroll());
    this.#zoom = zoom;
    this.#scale = scale;
    this.#size = size;
    this.#metrics = viewMetrics(this.#base, zoom, scale);
    this.#rebuildLayout();
    const after = this.#viewNow(this.#scroll);
    this.#placeScroll(
      before.size.width > 0 && before.size.height > 0
        ? reanchor(before, after, point)
        : clampScroll(this.#scroll, scrollLimits(this.#metrics, this.#sheet, size, this.#layout)),
    );
  }

  /** Zooms with the next frame, which a gesture may have more to add to until then. */
  #zoomSoon(zoom: number | null, point: Point): void {
    if (zoom !== null) {
      this.#zoomGoal = { zoom, point };
      this.#invalidate();
    }
    window.clearTimeout(this.#zoomTimer);
    this.#zoomTimer = window.setTimeout(this.#endZoomGesture, ZOOM_GESTURE_MS);
  }

  #endZoomGesture = (): void => {
    window.clearTimeout(this.#zoomTimer);
    this.#zoomTimer = 0;
    this.#gesture.end();
    if (this.#zoomGoal && !this.#drag) {
      this.#changeZoom(this.#zoomGoal.zoom, this.#zoomGoal.point);
    }
    this.#zoomGoal = null;
    // Zooming in leaves pages held that are far out of view.
    this.#holdsTooMuch = true;
    this.#invalidate();
  };

  /** Selects a single cell, clearing extra ranges. */
  select(cell: { row: number; col: number } | null): void {
    this.#extraRanges = [];
    this.#scope = "cells";
    this.#anchor = cell && this.#cellWithin(cell);
    this.#focus = this.#anchor;
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
    this.#settleScroll();
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

  /**
   * Where the editor of a cell is, shown or not. Other than the cell itself
   * it is never too small to read what is in it.
   */
  editorBoxOf(row: number, col: number, shown: boolean): EditorBox {
    return editorBox(
      this.#metrics,
      this.#size,
      this.boxOf(row, col),
      this.#zoom,
      Math.min(1, LEAST_EDITED_FONT / this.#theme.fontSize),
      this.#scale,
      shown,
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
      zoom: this.#zoom,
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
      renderer: {
        kind: this.#renderer.kind,
        ...this.#rendererState,
        counters: this.#renderer.counters ?? null,
      },
    };
  }

  /**
   * Ends the drag in progress as if it had not begun: nothing is resized, moved
   * or filled. Cells it has selected stay selected. Returns whether there was one.
   */
  cancelDrag(): boolean {
    const drag = this.#drag;
    const pointer = this.#draggingPointerId;
    if (!drag || pointer === null) {
      return false;
    }
    this.#drag = null;
    this.#draggingPointerId = null;
    this.#stopAutoScroll();
    this.#releasePointer(pointer);
    this.#fillPreview = null;
    this.#reorderGuide = null;
    if (this.#sizePreview) {
      this.#sizePreview = null;
      this.#rebuildLayout();
    }
    this.#invalidate();
    return true;
  }

  /**
   * Chooses what draws the grid. WebGPU takes over from the 2D canvas once it
   * is ready to, and hands back to another one when it is lost.
   */
  setRenderer(preference: RendererPreference): void {
    if (this.#destroyed || preference === this.#rendererPreference) {
      return;
    }
    this.#rendererPreference = preference;
    this.#endUpgrade();
    if (this.#renderer.kind !== "canvas2d") {
      this.#swapRenderer(null);
    }
    this.#endUpgrade = startUpgrade(
      {
        measure: this.#measurer,
        frame: () => this.#drawn,
        invalidate: () => this.#invalidate(),
        swap: (renderer) => this.#swapRenderer(renderer),
        report: (reason, adapter = null) => {
          this.#rendererState = { reason, adapter };
        },
      },
      preference,
    );
  }

  #canvas2d(): GridRenderer {
    return new Canvas2dRenderer({
      measure: this.#measurer,
      onRestored: () => this.#invalidate(),
    });
  }

  /**
   * Has another renderer draw from the next frame on; without one, a new 2D
   * canvas. Refused, with `false`, while something is dragged, unless what drew
   * so far cannot go on.
   */
  #swapRenderer(next: GridRenderer | null): boolean {
    if (this.#destroyed || (next && this.#drag)) {
      return false;
    }
    const renderer = next ?? this.#canvas2d();
    this.#retired?.destroy();
    this.#retired = this.#renderer;
    this.#renderer = renderer;
    renderer.canvas.setAttribute("aria-hidden", "true");
    this.#input.before(renderer.canvas);
    this.#resize();
    this.#invalidate();
    return true;
  }

  destroy(): void {
    this.#stopAutoScroll();
    window.clearTimeout(this.#zoomTimer);
    this.#hold(EMPTY_SPAN);
    this.#destroyed = true;
    this.#frameListeners.clear();
    cancelAnimationFrame(this.#frameRequest);
    this.#observer.disconnect();
    this.#scheme.removeEventListener("change", this.#restyle);
    this.#density?.removeEventListener("change", this.#onDensityChange);
    this.#host.removeEventListener("wheel", this.#onWheel);
    for (const type of PINCH_EVENTS) {
      this.#host.removeEventListener(type, this.#onPinch);
    }
    for (const type of KEY_EVENTS) {
      window.removeEventListener(type, this.#onControlKey, { capture: true });
    }
    this.#endUpgrade();
    this.#renderer.destroy();
    this.#retired?.destroy();
    this.#input.remove();
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
    this.#settleScroll();
    if (this.#zoomGoal) {
      const { zoom, point } = this.#zoomGoal;
      this.#zoomGoal = null;
      if (!this.#drag) {
        this.#changeZoom(zoom, point);
      }
    }

    const metrics = this.#metrics;
    const scroll = this.#drawnScroll();
    const visible = visibleCells(metrics, this.#sheet, scroll, this.#size, this.#layout);
    this.#visible = visible;
    const wantedPages = pagesToHold(visible, this.#sheet, this.#effectiveMargin(started));
    const freezeKey = `${this.#frozenRows}:${this.#frozenCols}`;
    const freezeChanged = freezeKey !== this.#lastFreezeKey;
    if (
      freezeChanged ||
      this.#holdsTooMuch ||
      (!this.#fixedMargin &&
        started >= this.#fastScrollUntil &&
        !isEmptySpanSubset(wantedPages, this.#cache.span))
    ) {
      this.#holdsTooMuch = false;
      this.#hold(wantedPages, freezeChanged);
    } else if (this.#fixedMargin || started < this.#fastScrollUntil || this.#cache.size === 0) {
      this.#hold(wantedPages, freezeChanged);
    }

    this.#drawn = {
      size: this.#size,
      scale: this.#scale,
      zoom: this.#zoom,
      metrics,
      theme: this.#theme,
      scroll,
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
      diffHighlights: this.#diffHighlights,
      fillPreview: this.#fillPreview,
      clipboardRange: this.#clipboardRange,
      reorderGuide: this.#reorderGuide,
      showFillHandle: this.#extraRanges.length === 0,
      filterState: this.#filterState,
      filterRowMap: this.#filterRowMap,
      activeSpillRange: this.#activeSpillRange,
    };
    const counts = this.#renderer.draw(this.#drawn);

    this.#retired?.destroy();
    this.#retired = null;

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
    // Pages of newly pinned strips may have come before there was room for them.
    const resend = force || isEmpty(this.#cache.span);
    this.#cache.retain(pages, this.#frozenRows, this.#frozenCols);
    const cells = cellsOfPages(pages, this.#sheet);
    this.#epoch = (this.#epoch + 1) >>> 0;
    this.#options.subscribe({
      epoch: this.#epoch,
      firstRow: cells.firstRow,
      rowCount: cells.endRow - cells.firstRow,
      firstCol: cells.firstCol,
      colCount: cells.endCol - cells.firstCol,
      visible: isEmpty(pages) ? EMPTY_SPAN : this.#visible,
      resend,
    });
  }

  #resize(): void {
    this.#resized = false;
    const scale = window.devicePixelRatio || 1;
    const pixelWidth = Math.round(this.#host.clientWidth * scale);
    const pixelHeight = Math.round(this.#host.clientHeight * scale);
    const size = { width: pixelWidth / scale, height: pixelHeight / scale };
    this.#renderer.resize(pixelWidth, pixelHeight);
    const { style } = this.#renderer.canvas;
    // The size the backing store has on the screen, not the host's, which is
    // rarely a whole number of device pixels: the browser would scale the one
    // to the other and blur every line. The host clips what is too much.
    style.width = `${pixelWidth / scale}px`;
    style.height = `${pixelHeight / scale}px`;
    if (scale !== this.#scale) {
      // At another density, sizes are rounded to other numbers of pixels. A
      // million rows down, that is thousands of rows.
      this.#rescale(this.#zoom, scale, size, null);
      this.#placeScrollbars();
      this.#holdsTooMuch = true;
      return;
    }
    this.#size = size;
    this.#scroll = clampScroll(
      this.#scroll,
      scrollLimits(this.#metrics, this.#sheet, size, this.#layout),
    );
    const opened = this.#openedAt;
    if (opened && size.width > 0 && size.height > 0) {
      // Where there was too much room to scroll to the end of a sheet, there may
      // be less of it once everything around the grid is there.
      this.#placeScroll(scrollToCorner(this.#viewNow(), opened));
      this.#openedAt = opened;
    }
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
    if (event.ctrlKey && !this.#options.zoom) {
      return;
    }
    const started = performance.now();
    event.preventDefault();
    if (event.ctrlKey) {
      this.#onZoomWheel(event);
      return;
    }
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

  #onZoomWheel(event: WheelEvent): void {
    const isPixels = event.deltaMode === WheelEvent.DOM_DELTA_PIXEL;
    const delta = event.deltaY * (isPixels ? 1 : WHEEL_LINE);
    if (this.#drag || this.#pinching || delta === 0) {
      return;
    }
    const zoom = this.#zoomGoal?.zoom ?? this.#zoom;
    this.#zoomSoon(
      isPixels && !this.#controlHeld
        ? this.#gesture.pinch(delta, zoom)
        : this.#gesture.turn(delta, zoom),
      this.#pointOf(event),
    );
  }

  /** Safari has events of its own for pinches on a trackpad. */
  #onPinch = (event: Event): void => {
    event.preventDefault();
    this.#pinching = event.type !== "gestureend";
    if (event.type === "gesturestart") {
      this.#endZoomGesture();
      this.#gesture.begin(this.#zoom);
    } else if (event.type === "gestureend") {
      this.#endZoomGesture();
    } else if (!this.#drag && "scale" in event && "clientX" in event && "clientY" in event) {
      const { scale, clientX, clientY } = event;
      const zoom = this.#gesture.spread(Number(scale), this.#zoomGoal?.zoom ?? this.#zoom);
      if (zoom !== null) {
        const point = this.#pointOf({ clientX: Number(clientX), clientY: Number(clientY) });
        this.#zoomGoal = { zoom, point };
        this.#invalidate();
      }
    }
  };

  #onControlKey = (event: Event): void => {
    if (event instanceof KeyboardEvent) {
      this.#controlHeld =
        event.key === "Control" ? event.type === "keydown" : this.#controlHeld && event.ctrlKey;
    } else if (event.target === window) {
      // The key may come up while another window has the keyboard.
      this.#controlHeld = false;
    }
  };

  #pointOf(event: { readonly clientX: number; readonly clientY: number }): Point {
    const bounds = this.#input.getBoundingClientRect();
    return { x: event.clientX - bounds.left, y: event.clientY - bounds.top };
  }

  #cellAt(x: number, y: number): Cell | null {
    const scroll = this.#drawnScroll();
    return cellAt(this.#metrics, this.#sheet, scroll, this.#size, x, y, this.#layout);
  }

  #headerAt(x: number, y: number): HeaderHit | null {
    const scroll = this.#drawnScroll();
    return headerHitAt(this.#metrics, this.#sheet, scroll, this.#size, x, y, this.#layout);
  }

  #isOverFillHandle(localX: number, localY: number): boolean {
    const range = this.selectionRange;
    return (
      range !== null &&
      this.#extraRanges.length === 0 &&
      isOverFillHandle(this.#viewNow(), range, { x: localX, y: localY })
    );
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
    const scroll = this.#drawnScroll();
    const sheetY = clampedY < fz.height ? clampedY : scroll.top + clampedY;
    const sheetX = clampedX < fz.width ? clampedX : scroll.left + clampedX;
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
    if (this.#drag) {
      if (event.pointerId !== this.#draggingPointerId) {
        return;
      }
      // The pointer's release was never heard of.
      this.cancelDrag();
    }
    const started = performance.now();
    const bounds = this.#input.getBoundingClientRect();
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
    const hit = this.#headerAt(x, y);
    if (hit) {
      this.#host.focus({ preventScroll: true });
      if (hit.kind === "corner") {
        this.selectAll();
      } else if (hit.kind === "colResize") {
        this.#drag = {
          kind: "colResize",
          col: hit.col,
          startClientX: event.clientX,
          startBase: sizeIn(this.#customCols, hit.col, this.#base.colWidth),
        };
        this.#beginPointerCapture(event);
      } else if (hit.kind === "rowResize") {
        this.#drag = {
          kind: "rowResize",
          row: hit.row,
          startClientY: event.clientY,
          startBase: sizeIn(this.#customRows, hit.row, this.#base.rowHeight),
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

    // 3. Cell body click / drag (after AutoFilter header chevron hit-test).
    const cell = this.#cellAt(x, y);
    if (cell) {
      if (this.#filterState && this.#options.onFilterButtonClick) {
        const headerDispRow =
          this.#filterRowMap?.physicalToDisplay(this.#filterState.headerRow) ??
          this.#filterState.headerRow;
        if (
          cell.row === headerDispRow &&
          cell.col >= this.#filterState.firstCol &&
          cell.col <= this.#filterState.lastCol
        ) {
          const cellBox = this.boxOf(cell.row, cell.col);
          const { left, top, width, height } = cellBox;
          const btn = filterButtonBox(left, top, width, height, this.#zoom);
          if (
            btn &&
            x >= btn.x - 2 &&
            x <= btn.x + btn.size + 2 &&
            y >= btn.y - 2 &&
            y <= btn.y + btn.size + 2
          ) {
            event.preventDefault();
            this.#options.onFilterButtonClick(
              cell.col,
              new DOMRect(bounds.left + btn.x, bounds.top + btn.y, btn.size, btn.size),
            );
            this.#pendingWork += performance.now() - started;
            return;
          }
        }
      }
      // Formula point-and-click reference insertion check (`14-m1b.md` 6.1):
      const initialSpan: Span = {
        firstRow: cell.row,
        endRow: cell.row + 1,
        firstCol: cell.col,
        endCol: cell.col + 1,
      };
      if (this.#options.onPointAndClickRange?.(initialSpan)) {
        event.preventDefault();
        this.#drag = { kind: "pointRef", anchor: cell, lastSpan: initialSpan };
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
      this.#input.setPointerCapture(event.pointerId);
    } catch {
      // Synthetic pointer events in unit tests may lack capture.
    }
  }

  #releasePointer(pointerId: number): void {
    try {
      if (this.#input.hasPointerCapture(pointerId)) {
        this.#input.releasePointerCapture(pointerId);
      }
    } catch {
      // Ignore synthetic pointer release errors.
    }
  }

  /** The pointer is gone without having been lifted: what it did counts for nothing. */
  #onPointerLost = (event: PointerEvent): void => {
    if (this.#draggingPointerId === event.pointerId) {
      this.cancelDrag();
    }
  };

  #onPointerMove = (event: PointerEvent): void => {
    const bounds = this.#input.getBoundingClientRect();
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
      const by = event.clientX - drag.startClientX;
      const px = dragged(drag.startBase, by, this.#zoom, MIN_COL_PX, MAX_COL_PX);
      this.#previewSize("col", drag.col, px);
      return;
    }
    if (drag.kind === "rowResize") {
      const by = event.clientY - drag.startClientY;
      const px = dragged(drag.startBase, by, this.#zoom, MIN_ROW_PX, MAX_ROW_PX);
      this.#previewSize("row", drag.row, px);
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
    this.#releasePointer(event.pointerId);

    if (!finishedDrag) {
      return;
    }

    if (finishedDrag.kind === "colResize") {
      const px = this.#takeSizePreview();
      if (px !== null && px !== finishedDrag.startBase) {
        this.#resizeCol(finishedDrag.col, px);
      }
    } else if (finishedDrag.kind === "rowResize") {
      const px = this.#takeSizePreview();
      if (px !== null && px !== finishedDrag.startBase) {
        this.#resizeRow(finishedDrag.row, px);
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
    const bounds = this.#input.getBoundingClientRect();
    const x = event.clientX - bounds.left;
    const y = event.clientY - bounds.top;

    // 1. Double-click fill handle -> auto-fill down to adjacent table extent!
    const curRange = this.selectionRange;
    if (curRange && this.#isOverFillHandle(x, y)) {
      this.#options.onAutoFillDown?.(curRange);
      return;
    }

    // 2. Double-click column resize border -> auto-fit column width to visible text!
    const hit = this.#headerAt(x, y);
    if (hit && hit.kind === "colResize") {
      const fz = frozenPixels(this.#metrics, this.#sheet, this.#size, this.#layout);
      const fitPx = this.#measurer.columnFitWidth(
        this.#theme,
        this.#cache,
        this.#styles,
        hit.col,
        fz.frozenRows > 0 ? 0 : this.#visible.firstRow,
        Math.max(fz.frozenRows, this.#visible.endRow),
      );
      if (fitPx !== sizeIn(this.#customCols, hit.col, this.#base.colWidth)) {
        this.#resizeCol(hit.col, fitPx);
      }
      return;
    }

    // 3. Double-click cell -> open editor!
    const cell = this.#cellAt(x, y);
    if (cell) {
      this.select(cell);
      this.#options.onActivateEdit?.(cell);
    }
  };

  #onContextMenu = (event: MouseEvent): void => {
    if (!this.#options.onContextMenu) {
      return;
    }
    const bounds = this.#input.getBoundingClientRect();
    const x = event.clientX - bounds.left;
    const y = event.clientY - bounds.top;
    const hit = this.#headerAt(x, y);
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
    const cell = this.#cellAt(x, y);
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
      const hit = this.#headerAt(localX, localY);
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
    if (this.#input.style.cursor !== cursor) {
      this.#input.style.cursor = cursor;
    }
  }

  #previewSize(axis: "row" | "col", index: number, px: number): void {
    this.#sizePreview = { axis, index, px };
    this.#rebuildLayout();
    this.#invalidate();
  }

  /** Ends the preview of a size, and returns the size it showed. */
  #takeSizePreview(): number | null {
    const px = this.#sizePreview?.px ?? null;
    this.#sizePreview = null;
    this.#rebuildLayout();
    this.#invalidate();
    return px;
  }

  /** Resizes a column here, without waiting for the engine to say so, and there. */
  #resizeCol(col: number, px: number): void {
    const id = this.#cache.colIdAt(col);
    if (id === undefined) {
      return;
    }
    const { colWidth } = this.#base;
    this.#customCols = withSize(this.#customCols, col, px, colWidth);
    this.#rebuildLayout();
    this.#invalidate();
    const run = { firstId: String(id), len: 1 };
    this.#options.onResizeCols?.([{ run, sizePx: px === colWidth ? null : px }]);
  }

  #resizeRow(row: number, px: number): void {
    const id = this.#cache.rowIdAt(row);
    if (id === undefined) {
      return;
    }
    const { rowHeight } = this.#base;
    this.#customRows = withSize(this.#customRows, row, px, rowHeight);
    this.#rebuildLayout();
    this.#invalidate();
    const run = { firstId: String(id), len: 1 };
    this.#options.onResizeRows?.([{ run, sizePx: px === rowHeight ? null : px }]);
  }

  #updateDragTarget(): void {
    const drag = this.#drag;
    if (!drag) {
      return;
    }
    const bounds = this.#input.getBoundingClientRect();
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
      if (
        span.firstRow === drag.lastSpan.firstRow &&
        span.endRow === drag.lastSpan.endRow &&
        span.firstCol === drag.lastSpan.firstCol &&
        span.endCol === drag.lastSpan.endCol
      ) {
        return;
      }
      drag.lastSpan = span;
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
    const bounds = this.#input.getBoundingClientRect();
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
      const b = this.#input.getBoundingClientRect();
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
