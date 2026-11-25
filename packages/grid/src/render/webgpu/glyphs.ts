import { alphaOf, cssColor, type PackedColor } from "../color.ts";
import type { PixelBox } from "../rects.ts";
import { rasterize, type SpriteSpec, spriteKey } from "../sprites.ts";
import { type Ink, textContext } from "../text-measure.ts";
import { type Atlas, type Sprite, STRIP_HEIGHT } from "./atlas.ts";
import type { InstanceWriter } from "./frame-buffer.ts";

// --- which texts are put together from characters -----------------------------

const ASCII = 1;
const STANDS_ALONE = 2;

/**
 * Characters whose pictures neither depend on their neighbours nor change places
 * with them: first and last. Whatever is not listed is drawn with its whole text:
 * combining marks, scripts that join or run the other way, jamo, emoji and what
 * chooses between their forms.
 */
const ALONE = [
  [0xa0, 0xac],
  [0xae, 0x24f],
  [0x370, 0x482],
  [0x48a, 0x52f],
  [0x1e00, 0x1fff],
  [0x2010, 0x2027],
  [0x2030, 0x203b],
  [0x20a0, 0x20c0],
  [0x3000, 0x3029],
  [0x3041, 0x3096],
  [0x309d, 0x312f],
  [0x3400, 0x4dbf],
  [0x4e00, 0x9fff],
  [0xac00, 0xd7a3],
  [0xf900, 0xfaff],
  [0xff01, 0xff9d],
  [0xffe0, 0xffe6],
] as const;

const KINDS = (() => {
  const kinds = new Uint8Array(0x10000);
  kinds.fill(ASCII, 0x20, 0x7f);
  for (const [first, last] of ALONE) {
    kinds.fill(STANDS_ALONE, first, last + 1);
  }
  return kinds;
})();

/** Whether a text can be put together from its characters, with or without those of ASCII. */
export function isSeparable(text: string, ascii: boolean): boolean {
  const least = ascii ? ASCII : STANDS_ALONE;
  for (let index = 0; index < text.length; index++) {
    if ((KINDS[text.charCodeAt(index)] as number) < least) {
      return false;
    }
  }
  return true;
}

/** Fractions of a pixel that characters are drawn at. */
export type Phases = 1 | 4;

/** How a font's texts come out when they are put together. */
export interface FontCheck {
  readonly phases: Phases;
  /** Whether the pictures of ASCII are those of its characters, not of ligatures. */
  readonly ascii: boolean;
}

// --- where a 2D canvas has its pen --------------------------------------------

/** Many different widths, whatever the fonts that text is in are like. */
const UNIT_SAMPLE = {
  text: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
  fonts: ["11.3px serif", "17.9px sans-serif"],
};
/** Units there may be to a pixel, depending on the screen and on the zoom of the page. */
const UNITS = { least: 6, most: 240 };

/**
 * How many of the units that a browser counts in there are to a pixel, if it
 * moves the pen by whole numbers of them, as Firefox does; else 0. What it says
 * of widths is less precise than that, and the sum of many of them off by so
 * much that a character half way between two phases ends up at the other one.
 */
export function unitsPerPixel(advances: readonly number[]): number {
  for (let units = UNITS.least; units <= UNITS.most; units++) {
    if (
      advances.every((advance) => Math.abs(advance * units - Math.round(advance * units)) < 1e-3)
    ) {
      return units;
    }
  }
  return 0;
}

function unitsOf(context: CanvasRenderingContext2D): number {
  // Elsewhere widths may be such multiples by chance, as of a font at a round size.
  if (!("mozInnerScreenX" in globalThis)) {
    return 0;
  }
  const { font } = context;
  const advances = UNIT_SAMPLE.fonts.flatMap((sample) => {
    context.font = sample;
    return [...UNIT_SAMPLE.text].map((character) => context.measureText(character).width);
  });
  context.font = font;
  return unitsPerPixel(advances);
}

// The pen is in the precision of a 2D canvas, on which the phases depend, and so
// is how far it has moved: in units, or in pixels where there are none.

function penAt(x: number, units: number): number {
  return units > 0 ? Math.fround(Math.fround(x) * units) : Math.fround(x);
}

function movedBy(moved: number, advance: number, units: number): number {
  return Math.fround(moved + (units > 0 ? Math.round(advance * units) : advance));
}

/** The phase that the pen is nearest to, in phases from the left edge. */
function stepOf(start: number, moved: number, units: number, phases: Phases): number {
  let at = Math.fround(start + moved);
  if (units > 0) {
    at = Math.fround(at / units);
  }
  return Math.floor(Math.fround(at + 0.5 / phases) * phases);
}

