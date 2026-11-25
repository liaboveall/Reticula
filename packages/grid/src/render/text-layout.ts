import { isBool, isError, isNumber, type Page } from "../page.ts";
import type { PackedColor } from "./color.ts";
import {
  ALIGN_AUTO,
  ALIGN_CENTER,
  ALIGN_LEFT,
  ALIGN_RIGHT,
  type Look,
  NO_COLOR,
  type ResolvedStyle,
  type StyleTable,
} from "./look.ts";
import { lineWidth, type PixelBox, RectList } from "./rects.ts";
import type { TextMeasure } from "./text-measure.ts";

/** Where the rows and columns of a page are, in device pixels from its origin. */
export interface PageGeometry {
  /** Edges of the columns: one more than there are columns. */
  readonly xs: Int32Array;
  /** Edges of the rows: one more than there are rows. */
  readonly ys: Int32Array;
  /** Left and right edge of the column each overflow record comes from. */
  readonly overflowXs: Int32Array;
}

/** What text is laid out with. */
export interface TextSetting {
  readonly measure: TextMeasure;
  readonly look: Look;
  readonly styles: StyleTable;
  /** Frozen columns of the sheet: their text ends where they do. */
  readonly frozenCols: number;
}

const NO_RECTS = new RectList(1);

interface Field<T> {
  [index: number]: T;
  slice(first: number, end: number): ArrayLike<T>;
}

/** Has the items of a list from one on be others of them. */
function reorder<T>(field: Field<T>, first: number, order: readonly number[]): void {
  const before = field.slice(first, first + order.length);
  order.forEach((from, index) => {
    field[first + index] = before[from - first] as T;
  });
}

/**
 * Texts to draw, each with the position of its pen: whole device pixels along
 * the baseline's normal, fractions of them along the baseline.
 */
export class TextRuns {
  length = 0;
  texts: string[];
  /** Left end of the pen's way. */
  x: Float64Array;
  /** How far the pen moves. */
  width: Float64Array;
  baseline: Int32Array;
  /** Variant of the sheet's font. */
  font: Uint8Array;
  color: Uint32Array;
  /** Row of the cell within its page. */
  row: Uint16Array;
  /** 1 where the run is drawn through its own clip. */
  clipped: Uint8Array;
  /** Left, top, right and bottom of the clip. */
  clip: Int32Array;
  /** Lines through the texts, painted over them. */
  strikes = NO_RECTS;

  constructor(capacity = 64) {
    // Those far down a page may be the first to be laid out.
    this.texts = new Array<string>(capacity).fill("");
    const block = new ArrayBuffer(capacity * 44);
    let at = 0;
    const take = (bytes: number): number => {
      at += capacity * bytes;
      return at - capacity * bytes;
    };
    this.x = new Float64Array(block, take(8), capacity);
    this.width = new Float64Array(block, take(8), capacity);
    this.clip = new Int32Array(block, take(16), capacity * 4);
    this.baseline = new Int32Array(block, take(4), capacity);
    this.color = new Uint32Array(block, take(4), capacity);
    this.row = new Uint16Array(block, take(2), capacity);
    this.font = new Uint8Array(block, take(1), capacity);
    this.clipped = new Uint8Array(block, take(1), capacity);
  }

  /** Forgets the texts, but not how many there is room for. */
  clear(): void {
    this.length = 0;
    this.texts.fill("");
    this.strikes = NO_RECTS;
  }

  /** Adds a run that needs no clip. */
  push(
    text: string,
    x: number,
    width: number,
    baseline: number,
    font: number,
    color: PackedColor,
    row: number,
  ): number {
    const run = this.length;
    if (run === this.x.length) {
      this.#grow();
    }
    this.texts[run] = text;
    this.x[run] = x;
    this.width[run] = width;
    this.baseline[run] = baseline;
    this.font[run] = font;
    this.color[run] = color;
    this.row[run] = row;
    this.clipped[run] = 0;
    this.length++;
    return run;
  }

