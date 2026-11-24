import type { PageCache } from "../cache.ts";
import { columnName } from "../layout.ts";
import type { Theme } from "../theme.ts";
import { type GridCellStyle, PADDING } from "./frame.ts";

/** How far a font reaches above and below its baseline. */
export interface FontLine {
  readonly ascent: number;
  readonly descent: number;
}

/** How far the pixels of a text reach from the point its pen starts at. */
export interface Ink extends FontLine {
  readonly left: number;
  readonly right: number;
}

/** Sizes of text, in the pixels its CSS `font` is in. */
export interface TextMeasure {
  /** How far the pen moves; with kerning off, the sum of what each character moves it. */
  width(font: string, text: string): number;
  line(font: string): FontLine;
  ink(font: string, text: string): Ink;
}

/** A 2D context whose text comes out the way every renderer draws it. */
export function textContext(canvas: HTMLCanvasElement): CanvasRenderingContext2D {
  // A canvas with alpha: the text on an opaque one has coloured edges on
  // screens of low density, which no other renderer can reproduce.
  const context = canvas.getContext("2d", { alpha: true });
  if (!context) {
    throw new Error("this browser cannot draw on a 2D canvas");
  }
  resetText(context);
  return context;
}

/** The state of a context that resizing and losing it set back. */
export function resetText(context: CanvasRenderingContext2D): void {
  context.textAlign = "left";
  context.textBaseline = "alphabetic";
  context.direction = "ltr";
  // Without it the width of a text is the sum of its characters' widths, and a
  // renderer that puts texts together from characters gets the same picture.
  context.fontKerning = "none";
}

/** Variants of the sheet's font, as bits of an index. */
export const BOLD = 1;
export const ITALIC = 2;

export function fontOf(theme: Theme, variant: number, scale = 1): string {
  const slant = variant & ITALIC ? "italic " : "";
  const weight = variant & BOLD ? "700 " : "";
  return `${slant}${weight}${theme.fontSize * scale}px ${theme.fontFamily}`;
}

/** Widths of up to this many texts are kept. */
const MAX_WIDTHS = 131_072;

/** Fonts, at their sizes, that widths are kept of. */
const MAX_FONTS = 16;

/**
 * Alphabets whose characters' widths add up to the width of a text of them,
 * which spares measuring every text: first and last character, and a text that
 * finds out fonts that join some of them or set them apart.
 */
const ALPHABETS = [
  { first: 0x20, last: 0x7e, sample: 'ffi fl tt -> => <= != === www 0x0 :: // "AV" To,' },
  { first: 0x4e00, last: 0x9fff, sample: "永a东1国酱爱" },
  { first: 0xc0, last: 0x24f, sample: "Äffin ÆØ çéñ Tö Łwów ĳ" },
  { first: 0x400, last: 0x45f, sample: "Тогда AРУчёт ГД" },
];

/** The widths of one font's texts. */
class FontWidths {
  readonly font: string;
  /** By text; the older ones are still to be had but no longer added to. */
  newer = new Map<string, number>();
  older: ReadonlyMap<string, number> = new Map();
  /** What each character moves the pen; `NaN` until it is needed. Grows with the characters. */
  advances = new Float64Array(0x80).fill(Number.NaN);
  /** Which of the alphabets add up. */
  readonly addUp: (boolean | undefined)[] = [];

  constructor(font: string) {
    this.font = font;
  }

  setAdvance(code: number, advance: number): void {
    if (code >= this.advances.length) {
      const grown = new Float64Array(code < 0x500 ? 0x500 : 0xa000).fill(Number.NaN);
      grown.set(this.advances);
      this.advances = grown;
    }
    this.advances[code] = advance;
  }
}

/** Measures text with a 2D context of its own, and remembers the widths. */
export class TextMeasurer implements TextMeasure {
  #open: () => CanvasRenderingContext2D;
  #limit: number;
  #context: CanvasRenderingContext2D | null = null;
  #fonts = new Map<string, FontWidths>();
  #current = new FontWidths("");
  /** Texts among the newer ones of all fonts. */
  #newer = 0;
  #lines = new Map<string, FontLine>();

  constructor(
    open = () => {
      // Too small to be worth a place on the GPU, and so never lost with it.
      const canvas = document.createElement("canvas");
      canvas.width = 1;
      canvas.height = 1;
      return textContext(canvas);
    },
    limit: number = MAX_WIDTHS,
  ) {
    this.#open = open;
    this.#limit = limit;
  }

  /** How many widths are remembered, apart from those of single characters. */
  get size(): number {
    let size = 0;
    for (const { newer, older } of this.#fonts.values()) {
      size += newer.size + older.size;
    }
    return size;
  }

  /** Forgets everything that was measured. */
  reset(): void {
    this.#fonts.clear();
    this.#current = new FontWidths("");
    this.#newer = 0;
    this.#lines.clear();
  }