// --- what a font's texts are like ---------------------------------------------

// A quarter of a pixel, by which characters between two phases may be off, makes less of a difference.
const CHECK = { width: 1024, line: 28, size: 20, far: 88, most: 2 };
const PLAIN_TEXT = "Hamburg 0123456789 nimble";
/** Programming fonts have ligatures as wide as what they stand for, so that widths tell nothing. */
const JOINED_TEXT = "ff fi fl ffi ffl fj tt ti -> => == != === <= >= |> :: // www 0x0 To AV";

/**
 * Draws a text whole and from its characters, and says with how many phases
 * they come out the same. The text starts at a fraction of a pixel that tells
 * browsers apart that round to whole ones, as on some screens of low density.
 */
function phasesOf(font: string, text: string): Phases | null {
  const canvas = document.createElement("canvas");
  canvas.width = CHECK.width;
  canvas.height = 3 * CHECK.line;
  const context = textContext(canvas);
  context.font = font.replace(/[\d.]+(?=px)/, String(CHECK.size));
  const start = 4.4;
  const baseline = (line: number) => Math.round((line + 0.75) * CHECK.line);
  context.fillText(text, start, baseline(0));
  const rules = [4, 1] as const;
  const units = unitsOf(context);
  for (const [index, phases] of rules.entries()) {
    const pen = penAt(start, units);
    let moved = 0;
    for (const character of text) {
      const x = stepOf(pen, moved, units, phases) / phases;
      context.fillText(character, x, baseline(index + 1));
      moved = movedBy(moved, context.measureText(character).width, units);
    }
  }
  const { data } = context.getImageData(0, 0, CHECK.width, 3 * CHECK.line);
  const bytes = CHECK.width * CHECK.line * 4;
  for (const [index, phases] of rules.entries()) {
    let far = 0;
    for (let at = 3; at < bytes; at += 4) {
      const composed = data[(index + 1) * bytes + at] ?? 0;
      far += Math.abs((data[at] ?? 0) - composed) > CHECK.far ? 1 : 0;
    }
    if (far <= CHECK.most) {
      return phases;
    }
  }
  return null;
}

export function checkFont(font: string): FontCheck {
  const phases = phasesOf(font, PLAIN_TEXT);
  return {
    phases: phases ?? 4,
    ascii: phases !== null && phasesOf(font, JOINED_TEXT) === phases,
  };
}

// --- their pictures ----------------------------------------------------------

/** The picture of a character at a phase. */
interface Glyph extends Sprite {
  /** From the pen, on whole pixels, to the picture. */
  readonly dx: number;
  readonly dy: number;
  /** How far the pen moves. */
  readonly advance: number;
}

/** The picture of a whole text, in blocks of which only some may have been drawn. */
interface Whole {
  readonly dx: number;
  readonly dy: number;
  readonly width: number;
  readonly height: number;
  readonly blocks: (Sprite | undefined)[];
}

/** Sides of a block. */
const BLOCK = 512;

/** Characters with a place in an array. */
const FAST = 0x80;

/** Pictures of one font in one colour. */
class GlyphSet {
  readonly css: string;
  /** By character and phase. */
  readonly fast = new Array<Glyph | undefined>(FAST * 4).fill(undefined);
  readonly glyphs = new Map<number, Glyph>();
  /** By the fraction of a pixel it starts at and the text. */
  readonly texts = new Map<string, Whole>();

  constructor(color: PackedColor) {
    this.css = cssColor(color);
  }

  set(key: number, glyph: Glyph): void {
    if (key < this.fast.length) {
      this.fast[key] = glyph;
    } else {
      this.glyphs.set(key, glyph);
    }
  }

  /** Forgets what was in a layer. Returns whether there is anything left. */
  evict(layer: number): boolean {
    let held = false;
    for (const [key, glyph] of this.fast.entries()) {
      if (glyph?.layer === layer) {
        this.fast[key] = undefined;
      }
      held ||= (glyph?.layer ?? -1) >= 0 && glyph?.layer !== layer;
    }
    for (const [key, glyph] of this.glyphs) {
      if (glyph.layer === layer) {
        this.glyphs.delete(key);
      }
    }
    for (const [key, { blocks }] of this.texts) {
      let left = false;
      for (const [index, block] of blocks.entries()) {
        if (block?.layer === layer) {
          blocks[index] = undefined;
        } else {
          left ||= block !== undefined;
        }
      }
      if (!left) {
        this.texts.delete(key);
      }
    }
    return held || this.glyphs.size > 0 || this.texts.size > 0;
  }
}

