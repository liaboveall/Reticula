import type { RemotePresence, WireCellStyle } from "@reticula/protocol";
import type { PageCache } from "../cache.ts";
import type { CellRect, FilterButtonRect, FilterDisplayState, FilterRowMap } from "../filter.ts";
import type { Metrics, Scroll, SheetLayout, SheetSize, Size, Span } from "../layout.ts";
import type { Theme } from "../theme.ts";
import type { PackedColor } from "./color.ts";
import type { Look } from "./look.ts";
import type { PixelBox } from "./rects.ts";
import type { SpriteSpec } from "./sprites.ts";
import type { TextMeasure } from "./text-measure.ts";

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

/** The header of a column that is to be drawn, in device pixels. */
export interface HeaderCell {
  readonly col: number;
  readonly left: number;
  /** The line after it is not a part of it. */
  readonly right: number;
  readonly bottom: number;
  /** Device pixels to a pixel of a header at a zoom of 1. */
  readonly scale: number;
  readonly look: Look;
  /** What texts are drawn in. */
  readonly font: string;
  readonly measure: TextMeasure;
  readonly fill: (box: PixelBox, color: PackedColor) => void;
  /** Draws a text over everything else, from a place to another one at most. */
  readonly text: (text: string, left: number, right: number) => void;
  readonly sprite: (spec: SpriteSpec, left: number, top: number) => void;
}

/**
 * The headers of columns that have names, which a sheet's do not: whoever needs
 * them loads `@reticula/grid/columns` for them.
 */
export interface ColumnHeaders {
  nameOf(col: number): string;
  /** The way the rows are sorted by a column. */
  sortOf(col: number): "ascending" | "descending" | null;
  /** The room a header takes besides its name, in pixels at a zoom of 1. */
  roomOf(col: number): number;
  /** The button in a header, if there is room for one, of so many units to a pixel at a zoom of 1. */
  buttonIn(left: number, width: number, height: number, scale: number): FilterButtonRect | null;
  draw(cell: HeaderCell): void;
}

/** Everything one frame is drawn from. */
export interface Frame {
  /** Size of the grid's area in CSS pixels. */
  readonly size: Size;
  /** Device pixels per CSS pixel. */
  readonly scale: number;
  /** How much the cells are enlarged, which the metrics and the layout already are. */
  readonly zoom?: number;
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
  /** What the columns are called, if not by letters. */
  readonly columns?: ColumnHeaders | null;
  /** Whether no text runs on over the empty cells next to its own. */
  readonly clipText?: boolean;
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
  /** Text runs that were drawn through a clip of their own. */
  clipped: number;
  /** Cells in view whose page has not arrived; they are shown as placeholders. */
  missing: number;
}

/** Space between a cell's edge and its text, in CSS pixels. */
export const PADDING = 6;
/** Fill handle square side in CSS pixels. */
export const FILL_HANDLE_SIZE = 6;