  width(font: string, text: string): number {
    if (font !== this.#current.font) {
      this.#current = this.#fontWidths(font);
    }
    const widths = this.#current;
    const sum = this.#sum(widths, text);
    if (sum >= 0) {
      return sum;
    }
    let width = widths.newer.get(text);
    if (width === undefined) {
      width = widths.older.get(text) ?? this.#using(font).measureText(text).width;
      if (this.#newer >= this.#limit / 2) {
        this.#age();
      }
      widths.newer.set(text, width);
      this.#newer++;
    }
    return width;
  }

  line(font: string): FontLine {
    let line = this.#lines.get(font);
    if (!line) {
      const measured = this.#using(font).measureText("Hg");
      line = {
        ascent: measured.fontBoundingBoxAscent,
        descent: measured.fontBoundingBoxDescent,
      };
      this.#lines.set(font, line);
    }
    return line;
  }

  ink(font: string, text: string): Ink {
    const measured = this.#using(font).measureText(text);
    return {
      left: measured.actualBoundingBoxLeft,
      right: measured.actualBoundingBoxRight,
      ascent: measured.actualBoundingBoxAscent,
      descent: measured.actualBoundingBoxDescent,
    };
  }

  /** The width in CSS pixels that a column needs for its name and the texts of some rows. */
  columnFitWidth(
    theme: Theme,
    cache: PageCache,
    styles: readonly GridCellStyle[] | undefined,
    col: number,
    firstRow: number,
    endRow: number,
  ): number {
    const header = `600 ${theme.fontSize}px ${theme.fontFamily}`;
    let fit = Math.max(48, Math.ceil(this.width(header, columnName(col)) + 2 * PADDING + 12));
    for (let row = firstRow; row < endRow; row++) {
      const page = cache.pageOf(row, col);
      if (!page || row >= page.firstRow + page.rowCount || col >= page.firstCol + page.colCount) {
        continue;
      }
      const cell = (row - page.firstRow) * page.colCount + (col - page.firstCol);
      const text = page.texts[cell];
      if (!text) {
        continue;
      }
      const style = styles?.[page.styleIds[cell] ?? 0];
      const variant = (style?.bold ? BOLD : 0) | (style?.italic ? ITALIC : 0);
      const width = Math.ceil(this.width(fontOf(theme, variant), text) + 2 * PADDING + 6);
      if (width > fit) {
        fit = Math.min(600, width);
      }
    }
    return fit;
  }

  #fontWidths(font: string): FontWidths {
    let widths = this.#fonts.get(font);
    if (!widths) {
      if (this.#fonts.size >= MAX_FONTS) {
        this.reset();
      }
      widths = new FontWidths(font);
      this.#fonts.set(font, widths);
    }
    return widths;
  }

  /** The newer widths become the older ones: what is still in use is taken over as it is asked for. */
  #age(): void {
    for (const widths of this.#fonts.values()) {
      widths.older = widths.newer;
      widths.newer = new Map();
    }
    this.#newer = 0;
  }

  /** The width of a text as the sum of its characters', or a negative number if it may not be. */
  #sum(widths: FontWidths, text: string): number {
    let { advances } = widths;
    const { length } = text;
    let sum = 0;
    for (let index = 0; index < length; index++) {
      const code = text.charCodeAt(index);
      const advance = code < advances.length ? (advances[code] as number) : Number.NaN;
      if (advance >= 0) {
        sum += advance;
        continue;
      }
      const measured = this.#advance(widths, code);
      if (measured < 0) {
        return -1;
      }
      sum += measured;
      advances = widths.advances;
    }
    return sum;
  }

  /** What a character moves the pen, or a negative number if that depends on its neighbours. */
  #advance(widths: FontWidths, code: number): number {
    const index = ALPHABETS.findIndex(({ first, last }) => code >= first && code <= last);
    const sample = ALPHABETS[index]?.sample;
    if (sample === undefined) {
      return -1;
    }
    const measured = (text: string) => this.#using(widths.font).measureText(text).width;
    if (widths.addUp[index] === undefined) {
      const whole = measured(sample);
      // For the sum, which asks the same.
      widths.addUp[index] = true;
      widths.addUp[index] = Math.abs(this.#sum(widths, sample) - whole) < 0.01 * sample.length;
      if (!widths.addUp[index]) {
        widths.advances.fill(Number.NaN);
      }
    }
    if (!widths.addUp[index]) {
      return -1;
    }
    let advance = code < widths.advances.length ? widths.advances[code] : undefined;
    if (advance === undefined || Number.isNaN(advance)) {
      advance = measured(String.fromCharCode(code));
      widths.setAdvance(code, advance);
    }
    return advance;
  }

  #using(font: string): CanvasRenderingContext2D {
    this.#context ??= this.#open();
    this.#context.font = font;
    return this.#context;
  }
}