/** How far a text reaches from its pen, and how far the pen moves. */
interface InkSize extends Ink {
  readonly advance: number;
}

class FontGlyphs {
  readonly font: string;
  readonly size: number;
  readonly check: FontCheck;
  /** Units to a pixel that the pen is moved by. */
  readonly units: number;
  readonly sets = new Map<PackedColor, GlyphSet>();
  /** Sizes of characters, which in some browsers take longer to find out than to draw them. */
  readonly sizes = new Map<number, InkSize>();

  constructor(font: string, check: FontCheck, units: number) {
    this.font = font;
    this.size = Number.parseFloat(/([\d.]+)px/.exec(font)?.[1] ?? "0");
    this.check = check;
    this.units = units;
  }

  set(color: PackedColor): GlyphSet {
    let set = this.sets.get(color);
    if (!set) {
      set = new GlyphSet(color);
      this.sets.set(color, set);
    }
    return set;
  }
}

const NO_INK = { layer: -1, u: 0, v: 0, width: 0, height: 0, dx: 0, dy: 0 };

/** Where the pictures of a run may be, from its page's origin. */
export interface RunPlace {
  /** Where the origin is on the screen. */
  readonly origin: number;
  readonly x: number;
  readonly baseline: number;
  /** How far its pen moves. */
  readonly width: number;
  readonly clip: PixelBox | null;
  readonly row: number;
  /** What will be in view of it, as long as it is kept. */
  readonly left: number;
  readonly right: number;
}

/** Turns texts into quads that copy from the atlas, and draws there what is missing. */
export class Glyphs {
  readonly atlas: Atlas;
  /** Layers that quads have been made from, as bits; whoever keeps quads resets it. */
  layers = 0;
  /** Set when a part of a text out of view was left out. */
  partial = false;
  /** Texts that are to be drawn another time, for want of time. */
  deferred = 0;
  hits = 0;
  misses = 0;
  #check: (font: string) => FontCheck;
  #unitsOf: (context: CanvasRenderingContext2D) => number;
  #checks = new Map<string, FontCheck>();
  #fonts = new Map<string, FontGlyphs>();
  #pictures = new Map<string, Sprite>();
  #font = "";
  #color = "";
  /** Until when the frame may make anything. */
  #deadline = 0;
  /** Set when nothing else fits into the atlas, this frame. */
  #closed = false;

  constructor(
    open: (onEvict: (layer: number) => void) => Atlas,
    check: (font: string) => FontCheck = checkFont,
    units: (context: CanvasRenderingContext2D) => number = unitsOf,
  ) {
    this.atlas = open((layer) => this.#evict(layer));
    this.#check = check;
    this.#unitsOf = units;
  }

  /** Starts a frame, which may take this many milliseconds for what is new in it. */
  begin(budget: number): void {
    this.#deadline = performance.now() + budget;
    this.#closed = false;
    this.atlas.unlimited = budget === Number.POSITIVE_INFINITY;
  }

  /** Lets the frame take some milliseconds more from now, whatever it has taken so far. */
  allow(time: number): void {
    this.#deadline = Math.max(this.#deadline, performance.now() + time);
  }

  /** Whether the frame has time for more that is new. */
  get hasTime(): boolean {
    return (
      this.atlas.unlimited || (!this.#closed && performance.now() + this.atlas.due < this.#deadline)
    );
  }

  /** Finds out how a font comes out, so that no frame has to wait for that. */
  prepare(font: string): void {
    this.#fontOf(font);
  }

  /** Adds a text's quads, or a bar in its place if there is no time to draw it. */
  run(out: InstanceWriter, font: string, color: PackedColor, text: string, place: RunPlace): void {
    if (alphaOf(color) === 0) {
      return;
    }
    const glyphs = this.#fontOf(font);
    const set = glyphs.set(color);
    const { length } = out;
    const { layers } = this;
    let drawn =
      isSeparable(text, glyphs.check.ascii) && this.#characters(out, glyphs, set, text, place);
    if (drawn === false) {
      out.length = length;
      this.layers = layers;
      drawn = this.#whole(out, glyphs, set, text, place);
    }
    if (drawn === null) {
      out.length = length;
      this.layers = layers;
      this.deferred++;
      bar(out, glyphs.size, color, place);
    }
  }

  /** The sprite of a small picture, or `null` if there is no room for it now. */
  picture(spec: SpriteSpec): Sprite | null {
    const key = spriteKey(spec);
    let sprite: Sprite | null | undefined = this.#pictures.get(key);
    if (!sprite) {
      const { width, height } = spec;
      const image = rasterize(spec);
      sprite = this.#draw(width, height, (x, y) =>
        this.atlas.raster.context.drawImage(image, x, y),
      );
      if (sprite) {
        this.#pictures.set(key, sprite);
      }
    }
    if (sprite) {
      this.layers |= 1 << sprite.layer;
    }
    return sprite;
  }

  /**
   * Puts a text together from its characters. `false` if that does not come out
   * right, `null` if one of them cannot be drawn now.
   */
  #characters(
    out: InstanceWriter,
    glyphs: FontGlyphs,
    set: GlyphSet,
    text: string,
    { origin, x, baseline, width, clip, row }: RunPlace,
  ): boolean | null {
    const { fast } = set;
    const { units } = glyphs;
    const { phases } = glyphs.check;
    const pen = penAt(origin + x, units);
    const { length } = text;
    let moved = 0;
    let found = 0;
    for (let index = 0; index < length; index++) {
      const code = text.charCodeAt(index);
      // The nearest phase: the one that a 2D canvas draws it at.
      const steps = stepOf(pen, moved, units, phases);
      const whole = phases === 4 ? steps >> 2 : steps;
      const phase = steps - whole * phases;
      const key = code * 4 + phase;
      let glyph = code < FAST ? fast[key] : set.glyphs.get(key);
      if (glyph) {
        found++;
      } else {
        const drawn = this.#glyph(glyphs, set, code, phase / phases);
        if (!drawn) {
          return drawn;
        }
        glyph = drawn;
        set.set(key, glyph);
      }
      if (glyph.layer >= 0) {
        this.layers |= 1 << glyph.layer;
        out.sprite(glyph, whole - origin + glyph.dx, baseline + glyph.dy, clip, row);
      }
      moved = movedBy(moved, glyph.advance, units);
    }
    this.hits += found;
    // Anything else in how the font sets texts shows in their widths.
    return Math.abs((units > 0 ? moved / units : moved) - width) <= 0.01 * (length + 1);
  }

