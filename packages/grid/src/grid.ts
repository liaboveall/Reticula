import { PageCache } from "./cache.ts";
import { type CellRect, type FilterDisplayState, FilterRowMap, filterButtonBox } from "./filter.ts";
import {
  type Box,
  boxOfSpan,
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
import { Mirror } from "./mirror.ts";
import { navigate } from "./navigation.ts";
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
import {
  type CustomSizes,
  commonSize,
  type Interval,
  mergeIntervals,
  sizeIn,
  withSizes,
} from "./sizes.ts";
import { type GridStrings, NO_STRINGS } from "./strings.ts";
import { readTheme, type Theme } from "./theme.ts";
import type { TouchInput } from "./touch-input.ts";
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
/** How long the engine has to send rows or columns that have moved, after which it is taken not to have moved them. */
const MOVE_SETTLE_MS = 2_000;

const NOTHING = (): void => {};

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

/** The runs of some rows or columns, or `null` if not all of their identifiers are known. */
function idRunsOf(
  intervals: readonly Interval[],
  idAt: (position: number) => bigint | undefined,
): IdRun[] | null {
  const ids: bigint[] = [];
  for (const { first, end } of mergeIntervals(intervals)) {
    for (let position = first; position < end; position++) {
      const id = idAt(position);
      if (id === undefined) {
        return null;
      }
      ids.push(id);
    }
  }
  return coalesceIdRuns(ids);
}

/** The cells between two corners. */
function spanOf(one: Cell, other: Cell): Span {
  return {
    firstRow: Math.min(one.row, other.row),
    endRow: Math.max(one.row, other.row) + 1,
    firstCol: Math.min(one.col, other.col),
    endCol: Math.max(one.col, other.col) + 1,
  };
}

const rowsOf = (spans: readonly Span[]): Interval[] =>
  spans.map(({ firstRow, endRow }) => ({ first: firstRow, end: endRow }));
const colsOf = (spans: readonly Span[]): Interval[] =>
  spans.map(({ firstCol, endCol }) => ({ first: firstCol, end: endCol }));

/**
 * The size an edge has been dragged to, as it is kept: at a zoom of 1. An edge
 * that has not moved has not changed it.
 */
function dragged(size: number, by: number, zoom: number, least: number, most: number): number {
  return by === 0 ? size : boundedSize(size + by / zoom, least, most);
}

function boundedSize(size: number, least: number, most: number): number {
  return Math.min(most, Math.max(least, Math.round(size)));
}

interface Cell {
  readonly row: number;
  readonly col: number;
}

const POINTER_EVENTS = [
  "pointerdown",
  "pointermove",
  "pointerup",
  "pointercancel",
  // Follows every release too, by when there is no drag left to abort.
  "lostpointercapture",
] as const;
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
      readonly kind: "resize";
      readonly axis: "row" | "col";
      readonly index: number;
      readonly startClient: number;
      readonly startBase: number;
    }
  | {
      readonly kind: "reorder";
      readonly axis: "row" | "col";
      readonly start: number;
      moved: boolean;
    };