  setClip(run: number, left: number, top: number, right: number, bottom: number): void {
    this.clipped[run] = 1;
    this.clip[run * 4] = left;
    this.clip[run * 4 + 1] = top;
    this.clip[run * 4 + 2] = right;
    this.clip[run * 4 + 3] = bottom;
  }

  /** Puts those of some runs together that have the same font and colour. */
  batch(first: number, end: number): void {
    const { font, color } = this;
    let run = first + 1;
    while (run < end && font[run] === font[first] && color[run] === color[first]) {
      run++;
    }
    if (run >= end) {
      return;
    }
    const states = new Map<number, number[]>();
    for (run = first; run < end; run++) {
      const state = (font[run] ?? 0) * 2 ** 32 + (color[run] ?? 0);
      const members = states.get(state);
      if (members) {
        members.push(run);
      } else {
        states.set(state, [run]);
      }
    }
    const order = [...states.values()].flat();
    const { texts, x, width, baseline, row, clipped, clip } = this;
    reorder(texts, first, order);
    for (const field of [x, width, baseline, font, color, row, clipped]) {
      reorder(field, first, order);
    }
    const sides = [0, 1, 2, 3];
    const clips = order.flatMap((from) => sides.map((side) => from * 4 + side));
    reorder(clip, first * 4, clips);
  }

  #copy(from: TextRuns): void {
    this.texts = [...from.texts];
    this.x.set(from.x);
    this.width.set(from.width);
    this.baseline.set(from.baseline);
    this.font.set(from.font);
    this.color.set(from.color);
    this.row.set(from.row);
    this.clipped.set(from.clipped);
    this.clip.set(from.clip);
  }

  #grow(): void {
    const grown = new TextRuns(Math.max(16, this.x.length * 2));
    grown.#copy(this);
    Object.assign(this, { ...grown, length: this.length, strikes: this.strikes });
  }
}

/** The colour text is drawn in. */
function inkOf(style: ResolvedStyle, look: Look): PackedColor {
  return style.text === NO_COLOR ? look.text : style.text;
}

/** Draws the line through a text, as far as the text itself is drawn. */
function strike(runs: TextRuns, run: number, look: Look, box: PixelBox): void {
  const x = runs.x[run] ?? 0;
  const left = Math.max(box.left, Math.round(x));
  const right = Math.min(box.right, Math.round(x + (runs.width[run] ?? 0)));
  const top = Math.round((box.top + box.bottom) / 2);
  const height = lineWidth(1, look.scale * look.zoom);
  if (runs.strikes === NO_RECTS) {
    runs.strikes = new RectList(8);
  }
  runs.strikes.push(left, top, right - left, height, runs.color[run] ?? 0);
}

/**
 * Lays out the texts of some rows of a page. What comes out depends on nothing
 * outside the page: text is cut at the page's edges, and text that runs on is
 * drawn again by the pages it runs into, from their overflow records.
 */