  /** The picture of a character; `false` if it has none, `null` if it cannot be drawn now. */
  #glyph(glyphs: FontGlyphs, set: GlyphSet, code: number, phase: number): Glyph | false | null {
    if (!this.hasTime) {
      return null;
    }
    const text = String.fromCharCode(code);
    const context = this.#using(glyphs, set);
    let size = glyphs.sizes.get(code);
    if (!size) {
      size = sizeOf(context.measureText(text));
      glyphs.sizes.set(code, size);
    }
    const { advance } = size;
    const ink = inkOf(size, phase);
    if (!ink) {
      return { ...NO_INK, advance };
    }
    const { left, top, width, height } = ink;
    if (width > BLOCK || height > STRIP_HEIGHT) {
      return false;
    }
    const sprite = this.#draw(width, height, (x, y) =>
      context.fillText(text, x - left + phase, y - top),
    );
    return sprite && { ...sprite, dx: left, dy: top, advance };
  }

  /** Copies a text from a picture of the whole of it, as far as that can come into view. */
  #whole(
    out: InstanceWriter,
    glyphs: FontGlyphs,
    set: GlyphSet,
    text: string,
    { origin, x, baseline, clip, row, left, right }: RunPlace,
  ): true | null {
    const start = Math.fround(origin + x);
    const pen = Math.floor(start) - origin;
    const phase = start - Math.floor(start);
    const key = `${phase}:${text}`;
    let whole = set.texts.get(key);
    if (!whole) {
      const size = sizeOf(this.#using(glyphs, set).measureText(text));
      // Firefox says that text from right to left is where it would be mirrored.
      const left = Math.max(0, size.left);
      const right = Math.max(size.advance, size.right);
      const ink = inkOf({ ...size, left, right }, phase);
      if (!ink) {
        return true;
      }
      whole = { ...ink, dx: ink.left, dy: ink.top, blocks: [] };
    }
    const from = Math.max(left, clip?.left ?? left);
    const to = Math.min(right, clip?.right ?? right);
    const columns = Math.ceil(whole.width / BLOCK);
    for (let dy = 0; dy < whole.height; dy += BLOCK) {
      for (let column = 0; column < columns; column++) {
        const dx = column * BLOCK;
        const width = Math.min(BLOCK, whole.width - dx);
        const height = Math.min(BLOCK, whole.height - dy);
        const at = pen + whole.dx + dx;
        if (at >= to || at + width <= from) {
          this.partial ||= !clip || (at < clip.right && at + width > clip.left);
          continue;
        }
        const index = (dy / BLOCK) * columns + column;
        let block = whole.blocks[index];
        if (block) {
          this.hits++;
        } else {
          const { dx: inkX, dy: inkY } = whole;
          const drawn = this.hasTime
            ? this.#draw(width, height, (u, v) => {
                const context = this.#using(glyphs, set);
                context.save();
                context.beginPath();
                context.rect(u, v, width, height);
                context.clip();
                context.fillText(text, u - dx - inkX + phase, v - dy - inkY);
                context.restore();
              })
            : null;
          if (!drawn) {
            return null;
          }
          block = drawn;
          whole.blocks[index] = block;
          // Not before: what has no picture is not found when a layer is emptied.
          set.texts.set(key, whole);
        }
        this.layers |= 1 << block.layer;
        out.sprite(block, at, baseline + whole.dy + dy, clip, row);
      }
    }
    return true;
  }

  /** Draws into the atlas, if there is room. */
  #draw(width: number, height: number, paint: (x: number, y: number) => void): Sprite | null {
    // What the quads so far were made from must not make room for the next one.
    this.atlas.plan.use(this.layers);
    const place = this.atlas.reserve(width, height);
    if (!place) {
      // Nothing else will fit either.
      this.#closed = true;
      return null;
    }
    const { layer, top } = this.atlas.plan;
    paint(place.x, place.y);
    this.misses++;
    return { layer, u: place.x, v: top + place.y, width, height };
  }

  #using(glyphs: FontGlyphs, set: GlyphSet): CanvasRenderingContext2D {
    const { context } = this.atlas.raster;
    if (glyphs.font !== this.#font) {
      this.#font = glyphs.font;
      context.font = glyphs.font;
    }
    if (set.css !== this.#color) {
      this.#color = set.css;
      context.fillStyle = set.css;
    }
    return context;
  }

  #fontOf(font: string): FontGlyphs {
    let glyphs = this.#fonts.get(font);
    if (!glyphs) {
      // At every size the same.
      const face = font.replace(/[\d.]+(?=px)/, "");
      let check = this.#checks.get(face);
      if (!check) {
        check = this.#check(font);
        this.#checks.set(face, check);
      }
      glyphs = new FontGlyphs(font, check, this.#unitsOf(this.atlas.raster.context));
      this.#fonts.set(font, glyphs);
    }
    return glyphs;
  }

  #evict(layer: number): void {
    for (const [font, glyphs] of this.#fonts) {
      for (const [color, set] of glyphs.sets) {
        if (!set.evict(layer)) {
          glyphs.sets.delete(color);
        }
      }
      if (glyphs.sets.size === 0) {
        this.#fonts.delete(font);
      }
    }
    for (const [key, sprite] of this.#pictures) {
      if (sprite.layer === layer) {
        this.#pictures.delete(key);
      }
    }
  }
}