/** The row or column that a part of a header is of. */
function lineOf(hit: Exclude<HeaderHit, { readonly kind: "corner" }>): ["row" | "col", number] {
  return "row" in hit ? ["row", hit.row] : ["col", hit.col];
}

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
  #mirror = new Mirror();
  #strings = NO_STRINGS;
  /** Called when there are other texts. */
  #stringsListeners = new Set<() => void>();
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
  /** Where rows or columns were last moved to, until when they may still be held where they were before. */
  #moved: {
    readonly axis: "row" | "col";
    readonly first: number;
    readonly ids: readonly bigint[];
    readonly until: number;
  } | null = null;
  #cache = new PageCache();
  #epoch = 0;
  #lastFreezeKey = "0:0";
  /** Whether more pages than are wanted may be held. */
  #holdsTooMuch = false;

  #lastVelocityTop = 0;
  #lastVelocityTime = 0;
  #fastScrollUntil = 0;

  /** What fingers do, which is loaded with the first of them. */
  #touch: Promise<TouchInput> | null = null;
  /** Whether the pointer that was heard of last is a finger. */
  #isTouched = false;
  #draggingPointerId: number | null = null;
  #drag: DragInteraction | null = null;
  /** Where what is dragged was last, on the page. */
  #dragged: { readonly clientX: number; readonly clientY: number } = { clientX: 0, clientY: 0 };
  #autoScrollTimer = 0;

  #frameRequest = 0;
  #resized = true;
  #destroyed = false;
  #observer: ResizeObserver;
  #language: MutationObserver;
  #lang = document.documentElement.lang;
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
    // Its keys move the selection, so that they are not for reading the page with.
    host.setAttribute("role", "application");
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
    host.append(this.#renderer.canvas, this.#input, ...bars, this.#mirror.element);
    this.#updateMirror();

    this.#theme = readTheme(host);
    this.#scheme = matchMedia("(prefers-color-scheme: dark)");
    this.#scheme.addEventListener("change", this.#restyle);
    this.#watchDensity();
    this.#language = new MutationObserver(this.#onLanguageChange);
    this.#language.observe(document.documentElement, { attributeFilter: ["lang"] });

    host.addEventListener("keydown", this.#onKeyDown);
    host.addEventListener("wheel", this.#onWheel, { passive: false });
    if (options.zoom) {
      for (const type of PINCH_EVENTS) {
        host.addEventListener(type, this.#onPinch, { passive: false });
      }
      for (const type of KEY_EVENTS) {
        window.addEventListener(type, this.#onControlKey, { capture: true });
      }
    }
    for (const type of POINTER_EVENTS) {
      this.#input.addEventListener(type, this.#onPointer);
    }
    this.#input.addEventListener("dblclick", this.#onDoubleClick);
    this.#input.addEventListener("contextmenu", this.#onContextMenu);
    this.#observer = new ResizeObserver(() => {
      this.#resized = true;
      this.#invalidate();
    });
    this.#observer.observe(host);
    this.#invalidate();
  }

  /** Hands the grid its texts, in the language of the page. */
  setStrings(strings: GridStrings): void {
    this.#strings = strings;
    this.#host.setAttribute("aria-label", strings.grid);
    this.#mirror.name(strings.selectedCell);
    for (const listener of this.#stringsListeners) {
      listener();
    }
  }

  get strings(): GridStrings {
    return this.#strings;
  }

  /**
   * Leaves the keyboard to an editor in the grid's element, which is called when
   * there are other texts. Returns how to take the keyboard back.
   */
  attachEditor(onStrings: () => void): () => void {
    this.#stringsListeners.add(onStrings);
    this.#host.tabIndex = -1;
    this.#host.setAttribute("role", "group");
    return () => {
      this.#stringsListeners.delete(onStrings);
      this.#host.tabIndex = 0;
      this.#host.setAttribute("role", "application");
    };
  }

  /** Takes the keyboard, or has the editor take it if there is one. */
  focus(): void {
    this.#host.focus({ preventScroll: true });
  }

  /**
   * Opens a new sheet, where it was left if that is known. Something is
   * selected then if there is anything to select: without a selection, the
   * keyboard would be of no use.
   */
  setSheet(sheet: SheetConfig, view: Partial<GridView> = {}): void {
    this.cancelDrag();
    this.#stopTouch();
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
    this.#moved = null;
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
    this.#notifySelection();
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
        preview?.axis === axis
          ? withSizes(
              sizes,
              [{ first: preview.index, end: preview.index + 1 }],
              preview.px,
              standard,
            )
          : sizes;
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
    if (this.#isDragging || !Number.isFinite(zoom)) {
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
    this.#stopTouch();
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
    if (this.#zoomGoal && !this.#isDragging) {
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

  /** Selects whole rows. The active cell stays in its column if it is in one of them. */
  selectRows(first: number, last: number): void {
    this.#selectWhole("rows", first, last);
  }

  /** Selects whole columns. The active cell stays in its row if it is in one of them. */
  selectCols(first: number, last: number): void {
    this.#selectWhole("cols", first, last);
  }

  #selectWhole(scope: "rows" | "cols", first: number, last: number): void {
    const active = this.#anchor;
    const at = scope === "rows" ? active?.row : active?.col;
    const isInside = at !== undefined && at >= first && at <= last;
    // The selection reaches from the active cell on, so that is at one of its ends.
    const from = at === last ? last : first;
    const to = from === last ? first : last;
    if (scope === "rows") {
      const col = isInside ? (active?.col ?? 0) : 0;
      this.selectRange({ row: from, col }, { row: to, col: this.#sheet.cols - 1 }, "rows");
    } else {
      const row = isInside ? (active?.row ?? 0) : 0;
      this.selectRange({ row, col: from }, { row: this.#sheet.rows - 1, col: to }, "cols");
    }
    this.#revealAnchor();
  }

  /** Selects some cells as well, the first of which becomes the active one. */
  addSelectionRange(span: Span): void {
    const { rows, cols } = this.#sheet;
    const firstRow = Math.max(0, span.firstRow);
    const firstCol = Math.max(0, span.firstCol);
    const endRow = Math.min(rows, span.endRow);
    const endCol = Math.min(cols, span.endCol);
    if (firstRow >= endRow || firstCol >= endCol) {
      return;
    }
    const others = this.selectionRanges;
    const wholeRows = firstCol === 0 && endCol === cols;
    const wholeCols = firstRow === 0 && endRow === rows;
    this.#extraRanges = [...others];
    this.#scope =
      wholeRows && wholeCols ? "all" : wholeRows ? "rows" : wholeCols ? "cols" : "cells";
    this.#anchor = { row: firstRow, col: firstCol };
    this.#focus = { row: endRow - 1, col: endCol - 1 };
    this.#revealAnchor();
    this.#notifySelection();
    this.#invalidate();
  }

  /** Scrolls to the active cell; along whole rows or columns there is nothing to scroll to. */
  #revealAnchor(): void {
    if (this.#anchor) {
      this.#revealWithin(this.#anchor, this.#scope);
    }
  }

  #revealWithin({ row, col }: Cell, scope: SelectionScope): void {
    this.#settleScroll();
    const { top, left } = scrollToReveal(
      this.#metrics,
      this.#size,
      this.#scroll,
      row,
      col,
      this.#sheet,
      this.#layout,
    );
    this.scrollTo({
      ...(scope === "cols" || scope === "all" ? {} : { top }),
      ...(scope === "rows" || scope === "all" ? {} : { left }),
    });
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

  /** The corner of the primary selection that is across from the active cell. */
  get selectionFocus(): { readonly row: number; readonly col: number } | null {
    return this.#focus;
  }

  /** Primary selection span (`firstRow..endRow`, `firstCol..endCol`). */
  get selectionRange(): Span | null {
    if (!this.#anchor || !this.#focus) {
      return null;
    }
    const span = spanOf(this.#anchor, this.#focus);
    const { rows, cols } = this.#sheet;
    const scope = this.#scope;
    return {
      ...span,
      ...(scope === "rows" || scope === "all" ? { firstCol: 0, endCol: cols } : {}),
      ...(scope === "cols" || scope === "all" ? { firstRow: 0, endRow: rows } : {}),
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
    return ranges.length === 0
      ? null
      : idRunsOf(rowsOf(ranges), (position) => this.#cache.rowIdAt(position));
  }

  /** Resolves contiguous `IdRun`s for all unique columns across all selected ranges. */
  selectedColRuns(): IdRun[] | null {
    const ranges = this.selectionRanges;
    return ranges.length === 0
      ? null
      : idRunsOf(colsOf(ranges), (position) => this.#cache.colIdAt(position));
  }

  /**
   * Moves the selected rows past the row before or after them, and the selection
   * with them. Returns whether it did: there has to be such a row, and the grid
   * has to know the rows, which it does of the ones around what is in view.
   */
  moveSelectedRows(direction: -1 | 1): boolean {
    return this.#moveSelected("row", direction);
  }

  moveSelectedCols(direction: -1 | 1): boolean {
    return this.#moveSelected("col", direction);
  }

  #moveSelected(axis: "row" | "col", direction: -1 | 1): boolean {
    const span = this.selectionRange;
    const { onMoveRows, onMoveCols } = this.#options;
    const onMove = axis === "row" ? onMoveRows : onMoveCols;
    const anchor = this.#anchor;
    const focus = this.#focus;
    if (!span || !anchor || !focus || !onMove || this.#extraRanges.length > 0 || this.#isMoving()) {
      return false;
    }
    const isRow = axis === "row";
    const count = isRow ? this.#sheet.rows : this.#sheet.cols;
    const [first, end] = isRow ? [span.firstRow, span.endRow] : [span.firstCol, span.endCol];
    const before = direction < 0 ? first - 1 : end + 1;
    if (before < 0 || before > count) {
      return false;
    }
    const after = before === 0 ? null : isRow ? this.rowIdAt(before - 1) : this.colIdAt(before - 1);
    const ids = this.#idsOf(axis, first, end);
    const passed = this.#idsOf(
      axis,
      direction < 0 ? first - 1 : end,
      direction < 0 ? first : end + 1,
    );
    if (!ids || !passed || (before > 0 && after === null)) {
      return false;
    }
    this.#moved = {
      axis,
      first: direction < 0 ? first - 1 : first,
      ids: direction < 0 ? [...ids, ...passed] : [...passed, ...ids],
      until: performance.now() + MOVE_SETTLE_MS,
    };
    onMove(coalesceIdRuns(ids), after);
    const moved = ({ row, col }: Cell): Cell =>
      isRow ? { row: row + direction, col } : { row, col: col + direction };
    this.#anchor = moved(anchor);
    this.#focus = moved(focus);
    this.#revealAnchor();
    this.#notifySelection();
    this.#invalidate();
    return true;
  }

  /**
   * Whether the rows or columns that were moved last are still held where they
   * were before. Their pages arrive one by one, some of which are not of them.
   */
  #isMoving(): boolean {
    if (this.#moved) {
      const { axis, first, ids, until } = this.#moved;
      const held = this.#idsOf(axis, first, first + ids.length);
      if (performance.now() >= until || ids.every((id, index) => id === held?.[index])) {
        this.#moved = null;
      }
    }
    return this.#moved !== null;
  }

  /** The identifiers of some rows or columns, or `null` if not all of them are known. */
  #idsOf(axis: "row" | "col", first: number, end: number): bigint[] | null {
    const ids: bigint[] = [];
    for (let position = first; position < end; position++) {
      const id = axis === "row" ? this.#cache.rowIdAt(position) : this.#cache.colIdAt(position);
      if (id === undefined) {
        return null;
      }
      ids.push(id);
    }
    return ids;
  }

  /**
   * Gives the selected rows a height in pixels at a zoom of 1, or the standard
   * one. Returns whether it did; the grid has to know the rows.
   */
  resizeSelectedRows(sizePx: number | null): boolean {
    const runs = this.selectedRowRuns();
    if (!runs || !this.#options.onResizeRows) {
      return false;
    }
    const { rowHeight } = this.#base;
    const px = sizePx === null ? rowHeight : boundedSize(sizePx, MIN_ROW_PX, MAX_ROW_PX);
    const intervals = rowsOf(this.selectionRanges);
    this.#customRows = withSizes(this.#customRows, intervals, px, rowHeight);
    this.#rebuildLayout();
    this.#invalidate();
    const size = px === rowHeight ? null : px;
    this.#options.onResizeRows(runs.map((run) => ({ run, sizePx: size })));
    return true;
  }

  /** The same for the width of the selected columns. */
  resizeSelectedCols(sizePx: number | null): boolean {
    const runs = this.selectedColRuns();
    if (!runs || !this.#options.onResizeCols) {
      return false;
    }
    const { colWidth } = this.#base;
    const px = sizePx === null ? colWidth : boundedSize(sizePx, MIN_COL_PX, MAX_COL_PX);
    const intervals = colsOf(this.selectionRanges);
    this.#customCols = withSizes(this.#customCols, intervals, px, colWidth);
    this.#rebuildLayout();
    this.#invalidate();
    const size = px === colWidth ? null : px;
    this.#options.onResizeCols(runs.map((run) => ({ run, sizePx: size })));
    return true;
  }

  /**
   * Makes the selected columns as wide as their texts in view need. Returns
   * whether there was any of them whose texts the grid has.
   */
  autofitSelectedCols(): boolean {
    const { colWidth } = this.#base;
    const sizes: { run: IdRun; sizePx: number | null }[] = [];
    let found = false;
    for (const { first, end } of mergeIntervals(colsOf(this.selectionRanges))) {
      for (let col = first; col < end; col++) {
        const id = this.#cache.colIdAt(col);
        if (id === undefined || !this.#cache.pageOf(this.#visible.firstRow, col)) {
          continue;
        }
        found = true;
        const px = this.#fitWidth(col);
        if (px !== sizeIn(this.#customCols, col, colWidth)) {
          this.#customCols = withSizes(
            this.#customCols,
            [{ first: col, end: col + 1 }],
            px,
            colWidth,
          );
          const run = { firstId: String(id), len: 1 };
          sizes.push({ run, sizePx: px === colWidth ? null : px });
        }
      }
    }
    if (sizes.length > 0) {
      this.#rebuildLayout();
      this.#invalidate();
      this.#options.onResizeCols?.(sizes);
    }
    return found;
  }

  /** How wide a column has to be for its name and those of its texts that are in view. */
  #fitWidth(col: number): number {
    const { frozenRows } = frozenPixels(this.#metrics, this.#sheet, this.#size, this.#layout);
    return this.#measurer.columnFitWidth(
      this.#theme,
      this.#cache,
      this.#styles,
      col,
      frozenRows > 0 ? 0 : this.#visible.firstRow,
      Math.max(frozenRows, this.#visible.endRow),
    );
  }

  /** The sizes that all selected rows and columns have, in pixels at a zoom of 1. */
  sizeOfSelection(): { readonly rowPx: number | null; readonly colPx: number | null } {
    const ranges = this.selectionRanges;
    const { rowHeight, colWidth } = this.#base;
    return {
      rowPx: commonSize(this.#customRows, rowsOf(ranges), rowHeight),
      colPx: commonSize(this.#customCols, colsOf(ranges), colWidth),
    };
  }

  /**
   * What is in view of the primary selection, in CSS pixels from the corner of
   * the grid's element, or `null` if nothing of it is.
   */
  boxOfSelection(): Box | null {
    const span = this.selectionRange;
    return span && boxOfSpan(this.#viewNow(), span);
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
    this.#revealWithin(this.#focus, this.#scope);
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
    this.#startUpgrade();
  }

  #startUpgrade(): void {
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
      this.#rendererPreference,
    );
  }

  /**
   * Which of a character's forms is drawn depends on the language, and a canvas
   * keeps to the one it has begun with: everything is drawn anew, on new ones.
   */
  #onLanguageChange = (): void => {
    const { lang } = document.documentElement;
    if (lang === this.#lang) {
      return;
    }
    this.#lang = lang;
    this.#measurer.reset();
    this.#endUpgrade();
    this.#swapRenderer(null);
    this.#startUpgrade();
  };

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
    this.#touch?.then((touch) => touch.destroy(), NOTHING);
    window.clearTimeout(this.#zoomTimer);
    this.#hold(EMPTY_SPAN);
    this.#destroyed = true;
    this.#frameListeners.clear();
    cancelAnimationFrame(this.#frameRequest);
    this.#observer.disconnect();
    this.#language.disconnect();
    this.#scheme.removeEventListener("change", this.#restyle);
    this.#density?.removeEventListener("change", this.#onDensityChange);
    this.#host.removeEventListener("keydown", this.#onKeyDown);
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
    this.#mirror.element.remove();
    this.#host.classList.remove("reticula-grid");
    for (const attribute of ["tabindex", "role", "aria-label"]) {
      this.#host.removeAttribute(attribute);
    }
  }

  #notifySelection(): void {
    this.#updateMirror();
    this.#options.onSelectionChange?.(this.#selection());
  }

  /** Not with every frame: what it shows changes with the selection, and as pages arrive. */
  #updateMirror(): void {
    const cell = this.#anchor;
    const rows = this.#filterRowMap?.totalRows ?? this.#sheet.rows;
    this.#mirror.show(
      { rows, cols: this.#sheet.cols },
      cell && {
        row: this.displayRowToPhysicalRow(cell.row),
        col: cell.col,
        text: this.textAt(cell.row, cell.col) ?? "",
      },
    );
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
    const rowName = (row: number) => this.displayRowToPhysicalRow(row) + 1;
    const startName = `${columnName(range.firstCol)}${rowName(range.firstRow)}`;
    const single =
      ranges.length === 1 &&
      range.endRow - range.firstRow === 1 &&
      range.endCol - range.firstCol === 1;
    const baseName = single
      ? `${columnName(anchor.col)}${rowName(anchor.row)}`
      : `${startName}:${columnName(range.endCol - 1)}${rowName(range.endRow - 1)}`;
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
      if (!this.#isDragging) {
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

  /** Zooming would pull away what is dragged from under the pointer. */
  get #isDragging(): boolean {
    return this.#drag !== null || this.#vertical.isDragging || this.#horizontal.isDragging;
  }

  #onKeyDown = (event: KeyboardEvent): void => {
    // Whatever is in the grid's element, such as an editor, has keys of its own.
    if (event.target !== this.#host || event.defaultPrevented || event.isComposing) {
      return;
    }
    if (event.key === "Escape" ? this.cancelDrag() : navigate(this, event)) {
      event.preventDefault();
    }
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
    this.#scrollBy(left, top);
    this.#pendingWork += performance.now() - started;
  };

  #scrollBy = (dx: number, dy: number): void => {
    this.scrollTo({ top: this.#scroll.top + dy, left: this.#scroll.left + dx });
  };

  #onZoomWheel(event: WheelEvent): void {
    const isPixels = event.deltaMode === WheelEvent.DOM_DELTA_PIXEL;
    const delta = event.deltaY * (isPixels ? 1 : WHEEL_LINE);
    if (this.#isDragging || this.#pinching || delta === 0) {
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

  /** Safari has events of its own for pinches, on a touch screen too. */
  #onPinch = (event: Event): void => {
    event.preventDefault();
    if (this.#isTouched) {
      return;
    }
    this.#pinching = event.type !== "gestureend";
    if (event.type === "gesturestart") {
      this.#beginPinch();
    } else if (event.type === "gestureend") {
      this.#endZoomGesture();
    } else if (!this.#isDragging && "scale" in event && "clientX" in event && "clientY" in event) {
      const { scale, clientX, clientY } = event;
      const point = this.#pointOf({ clientX: Number(clientX), clientY: Number(clientY) });
      this.#pinch(Number(scale), point);
    }
  };

  #beginPinch = (): void => {
    this.#endZoomGesture();
    this.#gesture.begin(this.#zoom);
  };

  #pinch = (scale: number, point: Point): void => {
    const goal = this.#zoomGoal?.zoom;
    const zoom = this.#gesture.spread(scale, goal ?? this.#zoom) ?? goal;
    if (zoom !== undefined) {
      this.#zoomGoal = { zoom, point };
      this.#invalidate();
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

  /** Loads what fingers do, if there may be any. */
  prepareTouch(): void {
    if (matchMedia("(any-pointer: coarse)").matches) {
      this.#withTouch(NOTHING);
    }
  }

  #withTouch(act: (touch: TouchInput) => void): void {
    this.#touch ??= import("./touch-input.ts").then(
      ({ TouchInput }) =>
        new TouchInput({
          grid: this,
          host: this.#host,
          input: this.#input,
          zooms: this.#options.zoom === true,
          metrics: () => this.#metrics,
          scroll: () => this.#scroll,
          scrollBy: this.#scrollBy,
          cellAt: (x, y) => this.#cellAt(x, y),
          beginPinch: this.#beginPinch,
          pinch: this.#pinch,
          endPinch: this.#endZoomGesture,
          pressFilterButton: (point) => this.#pressFilterButton(point),
        }),
    );
    this.#touch.then(act, () => {
      this.#touch = null;
    });
  }

  #stopTouch(): void {
    this.#touch?.then((touch) => touch.stop(), NOTHING);
  }

  #onPointer = (event: PointerEvent): void => {
    const isDown = event.type === "pointerdown";
    this.#isTouched = event.pointerType === "touch";
    if (this.#isTouched && event.pointerId !== this.#draggingPointerId) {
      if (isDown) {
        // Or mouse events would follow, which would take the focus to the editor.
        event.preventDefault();
      }
      // A finger that comes to what is being dragged is in the way.
      if (!isDown || !this.#drag) {
        this.#withTouch((touch) => touch.handle(event));
      }
    } else if (isDown) {
      this.#onPointerDown(event);
    } else if (event.type === "pointermove") {
      this.#onPointerMove(event);
    } else if (event.type === "pointerup") {
      this.#onPointerUp(event);
    } else {
      this.#onPointerLost(event);
    }
  };

  #onPointerDown(event: PointerEvent): void {
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
    this.#press(event);
    this.#pendingWork += performance.now() - started;
  }

  #press(event: PointerEvent): void {
    const { x, y } = this.#pointOf(event);
    const multiMod = event.ctrlKey || event.metaKey;

    // 1. Check fill handle first!
    const curRange = this.selectionRange;
    if (curRange && this.#isOverFillHandle(x, y)) {
      this.focus();
      this.#drag = { kind: "fill", src: curRange };
      this.#fillPreview = curRange;
      this.#beginPointerCapture(event);
      this.#invalidate();
      return;
    }

    // 2. Check header strips (corner, resize handles, whole-row/col select or drag reorder).
    const hit = this.#headerAt(x, y);
    if (hit) {
      this.focus();
      if (hit.kind === "corner") {
        this.selectAll();
      } else {
        const [axis, index] = lineOf(hit);
        if (hit.kind === "colResize" || hit.kind === "rowResize") {
          const { rowHeight, colWidth } = this.#base;
          this.#drag = {
            kind: "resize",
            axis,
            index,
            startClient: axis === "row" ? event.clientY : event.clientX,
            startBase:
              axis === "row"
                ? sizeIn(this.#customRows, index, rowHeight)
                : sizeIn(this.#customCols, index, colWidth),
          };
        } else if (!event.shiftKey && !multiMod && this.#isLineSelected(axis, index)) {
          // If clicking an already-selected whole line without Shift/Ctrl, allow dragging to reorder!
          this.#drag = { kind: "reorder", axis, start: index, moved: false };
        } else {
          if (multiMod && curRange) {
            this.#extraRanges.push(curRange);
          } else if (!event.shiftKey) {
            this.#extraRanges = [];
          }
          this.#pickLine(axis, index, event.shiftKey);
          this.#drag = { kind: "select", mode: this.#scope };
        }
        this.#beginPointerCapture(event);
      }
      return;
    }

    // 3. Cell body click / drag (after AutoFilter header chevron hit-test).
    const cell = this.#cellAt(x, y);
    if (cell) {
      if (this.#pressFilterButton({ x, y })) {
        event.preventDefault();
        return;
      }
      // Formula point-and-click reference insertion check (`14-m1b.md` 6.1):
      const initialSpan = spanOf(cell, cell);
      if (this.#options.onPointAndClickRange?.(initialSpan)) {
        event.preventDefault();
        this.#drag = { kind: "pointRef", anchor: cell, lastSpan: initialSpan };
        this.#beginPointerCapture(event);
        return;
      }

      this.focus();
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
  }

  /** Opens the filter whose button is at a point. Returns whether there is one. */
  #pressFilterButton({ x, y }: Point): boolean {
    const filter = this.#filterState;
    const cell = this.#cellAt(x, y);
    const { onFilterButtonClick } = this.#options;
    if (
      !filter ||
      !cell ||
      !onFilterButtonClick ||
      cell.row !== (this.#filterRowMap?.physicalToDisplay(filter.headerRow) ?? filter.headerRow) ||
      cell.col < filter.firstCol ||
      cell.col > filter.lastCol
    ) {
      return false;
    }
    const { left, top, width, height } = this.boxOf(cell.row, cell.col);
    const button = filterButtonBox(left, top, width, height, this.#zoom);
    if (
      !button ||
      x < button.x - 2 ||
      x > button.x + button.size + 2 ||
      y < button.y - 2 ||
      y > button.y + button.size + 2
    ) {
      return false;
    }
    const bounds = this.#input.getBoundingClientRect();
    onFilterButtonClick(
      cell.col,
      new DOMRect(bounds.left + button.x, bounds.top + button.y, button.size, button.size),
    );
    return true;
  }

  #beginPointerCapture(event: PointerEvent): void {
    this.#draggingPointerId = event.pointerId;
    this.#dragged = event;
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
  #onPointerLost(event: PointerEvent): void {
    if (this.#draggingPointerId === event.pointerId) {
      this.cancelDrag();
    }
  }

  #onPointerMove(event: PointerEvent): void {
    const { x: localX, y: localY } = this.#pointOf(event);

    if (this.#draggingPointerId !== event.pointerId || !this.#drag) {
      this.#updateHoverCursor(localX, localY);
      return;
    }

    this.#dragged = event;
    const drag = this.#drag;

    if (drag.kind === "resize") {
      const isRow = drag.axis === "row";
      const by = (isRow ? event.clientY : event.clientX) - drag.startClient;
      const px = isRow
        ? dragged(drag.startBase, by, this.#zoom, MIN_ROW_PX, MAX_ROW_PX)
        : dragged(drag.startBase, by, this.#zoom, MIN_COL_PX, MAX_COL_PX);
      this.#previewSize(drag.axis, drag.index, px);
      return;
    }
    if (drag.kind === "reorder") {
      drag.moved = true;
      const { headerWidth, headerHeight } = this.#metrics;
      const isRow = drag.axis === "row";
      const target = this.#cellAtClamped(
        isRow ? headerWidth + 4 : localX,
        isRow ? localY : headerHeight + 4,
      );
      if (target) {
        const { top, left, width, height } = this.boxOf(target.row, target.col);
        this.#reorderGuide = isRow
          ? { axis: "row", beforeRow: target.row + (localY > top + height / 2 ? 1 : 0) }
          : { axis: "col", beforeCol: target.col + (localX > left + width / 2 ? 1 : 0) };
        this.#invalidate();
      }
      this.#syncAutoScroll();
      return;
    }

    this.#updateDragTarget();
    this.#syncAutoScroll();
  }

  #onPointerUp(event: PointerEvent): void {
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

    if (finishedDrag.kind === "resize") {
      const px = this.#takeSizePreview();
      if (px !== null && px !== finishedDrag.startBase) {
        this.#resizeLine(finishedDrag.axis, finishedDrag.index, px);
      }
    } else if (finishedDrag.kind === "fill") {
      const preview = this.#fillPreview;
      this.#fillPreview = null;
      this.#invalidate();
      if (preview && !sameSpan(preview, finishedDrag.src)) {
        this.#options.onFillRange?.(finishedDrag.src, preview);
      }
    } else if (finishedDrag.kind === "reorder") {
      const guide = this.#reorderGuide;
      const range = this.selectionRange;
      this.#reorderGuide = null;
      this.#invalidate();
      if (!finishedDrag.moved) {
        this.#extraRanges = [];
        this.#pickLine(finishedDrag.axis, finishedDrag.start);
      } else if (guide && range) {
        const isRow = guide.axis === "row";
        const before = isRow ? guide.beforeRow : guide.beforeCol;
        const [first, end] = isRow
          ? [range.firstRow, range.endRow]
          : [range.firstCol, range.endCol];
        const runs = isRow ? this.selectedRowRuns() : this.selectedColRuns();
        const after =
          before === 0 ? null : isRow ? this.rowIdAt(before - 1) : this.colIdAt(before - 1);
        if (runs && (before < first || before > end) && (before === 0 || after !== null)) {
          const { onMoveRows, onMoveCols } = this.#options;
          (isRow ? onMoveRows : onMoveCols)?.(runs, after);
        }
      }
    }
  }

  /** Whether a whole row or column is selected, by itself or with others around it. */
  #isLineSelected(axis: "row" | "col", index: number): boolean {
    const range = this.selectionRange;
    return (
      range !== null &&
      (axis === "row"
        ? this.#scope === "rows" && index >= range.firstRow && index < range.endRow
        : this.#scope === "cols" && index >= range.firstCol && index < range.endCol)
    );
  }

  /** Selects a whole row or column, or extends the selection to it. */
  #pickLine(axis: "row" | "col", index: number, extend = false): void {
    const { rows, cols } = this.#sheet;
    const isRow = axis === "row";
    this.#scope = isRow ? "rows" : "cols";
    if (!extend || !this.#anchor) {
      this.#anchor = isRow ? { row: index, col: 0 } : { row: 0, col: index };
    }
    this.#focus = isRow ? { row: index, col: cols - 1 } : { row: rows - 1, col: index };
    this.#notifySelection();
    this.#invalidate();
  }

  #onDoubleClick = (event: MouseEvent): void => {
    if (this.#isTouched) {
      return;
    }
    const { x, y } = this.#pointOf(event);

    // 1. Double-click fill handle -> auto-fill down to adjacent table extent!
    const curRange = this.selectionRange;
    if (curRange && this.#isOverFillHandle(x, y)) {
      this.#options.onAutoFillDown?.(curRange);
      return;
    }

    // 2. Double-click column resize border -> auto-fit column width to visible text!
    const hit = this.#headerAt(x, y);
    if (hit && hit.kind === "colResize") {
      const fitPx = this.#fitWidth(hit.col);
      if (fitPx !== sizeIn(this.#customCols, hit.col, this.#base.colWidth)) {
        this.#resizeLine("col", hit.col, fitPx);
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
    if (this.#isTouched) {
      // A long press, which has no menu of the browser's either.
      event.preventDefault();
      return;
    }
    if (!this.#options.onContextMenu) {
      return;
    }
    const { x, y } = this.#pointOf(event);
    const hit = this.#headerAt(x, y);
    const { clientX, clientY } = event;
    if (hit?.kind === "row" || hit?.kind === "col") {
      event.preventDefault();
      const [axis, index] = lineOf(hit);
      const isRow = axis === "row";
      if (!this.#isLineSelected(axis, index)) {
        this.#extraRanges = [];
        this.#pickLine(axis, index);
      }
      this.#options.onContextMenu({
        clientX,
        clientY,
        zone: isRow ? "rowHeader" : "colHeader",
        row: isRow ? index : 0,
        col: isRow ? 0 : index,
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
      this.#options.onContextMenu({ clientX, clientY, zone: "cell", row: cell.row, col: cell.col });
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
        } else if (hit.kind !== "corner" && this.#isLineSelected(...lineOf(hit))) {
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

  /** Resizes a row or a column here, without waiting for the engine to say so, and there. */
  #resizeLine(axis: "row" | "col", index: number, px: number): void {
    const isRow = axis === "row";
    const id = isRow ? this.#cache.rowIdAt(index) : this.#cache.colIdAt(index);
    if (id === undefined) {
      return;
    }
    const { rowHeight, colWidth } = this.#base;
    const standard = isRow ? rowHeight : colWidth;
    const line = [{ first: index, end: index + 1 }];
    if (isRow) {
      this.#customRows = withSizes(this.#customRows, line, px, standard);
    } else {
      this.#customCols = withSizes(this.#customCols, line, px, standard);
    }
    this.#rebuildLayout();
    this.#invalidate();
    const { onResizeRows, onResizeCols } = this.#options;
    const run = { firstId: String(id), len: 1 };
    (isRow ? onResizeRows : onResizeCols)?.([{ run, sizePx: px === standard ? null : px }]);
  }

  #updateDragTarget(): void {
    const drag = this.#drag;
    if (!drag) {
      return;
    }
    const { x, y } = this.#pointOf(this.#dragged);
    const next = this.#cellAtClamped(
      drag.kind === "select" && drag.mode === "rows" ? this.#metrics.headerWidth + 4 : x,
      drag.kind === "select" && drag.mode === "cols" ? this.#metrics.headerHeight + 4 : y,
    );
    if (!next) {
      return;
    }

    if (drag.kind === "pointRef") {
      const span = spanOf(drag.anchor, next);
      if (!sameSpan(span, drag.lastSpan)) {
        drag.lastSpan = span;
        this.#options.onPointAndClickRange?.(span);
      }
      return;
    }

    if (drag.kind === "fill") {
      const { src } = drag;
      const { firstRow, endRow, firstCol, endCol } = src;
      const { row, col } = next;
      const rows = Math.max(firstRow - row, row - endRow + 1, 0);
      const cols = Math.max(firstCol - col, col - endCol + 1, 0);
      const preview =
        rows >= cols
          ? { ...src, firstRow: Math.min(firstRow, row), endRow: Math.max(endRow, row + 1) }
          : { ...src, firstCol: Math.min(firstCol, col), endCol: Math.max(endCol, col + 1) };
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

  /** How far to scroll as long as what is dragged is past an edge of the cells. */
  #overshoot(): Scroll {
    const { x, y } = this.#pointOf(this.#dragged);
    const { width, height } = cellArea(this.#metrics, this.#size);
    const { headerWidth, headerHeight, rowHeight, colWidth } = this.#metrics;
    const mode = this.#drag?.kind === "select" ? this.#drag.mode : null;
    const past = (at: number, size: number) => (at < 0 ? -1 : at >= size ? 1 : 0);
    return {
      top: mode === "cols" ? 0 : past(y - headerHeight, height) * rowHeight * 2,
      left: mode === "rows" ? 0 : past(x - headerWidth, width) * colWidth,
    };
  }

  #syncAutoScroll(): void {
    const { top, left } = this.#overshoot();
    if (top === 0 && left === 0) {
      this.#stopAutoScroll();
    } else if (this.#autoScrollTimer === 0) {
      this.#autoScrollTimer = window.setInterval(() => {
        const { top, left } = this.#overshoot();
        this.#scrollBy(left, top);
        this.#updateDragTarget();
      }, 40);
    }
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