export function layoutText(
  page: Page,
  geometry: PageGeometry,
  setting: TextSetting,
  firstRow: number,
  endRow: number,
  runs: TextRuns,
): void {
  const { xs, ys, overflowXs } = geometry;
  const { measure, look, styles } = setting;
  const { colCount, texts } = page;
  const { padding } = look;
  const frozen = setting.frozenCols - page.firstCol;

  /** The columns a text may run across, up to its page's edge or the freeze line. */
  const limitOf = (frozenOrigin: boolean) => (frozenOrigin ? Math.min(colCount, frozen) : colCount);

  /** The column before which text that wants to reach `end` stops, for a cell in its way or for `limit`. */
  const stopOf = (row: number, from: number, limit: number, end: number): number => {
    let stop = from;
    while (stop < limit && (xs[stop] ?? 0) - padding < end && !texts[row * colCount + stop]) {
      stop++;
    }
    return stop;
  };

  // At the limit there is nothing to keep a distance from: the text goes on there.
  const rightOf = (stop: number, limit: number, end: number): number => {
    const padded = (xs[stop] ?? 0) - padding;
    return stop === limit && padded < end ? (xs[stop] ?? 0) : padded;
  };

  const overflows = page.overflows;
  for (let index = 0; index < overflows.length; index++) {
    const record = overflows[index];
    if (!record || record.rowOffset < firstRow || record.rowOffset >= endRow) {
      continue;
    }
    const row = record.rowOffset;
    const top = ys[row] ?? 0;
    const bottom = ys[row + 1] ?? 0;
    const origin = overflowXs[index * 2] ?? 0;
    const limit = limitOf(record.originCol < setting.frozenCols);
    if (
      limit <= 0 ||
      bottom <= top ||
      // A hidden column shows none of its text.
      origin >= (overflowXs[index * 2 + 1] ?? 0) ||
      texts[row * colCount] !== ""
    ) {
      continue;
    }
    const style = styles.get(record.styleId);
    const width = measure.width(look.fonts[style.font] ?? "", record.text);
    const x = origin + padding;
    if (x + width <= 0) {
      continue;
    }
    const stop = stopOf(row, 1, limit, x + width);
    const right = rightOf(stop, limit, x + width);
    if (right <= 0) {
      continue;
    }
    const baseline = Math.round((top + bottom) / 2 + look.lineOf(style.font).lift);
    const run = runs.push(record.text, x, width, baseline, style.font, inkOf(style, look), row);
    runs.setClip(run, 0, top, right, bottom);
    if (style.strikethrough) {
      strike(runs, run, look, { left: 0, top, right, bottom });
    }
  }

  for (let col = 0; col < colCount; col++) {
    const left = xs[col] ?? 0;
    const cellRight = xs[col + 1] ?? 0;
    if (cellRight <= left) {
      continue;
    }
    const room = cellRight - left - 2 * padding;
    for (let row = firstRow; row < endRow; row++) {
      const cell = row * colCount + col;
      const text = texts[cell];
      if (!text) {
        continue;
      }
      const top = ys[row] ?? 0;
      const bottom = ys[row + 1] ?? 0;
      if (bottom <= top) {
        continue;
      }
      const flags = page.flags[cell] ?? 0;
      const style = styles.get(page.styleIds[cell] ?? 0);
      const error = isError(flags);
      const align =
        style.align !== ALIGN_AUTO
          ? style.align
          : isNumber(flags)
            ? ALIGN_RIGHT
            : error || isBool(flags)
              ? ALIGN_CENTER
              : ALIGN_LEFT;
      if (align !== ALIGN_LEFT && room <= 0) {
        continue;
      }
      const color = error ? look.errorText : inkOf(style, look);
      const width = measure.width(look.fonts[style.font] ?? "", text);
      let x = left + padding;
      let right = cellRight - padding;
      let fits = width <= room;
      if (align === ALIGN_RIGHT) {
        x = right - width;
      } else if (align === ALIGN_CENTER) {
        x = (left + cellRight - width) / 2;
      } else if (!fits) {
        // Left-aligned text runs on over the empty cells to its right.
        const limit = limitOf(col < frozen);
        const stop = stopOf(row, col + 1, limit, x + width);
        right = rightOf(stop, limit, x + width);
        fits = x + width <= (xs[stop] ?? 0) - padding;
        if (right <= x) {
          continue;
        }
      }
      const line = look.lineOf(style.font);
      const baseline = Math.round((top + bottom) / 2 + line.lift);
      const run = runs.push(text, x, width, baseline, style.font, color, row);
      const clipLeft = align === ALIGN_LEFT ? left : left + padding;
      if (!fits || line.height > bottom - top) {
        runs.setClip(run, clipLeft, top, right, bottom);
      }
      if (style.strikethrough) {
        strike(runs, run, look, { left: clipLeft, top, right, bottom });
      }
    }
  }
}
