import { FilterRowMap } from "@reticula/grid";
import { type NamedColumn, NamedColumns } from "@reticula/grid/columns";
import {
  type Frame,
  type Metrics,
  type Page,
  PageCache,
  type Scroll,
  type Span,
  SparseAxis,
  type Theme,
  viewMetrics,
  visibleCells,
} from "@reticula/grid/internals";
import { PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import type { CaseName } from "./names.ts";
import { type Style, SyntheticSheet } from "./sheet.ts";

const METRICS: Metrics = {
  rowHeight: 24,
  colWidth: 80,
  headerHeight: 24,
  headerWidth: 56,
  scrollbarSize: 12,
};
const SIZE = { width: 480, height: 300 };
/** How much more of a sheet a zoomed-out picture shows. */
const MOST_ENLARGED = 3;

/** A frame, and what it takes to draw it once more with a change to its content. */
export interface BuiltCase {
  readonly frame: Frame;
  /** The pages the frame's cache holds. */
  readonly pages: readonly Page[];
}

type Overlays = Partial<
  Pick<
    Frame,
    | "selection"
    | "selectionRange"
    | "selectionRanges"
    | "formulaReferences"
    | "remotePresences"
    | "diffHighlights"
    | "fillPreview"
    | "clipboardRange"
    | "reorderGuide"
    | "filterState"
    | "filterRowMap"
    | "activeSpillRange"
    | "columns"
    | "clipText"
    | "showFillHandle"
  >
>;

interface Setting {
  readonly sheet: SyntheticSheet;
  readonly zoom?: number;
  /** In the pixels on the screen, whatever the zoom. */
  readonly scroll?: Scroll;
  readonly frozenRows?: number;
  readonly frozenCols?: number;
  readonly rowHeights?: readonly (readonly [number, number])[];
  readonly colWidths?: readonly (readonly [number, number])[];
  /** Width of the row headers at a zoom of 1, if not the usual one. */
  readonly headerWidth?: number;
  /** Pages that have not arrived. */
  readonly missing?: (pageRow: number, pageCol: number) => boolean;
  readonly overlays?: Overlays;
}

// --- text --------------------------------------------------------------------

/** Characters the generated font has: it starts at the first ideograph. */
function han(seed: number, length: number): string {
  let text = "";
  for (let index = 0; index < length; index++) {
    text += String.fromCodePoint(0x4e00 + ((seed * 37 + index * 53) % 200));
  }
  return text;
}

const WORDS = [
  "Revenue",
  "quarter",
  "jellyfish",
  "Overhead",
  "of which",
  "Budget",
  "fjord",
  "WAVE",
  "illicit",
  "Total:",
];

function word(seed: number): string {
  return WORDS[seed % WORDS.length] ?? "";
}

function amount(seed: number): string {
  const whole = ((seed * 7919) % 100000) - 20000;
  return `${whole}.${String((seed * 37) % 100).padStart(2, "0")}`;
}

/** Some of everything, so that neighbouring cells differ. */
function mixed(row: number, col: number): string {
  const seed = row * 7 + col * 3;
  switch (seed % 6) {
    case 0:
      return word(seed);
    case 1:
    case 2:
      return amount(seed + 1);
    case 3:
      return han(seed, 1 + (seed % 5));
    case 4:
      return seed % 4 === 0 ? "TRUE" : "#DIV/0!";
    default:
      return `${word(seed)} ${han(seed, 2)}`;
  }
}

/** Half of the cells empty, the rest short. */
function sparse(row: number, col: number): string {
  return (row + col) % 2 === 0 ? mixed(row, col) : "";
}

function filled(): SyntheticSheet {
  return new SyntheticSheet().fill(mixed);
}

function letters(length: number, seed: number): string {
  let text = "";
  for (let index = 0; index < length; index++) {
    text += String.fromCodePoint(0x61 + ((seed + index * 5) % 26));
  }
  return text;
}

const HEBREW = "אבגדה וזחט יכלמ";
/** An acute accent, a tilde and a circumflex, over letters that have no form with them. */
const COMBINED = ["x\u{301}", "q\u{303}", "w\u{302}\u{301}"];
const EMOJI = "\u{1f600}";

/** 512 characters, many of them emoji. */
function longRun(): string {
  return Array.from({ length: 512 }, (_, index) =>
    index % 24 === 7 ? EMOJI : letters(1, index),
  ).join("");
}

const palette = (slot: number): NonNullable<Style["textColor"]> => ({ kind: "palette", slot });
const rgb = (value: number): NonNullable<Style["textColor"]> => ({ kind: "rgb", rgb: value });

function span(firstRow: number, endRow: number, firstCol: number, endCol: number): Span {
  return { firstRow, endRow, firstCol, endCol };
}

// --- the cases ---------------------------------------------------------------

function borders(): SyntheticSheet {
  const sheet = new SyntheticSheet();
  const thin = {
    borderTop: "thin",
    borderRight: "thin",
    borderBottom: "thin",
    borderLeft: "thin",
  } as const;
  const medium = {
    borderTop: "medium",
    borderRight: "medium",
    borderBottom: "medium",
    borderLeft: "medium",
  } as const;
  sheet.put(1, 1, "thin", thin).put(1, 3, "medium", medium);
  sheet.put(3, 0, "", { borderTop: "thin" }).put(3, 1, "", { borderRight: "medium" });
  sheet.put(3, 2, "", { borderBottom: "thin" }).put(3, 3, "", { borderLeft: "medium" });
  for (let col = 0; col < 4; col++) {
    sheet.put(5, col, word(col), { ...thin, fillColor: palette(col) });
    sheet.put(6, col, amount(col), { ...medium, borderTop: "none" });
    sheet.put(7, col, "", col % 2 === 0 ? thin : medium);
  }
  return sheet;
}

function styled(): SyntheticSheet {
  const sheet = new SyntheticSheet().fill((row, col) => (row > 9 ? mixed(row, col) : ""));
  for (let slot = 0; slot < 8; slot++) {
    sheet.put(slot, 0, word(slot), { textColor: palette(slot) });
    sheet.put(slot, 1, amount(slot), { fillColor: palette(slot) });
    sheet.put(slot, 2, han(slot, 3), { fillColor: palette(slot), textColor: palette(7 - slot) });
    sheet.put(slot, 3, word(slot + 3), { strikethrough: true, bold: slot % 2 === 0 });
    sheet.put(slot, 4, word(slot + 5), { fillColor: rgb(0x204060 + slot * 0x101808) });
  }
  sheet.put(8, 0, "rgb", { textColor: rgb(0xc04000), fillColor: rgb(0xfff0c0) });
  sheet.put(8, 2, amount(3), { strikethrough: true, align: "left" });
  sheet.put(8, 3, han(5, 4), { strikethrough: true, align: "center", italic: true });
  return sheet;
}

/** A table to read: the texts stay in their cells, and the columns have names. */
function table(columns: readonly NamedColumn[]): Overlays {
  return { columns: new NamedColumns(columns), clipText: true, showFillHandle: false };
}

const FILTER_HIDDEN = [
  [3, 5],
  [9, 9],
] as const;

const SETTINGS: Record<CaseName, () => Setting> = {
  "plain-text": () => ({ sheet: filled() }),
  "small-text": () => ({ sheet: filled(), scroll: { top: 130, left: 33 } }),
  numbers: () => ({
    sheet: new SyntheticSheet().fill((row, col) =>
      col === 2 ? `${((row * 13) % 400) / 4 - 10}%` : amount(row * 5 + col),
    ),
    colWidths: [
      [0, 61],
      [1, 103],
    ],
  }),
  alignment: () => ({
    sheet: (() => {
      const sheet = new SyntheticSheet();
      const aligns = ["left", "center", "right"] as const;
      for (let row = 0; row < 12; row++) {
        aligns.forEach((align, col) => {
          sheet.put(row, col, mixed(row, col), { align });
        });
        sheet.put(row, 3, row % 2 === 0 ? "TRUE" : "#N/A");
        // Wider than its cell: what does not fit is cut off at both ends.
        sheet.put(row, 4, `${word(row)} ${word(row + 1)}`, { align: aligns[row % 3] ?? "left" });
      }
      return sheet;
    })(),
  }),
  han: () => ({
    sheet: new SyntheticSheet().fill((row, col) => han(row * 11 + col, 2 + ((row + col) % 4))),
  }),
  hebrew: () => ({
    sheet: new SyntheticSheet().fill((row, col) => {
      const texts = [
        HEBREW.slice(row % 3, 5 + (row % 4)),
        `${HEBREW.slice(0, 3)} 12`,
        `a ${HEBREW}`,
      ];
      return texts[(row + col) % 4] ?? "";
    }),
  }),
  "combining-marks": () => ({
    sheet: new SyntheticSheet().fill((row, col) =>
      (row + col) % 3 === 2 ? "" : COMBINED.map((mark) => word(row).charAt(col) + mark).join(""),
    ),
  }),
  "long-run": () => ({
    sheet: new SyntheticSheet()
      .put(1, 1, longRun())
      .put(3, 0, han(3, 512))
      .put(5, 13, longRun())
      .put(7, 15, longRun(), { bold: true, textColor: palette(4) })
      .put(7, 17, "end"),
    scroll: { top: 0, left: 13 * 80 + 17 },
  }),
  overhang: () => ({
    sheet: (() => {
      const sheet = new SyntheticSheet();
      const styles: Partial<Style>[] = [
        { italic: true },
        { bold: true },
        { bold: true, italic: true },
        { italic: true, align: "right" },
        { italic: true, align: "center", fillColor: palette(3) },
      ];
      for (let row = 0; row < 12; row++) {
        styles.forEach((style, col) => {
          sheet.put(row, col, row % 3 === 0 ? "jf" : `j${letters(4 + row, row)}f`, style);
        });
      }
      return sheet;
    })(),
    colWidths: [
      [0, 47],
      [3, 62],
    ],
  }),
  overflow: () => ({
    sheet: (() => {
      const sheet = new SyntheticSheet();
      const long = `${word(0)} ${word(2)} ${word(3)} ${han(2, 6)} ${word(5)}`;
      sheet.put(0, 0, long);
      sheet.put(1, 0, long).put(1, 1, "stop");
      sheet.put(2, 0, long).put(2, 2, amount(2));
      sheet.put(3, 1, "1234567890123456789.25");
      sheet.put(4, 1, long, { align: "right" }).put(5, 1, long, { align: "center" });
      sheet.put(6, 0, long, { fillColor: palette(1) }).put(6, 2, "", { fillColor: palette(2) });
      sheet.put(7, 1, long, { bold: true, strikethrough: true, textColor: palette(5) });
      sheet.put(8, 3, long).put(9, 0, HEBREW + HEBREW);
      sheet.put(10, 0, "#VALUE! not supported here");
      return sheet;
    })(),
  }),
  "overflow-across-pages": () => ({
    sheet: (() => {
      const sheet = new SyntheticSheet();
      const long = `${word(1)} ${word(2)} ${han(4, 8)} ${word(6)} ${word(7)}`;
      for (let row = 0; row < 12; row++) {
        sheet.put(row, 12 + (row % 5), long, row % 4 === 3 ? { italic: true, bold: true } : {});
        if (row % 3 === 1) {
          sheet.put(row, 16, amount(row));
        } else if (row % 3 === 2) {
          sheet.put(row, 18, "stop");
        }
      }
      return sheet;
    })(),
    scroll: { top: 11, left: 14 * 80 - 13 },
  }),
  "overflow-scrolled-off": () => ({
    sheet: (() => {
      const sheet = new SyntheticSheet();
      const long = `${letters(20, 3)} ${han(4, 12)} ${word(6)} ${word(7)} ${word(8)}`;
      for (let row = 0; row < 12; row++) {
        sheet.put(row, row % 4, long, row % 2 === 0 ? {} : { textColor: palette(row % 8) });
        if (row % 3 === 0) {
          sheet.put(row, 6, "stop");
        }
      }
      return sheet;
    })(),
    scroll: { top: 5, left: 4 * 80 + 21 },
  }),
  frozen: () => ({
    sheet: filled(),
    frozenRows: 2,
    frozenCols: 1,
    scroll: { top: 24 * 70 + 9, left: 80 * 15 + 31 },
    overlays: {
      selection: { row: 1, col: 0 },
      selectionRange: span(1, 2, 0, 1),
    },
  }),
  "frozen-overflow": () => ({
    sheet: (() => {
      const sheet = new SyntheticSheet();
      const long = `${word(1)} ${word(2)} ${han(4, 8)} ${word(6)} ${word(7)}`;
      for (let row = 0; row < 14; row++) {
        sheet.put(row, row % 3, long).put(row, 3 + (row % 2), row % 4 === 0 ? "" : long);
      }
      return sheet;
    })(),
    frozenRows: 1,
    frozenCols: 2,
    scroll: { top: 13, left: 40 },
  }),
  "custom-sizes": () => ({
    sheet: filled(),
    rowHeights: [
      [1, 12],
      [2, 41],
      [4, 3],
      [5, 17],
      [8, 63],
    ],
    colWidths: [
      [0, 30],
      [1, 141],
      [2, 6],
      [3, 55],
      [5, 13],
    ],
  }),
  borders: () => ({ sheet: borders() }),
  styles: () => ({ sheet: styled() }),
  selection: () => ({
    sheet: styled(),
    overlays: { selection: { row: 2, col: 1 }, selectionRange: span(2, 7, 1, 4) },
  }),
  "multi-selection": () => ({
    sheet: styled(),
    scroll: { top: 7, left: 11 },
    overlays: {
      selection: { row: 6, col: 2 },
      selectionRange: span(5, 8, 2, 5),
      selectionRanges: [span(0, 4, 0, 2), span(3, 4, 1, 7), span(5, 8, 2, 5)],
    },
  }),
  "formula-references": () => ({
    sheet: filled(),
    overlays: {
      formulaReferences: Array.from({ length: 8 }, (_, index) => ({
        colorIndex: index,
        firstRow: index,
        lastRow: index + (index % 3),
        firstCol: index % 4,
        lastCol: (index % 4) + (index % 2),
      })),
    },
  }),
  dashes: () => ({
    sheet: new SyntheticSheet().fill(sparse),
    overlays: {
      clipboardRange: { span: span(1, 4, 0, 2), cut: false },
      activeSpillRange: { firstRow: 2, lastRow: 8, firstCol: 3, lastCol: 3 },
      fillPreview: span(6, 9, 0, 2),
      selection: { row: 5, col: 0 },
      selectionRange: span(5, 6, 0, 2),
    },
  }),
  "dashes-cut-off": () => ({
    sheet: new SyntheticSheet().fill(sparse),
    scroll: { top: 24 * 2 + 7, left: 53 },
    overlays: {
      clipboardRange: { span: span(0, 6, 0, 3), cut: true },
      activeSpillRange: { firstRow: 1, lastRow: 600, firstCol: 4, lastCol: 120 },
      fillPreview: span(3, 8, 1, 40),
    },
  }),
  filter: () => ({
    sheet: filled(),
    overlays: {
      filterState: {
        headerRow: 1,
        lastRow: 16,
        firstCol: 0,
        lastCol: 3,
        activeCols: [1, 3],
        hiddenRowSpans: FILTER_HIDDEN,
      },
      filterRowMap: new FilterRowMap(1000, FILTER_HIDDEN),
    },
  }),
  diff: () => ({
    sheet: filled(),
    scroll: { top: 5, left: 3 },
    overlays: {
      diffHighlights: new Map([
        ["1:0", "added"],
        ["1:1", "added"],
        ["3:1", "conflict"],
        ["3:2", "deleted"],
        ["5:0", "deleted"],
        ["6:3", "conflict"],
        ["0:4", "added"],
      ]),
    },
  }),
  collaborators: () => ({
    sheet: filled(),
    scroll: { top: 6, left: 0 },
    overlays: {
      remotePresences: [
        {
          replica: 1,
          displayName: "Ada",
          colorSlot: 0,
          cursorRow: 2,
          cursorCol: 0,
          selection: span(2, 5, 0, 2),
        },
        {
          replica: 2,
          displayName: han(9, 3),
          colorSlot: 3,
          cursorRow: 0,
          cursorCol: 1,
          selection: undefined,
        },
        {
          replica: 3,
          displayName: "A name that does not fit its tag",
          colorSlot: 5,
          cursorRow: 6,
          cursorCol: 4,
          selection: span(6, 8, 3, 5),
        },
        {
          replica: 4,
          displayName: "Analyst",
          colorSlot: 0x80 | 5,
          cursorRow: 9,
          cursorCol: 1,
          selection: span(8, 11, 1, 3),
        },
      ],
    },
  }),
  "header-labels": () => ({
    sheet: new SyntheticSheet(1_000_000, 1_000),
    scroll: { top: 24 * 999_870 + 5, left: 80 * 696 + 9 },
    overlays: {
      selection: { row: 999_880, col: 699 },
      selectionRange: span(999_878, 999_890, 699, 704),
    },
  }),
  placeholders: () => ({
    sheet: filled(),
    scroll: { top: 24 * 58 + 3, left: 80 * 13 + 40 },
    missing: (pageRow, pageCol) => pageRow % 2 !== pageCol % 2,
  }),
  "reorder-guide": () => ({
    sheet: new SyntheticSheet(200, 40).fill(sparse),
    overlays: {
      reorderGuide: { axis: "row", beforeRow: 3 },
      selection: { row: 0, col: 0 },
      selectionRange: span(0, 200, 0, 40),
    },
  }),
  "every-overlay": () => ({
    sheet: styled(),
    frozenRows: 1,
    frozenCols: 1,
    scroll: { top: 19, left: 37 },
    overlays: {
      selection: { row: 3, col: 1 },
      selectionRange: span(3, 6, 1, 3),
      formulaReferences: [{ colorIndex: 4, firstRow: 0, lastRow: 4, firstCol: 0, lastCol: 1 }],
      clipboardRange: { span: span(2, 5, 0, 3), cut: false },
      activeSpillRange: { firstRow: 4, lastRow: 7, firstCol: 2, lastCol: 2 },
      fillPreview: span(6, 8, 1, 3),
      diffHighlights: new Map([
        ["4:1", "conflict"],
        ["4:2", "deleted"],
      ]),
      remotePresences: [
        {
          replica: 1,
          displayName: "Ada",
          colorSlot: 2,
          cursorRow: 5,
          cursorCol: 2,
          selection: span(3, 6, 0, 3),
        },
      ],
      reorderGuide: { axis: "col", beforeCol: 2 },
      filterState: {
        headerRow: 0,
        lastRow: 12,
        firstCol: 0,
        lastCol: 2,
        activeCols: [0],
        hiddenRowSpans: [],
      },
    },
  }),
  "zoom-quarter": () => ({ sheet: styled(), zoom: 0.25, ...UNEVEN, overlays: ZOOMED }),
  // Labels of every second row and column, the first ones cut off.
  "zoom-quarter-headers": () => ({
    sheet: new SyntheticSheet(1_000_000, 1_000).fill(sparse),
    zoom: 0.25,
    scroll: { top: 6 * 999_870 + 3, left: 20 * 700 + 7 },
    overlays: {
      selection: { row: 999_880, col: 704 },
      selectionRange: span(999_878, 999_890, 704, 711),
    },
  }),
  "zoom-half": () => ({
    sheet: styled(),
    zoom: 0.5,
    ...UNEVEN,
    scroll: { top: 5, left: 3 },
    overlays: ZOOMED,
  }),
  "zoom-half-frozen": () => ({
    sheet: filled(),
    zoom: 0.5,
    ...UNEVEN,
    frozenRows: 3,
    frozenCols: 2,
    scroll: { top: 12 * 70 + 5, left: 40 * 15 + 13 },
    overlays: {
      selection: { row: 78, col: 18 },
      selectionRange: span(78, 79, 18, 19),
    },
  }),
  "zoom-one-and-a-half": () => ({
    sheet: styled(),
    zoom: 1.5,
    ...UNEVEN,
    scroll: { top: 11, left: 17 },
    overlays: ZOOMED,
  }),
  "zoom-double": () => ({
    sheet: filled(),
    zoom: 2,
    frozenRows: 1,
    frozenCols: 1,
    scroll: { top: 9, left: 31 },
    overlays: ZOOMED,
  }),
  "column-names": () => ({
    sheet: new SyntheticSheet(1000, 9).fill(mixed),
    scroll: { top: 24 * 3 + 5, left: 21 },
    colWidths: [
      [0, 104],
      [1, 141],
      [3, 33],
      [4, 61],
      [5, 18],
    ],
    overlays: {
      ...table([
        { name: "jellyfish of the fjord" },
        { name: han(3, 24) },
        { name: `${word(0)} ${han(7, 9)}` },
        { name: word(5) },
        { name: `${han(11, 2)}${EMOJI}${EMOJI}${EMOJI}` },
        { name: word(8) },
        { name: "id" },
        { name: `${HEBREW} ${HEBREW}` },
      ]),
      selection: { row: 6, col: 1 },
      selectionRange: span(5, 8, 1, 3),
    },
  }),
  "column-sort": () => ({
    sheet: new SyntheticSheet(1000, 12).fill(mixed),
    scroll: { top: 0, left: 13 },
    colWidths: [
      [1, 120],
      [3, 47],
      [4, 31],
    ],
    overlays: table([
      { name: word(0), sort: "ascending" },
      { name: word(1), sort: "descending", sortOrder: 1 },
      { name: han(5, 2), sort: "ascending", sortOrder: 2 },
      { name: word(2), sort: "descending", sortOrder: 3 },
      { name: word(3), sort: "ascending", sortOrder: 14 },
      { name: `${word(4)} ${word(5)}`, sort: "descending" },
    ]),
  }),
  "column-filters": () => ({
    sheet: new SyntheticSheet(1000, 12).fill(mixed),
    scroll: { top: 11, left: 0 },
    colWidths: [
      [2, 46],
      [3, 44],
    ],
    overlays: {
      ...table([
        { name: word(0), filtered: true },
        { name: word(1) },
        { name: word(2), filtered: true },
        { name: word(3), filtered: true },
        { name: han(9, 6), filtered: true, sort: "descending", sortOrder: 2 },
      ]),
      selection: { row: 0, col: 1 },
      selectionRange: span(0, 1000, 1, 2),
      selectionRanges: [span(0, 1000, 4, 5), span(0, 1000, 1, 2)],
    },
  }),
  "column-names-frozen": () => ({
    sheet: new SyntheticSheet(1000, 24).fill(mixed),
    zoom: 1.5,
    frozenCols: 2,
    scroll: { top: 36 * 70 + 7, left: 120 * 15 + 44 },
    colWidths: [
      [0, 64],
      [1, 49],
    ],
    overlays: table(
      Array.from({ length: 24 }, (_, col) => ({
        name: `${han(col * 3, 1 + (col % 3))} ${word(col)}`,
        sort: col % 4 === 1 ? "ascending" : null,
        sortOrder: col > 4 ? 2 : 1,
        filtered: col % 3 === 0,
      })),
    ),
  }),
  // The last rows there can be, and headers as wide as their numbers.
  "column-names-zoom-half": () => ({
    sheet: new SyntheticSheet(100_000_000, 30).fill(mixed),
    zoom: 0.5,
    frozenCols: 1,
    headerWidth: 63,
    scroll: { top: 12 * 99_999_978 + 5, left: 40 * 3 + 9 },
    overlays: table(
      Array.from({ length: 30 }, (_, col) => ({
        name: `${word(col)} ${han(col, 4)}`,
        sort: col % 5 === 0 ? "descending" : null,
        filtered: col % 2 === 0,
      })),
    ),
  }),
  "clipped-text": () => ({
    sheet: (() => {
      const sheet = new SyntheticSheet(1000, 8);
      const long = `${word(2)} ${word(3)} ${han(4, 8)} ${word(6)}`;
      for (let row = 0; row < 14; row++) {
        sheet.put(row, row % 4, long, row % 5 === 2 ? { italic: true, strikethrough: true } : {});
      }
      sheet.put(2, 4, "1234567890123456789.25").put(3, 4, "#VALUE! not supported here");
      sheet.put(4, 5, long, { align: "right" }).put(5, 5, long, { align: "center" });
      return sheet;
    })(),
    scroll: { top: 5, left: 27 },
    colWidths: [
      [1, 33],
      [2, 13],
      [3, 12],
    ],
    overlays: { clipText: true },
  }),
};

/** Rows and columns from a hair's breadth to most of the picture. */
const UNEVEN = {
  rowHeights: [
    [1, 40],
    [2, 7],
    [6, 120],
    [9, 16],
    [10, 3],
    [17, 61],
  ],
  colWidths: [
    [0, 33],
    [1, 141],
    [2, 24],
    [4, 9],
    [5, 203],
  ],
} as const satisfies Pick<Setting, "rowHeights" | "colWidths">;

const ZOOMED: Overlays = {
  selection: { row: 3, col: 1 },
  selectionRange: span(3, 6, 1, 4),
  clipboardRange: { span: span(4, 9, 0, 2), cut: false },
  formulaReferences: [{ colorIndex: 2, firstRow: 0, lastRow: 4, firstCol: 2, lastCol: 3 }],
  remotePresences: [
    {
      replica: 1,
      displayName: "Ada",
      colorSlot: 0,
      cursorRow: 8,
      cursorCol: 3,
      selection: span(7, 11, 2, 4),
    },
  ],
  filterState: {
    headerRow: 1,
    lastRow: 40,
    firstCol: 0,
    lastCol: 5,
    activeCols: [1],
    hiddenRowSpans: [],
  },
};

function pageSpan(cells: Span): Span {
  return {
    firstRow: Math.floor(cells.firstRow / PAGE_ROWS),
    endRow: Math.ceil(cells.endRow / PAGE_ROWS),
    firstCol: Math.floor(cells.firstCol / PAGE_COLS),
    endCol: Math.ceil(cells.endCol / PAGE_COLS),
  };
}

/** A cache that holds these pages for a frame. */
export function cacheOf(
  pages: readonly Page[],
  visible: Span,
  frozenRows: number,
  frozenCols: number,
): PageCache {
  const cache = new PageCache();
  cache.retain(pageSpan(visible), frozenRows, frozenCols);
  for (const page of pages) {
    cache.accept(page);
  }
  return cache;
}

export function buildCase(name: CaseName, theme: Theme, scale: number): BuiltCase {
  const setting = SETTINGS[name]();
  const { sheet, zoom, frozenRows = 0, frozenCols = 0, overlays = {} } = setting;
  const base = { ...METRICS, headerWidth: setting.headerWidth ?? METRICS.headerWidth };
  const metrics = zoom === undefined ? base : viewMetrics(base, zoom, scale);
  const enlarged = Math.min(MOST_ENLARGED, Math.max(1, 1 / scale));
  const size = { width: SIZE.width * enlarged, height: SIZE.height * enlarged };
  const snap = (position: number) => Math.round(position * scale) / scale;
  const scroll = { top: snap(setting.scroll?.top ?? 0), left: snap(setting.scroll?.left ?? 0) };
  const layout = {
    rowAxis: new SparseAxis(METRICS.rowHeight, setting.rowHeights ?? [], scale, zoom),
    colAxis: new SparseAxis(METRICS.colWidth, setting.colWidths ?? [], scale, zoom),
    frozenRows,
    frozenCols,
  };
  const rows = overlays.filterRowMap?.visibleRows ?? sheet.rows;
  const extent = { rows, cols: sheet.cols };
  const visible = visibleCells(metrics, extent, scroll, size, layout);

  const pages: Page[] = [];
  const held = pageSpan(visible);
  const strip = (first: number, end: number, frozen: number, per: number) => {
    const all = new Set(Array.from({ length: end - first }, (_, index) => first + index));
    for (let page = 0; page < Math.ceil(frozen / per); page++) {
      all.add(page);
    }
    return [...all];
  };
  for (const pageRow of strip(held.firstRow, held.endRow, frozenRows, PAGE_ROWS)) {
    for (const pageCol of strip(held.firstCol, held.endCol, frozenCols, PAGE_COLS)) {
      if (!setting.missing?.(pageRow, pageCol)) {
        pages.push(sheet.page(pageRow, pageCol));
      }
    }
  }
  return {
    pages,
    frame: {
      size,
      scale,
      ...(zoom === undefined ? {} : { zoom }),
      metrics,
      theme,
      scroll,
      visible,
      cache: cacheOf(pages, visible, frozenRows, frozenCols),
      selection: null,
      sheet: extent,
      layout,
      styles: sheet.styles,
      ...overlays,
    },
  };
}