function sizeOf(measured: TextMetrics): InkSize {
  return {
    advance: measured.width,
    left: measured.actualBoundingBoxLeft,
    right: measured.actualBoundingBoxRight,
    ascent: measured.actualBoundingBoxAscent,
    descent: measured.actualBoundingBoxDescent,
  };
}

/** The pixels a text may have, from its pen on whole pixels, with one more all round. */
function inkOf(size: InkSize, phase: number) {
  if (size.left + size.right <= 0 || size.ascent + size.descent <= 0) {
    return null;
  }
  const left = Math.floor(phase - size.left) - 1;
  const top = Math.floor(-size.ascent) - 1;
  return {
    left,
    top,
    width: Math.ceil(phase + size.right) + 1 - left,
    height: Math.ceil(size.descent) + 1 - top,
  };
}

/** What stands for a text until there is time to draw it. */
function bar(out: InstanceWriter, size: number, color: PackedColor, place: RunPlace): void {
  const { x, baseline, width, clip, row } = place;
  const left = Math.max(Math.round(x), clip?.left ?? Number.NEGATIVE_INFINITY);
  const right = Math.min(Math.round(x + width), clip?.right ?? Number.POSITIVE_INFINITY);
  const top = Math.max(
    baseline - Math.max(1, Math.round(size / 2)),
    clip?.top ?? Number.NEGATIVE_INFINITY,
  );
  const bottom = Math.min(baseline, clip?.bottom ?? Number.POSITIVE_INFINITY);
  out.rect(left, top, right - left, bottom - top, faint(color), row);
}

/** The colour of what stands for text of a colour. */
export function faint(color: PackedColor): PackedColor {
  return ((color & 0xffffff) | ((alphaOf(color) >> 2) << 24)) >>> 0;
}
