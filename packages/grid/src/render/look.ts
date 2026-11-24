import { headerZoom } from "../layout.ts";
import { rgbToHex, type Theme } from "../theme.ts";
import { type Colors, type PackedColor, packColor } from "./color.ts";
import type { GridCellStyle } from "./frame.ts";
import { PADDING } from "./frame.ts";
import { BOLD, fontOf, ITALIC, type TextMeasure } from "./text-measure.ts";

/** No colour of its own; also what a fully transparent colour packs to. */
export const NO_COLOR: PackedColor = 0;

/** Index among the fonts of the one of the headers' labels, after the variants of the sheet's. */
export const HEADER_FONT = 4;

/** A theme at a scale and a zoom: colours packed, sizes in device pixels. */
export interface Look {
  readonly theme: Theme;
  readonly scale: number;
  /** How much the cells and what is in them are enlarged. Lines and name plates are not. */
  readonly zoom: number;
  /** The sheet's font by variant (`BOLD`, `ITALIC`), and the headers'. */
  readonly fonts: readonly string[];
  /** From the middle of a row down to its baseline, by font. */
  readonly lifts: readonly number[];
  /** Height of a line of text, by font. */
  readonly lineHeights: readonly number[];
  readonly plateFont: string;
  /** Space between a cell's edge and its text. */
  readonly padding: number;
  /** Width of a hairline. */
  readonly hairline: number;
  readonly background: PackedColor;
  readonly text: PackedColor;
  readonly errorText: PackedColor;
  readonly line: PackedColor;
  readonly freezeLine: PackedColor;
  readonly placeholder: PackedColor;
  readonly headerBackground: PackedColor;
  readonly headerText: PackedColor;
  readonly headerLine: PackedColor;
  readonly headerSelected: PackedColor;
  readonly selection: PackedColor;
  readonly selectionFill: PackedColor;
  readonly filterHeaderText: PackedColor;
  readonly filterIcon: PackedColor;
  readonly filterIconActive: PackedColor;
  readonly spillBorder: PackedColor;
  readonly refStrokes: readonly PackedColor[];
  readonly refFills: readonly PackedColor[];
}

export function makeLook(
  theme: Theme,
  scale: number,
  measure: TextMeasure,
  colors: Colors,
  zoom = 1,
): Look {
  const variants = [0, BOLD, ITALIC, BOLD | ITALIC];
  const fonts = [
    ...variants.map((variant) => fontOf(theme, variant, scale * zoom)),
    fontOf(theme, 0, scale * headerZoom(zoom)),
  ];
  const lines = fonts.map((font) => measure.line(font));
  const slots = [0, 1, 2, 3, 4, 5, 6, 7];
  const selection = colors.parse(theme.selection);
  const selectionFill = colors.parse(theme.selectionFill);
  return {
    theme,
    scale,
    zoom,
    fonts,
    lifts: lines.map((line) => (line.ascent - line.descent) / 2),
    lineHeights: lines.map((line) => Math.ceil(line.ascent + line.descent)),
    plateFont: `600 ${11 * scale}px ${theme.fontFamily}`,
    padding: Math.round(PADDING * zoom * scale),
    hairline: Math.max(1, Math.round(scale)),
    background: colors.parse(theme.background),
    text: colors.parse(theme.text),
    errorText: colors.parse(theme.errorText),
    line: colors.parse(theme.line),
    freezeLine: colors.parse(theme.freezeLine),
    placeholder: colors.parse(theme.placeholder),
    headerBackground: colors.parse(theme.headerBackground),
    headerText: colors.parse(theme.headerText),
    headerLine: colors.parse(theme.headerLine),
    headerSelected: colors.parse(theme.headerSelected),
    selection,
    selectionFill,
    filterHeaderText: colors.parse(theme.filterHeaderText),
    filterIcon: colors.parse(theme.filterIconColor),
    filterIconActive: colors.parse(theme.filterIconActiveColor),
    spillBorder: colors.parse(theme.spillBorderColor),
    refStrokes: slots.map((slot) => {
      const css = theme.refStrokes[slot];
      return css === undefined ? selection : colors.parse(css);
    }),
    refFills: slots.map((slot) => {
      const css = theme.refFills[slot];
      return css === undefined ? selectionFill : colors.parse(css);
    }),
  };
}

/** Colours that are the same in every theme. */
export const FIXED = {
  ai: packColor(0x7c, 0x3a, 0xed),
  aiFill: packColor(124, 58, 237, Math.round(0.12 * 255)),
  white: packColor(255, 255, 255),
  conflict: packColor(0xd9, 0x77, 0x06),
  conflictFill: packColor(245, 158, 11, Math.round(0.22 * 255)),
  deleted: packColor(0xdc, 0x26, 0x26),
  deletedFill: packColor(239, 68, 68, Math.round(0.12 * 255)),
  added: packColor(0x16, 0xa3, 0x4a),
  addedFill: packColor(34, 197, 94, Math.round(0.1 * 255)),
} as const;

export const ALIGN_AUTO = 0;
export const ALIGN_LEFT = 1;
export const ALIGN_CENTER = 2;
export const ALIGN_RIGHT = 3;

/** Border weights; where two meet, the larger one is drawn. */
export const THIN = 1;
export const MEDIUM = 2;

const ALIGNS = { auto: ALIGN_AUTO, left: ALIGN_LEFT, center: ALIGN_CENTER, right: ALIGN_RIGHT };
const WEIGHTS = { none: 0, thin: THIN, medium: MEDIUM };

type CellColor = GridCellStyle["textColor"];

function sameColor(a: CellColor, b: CellColor): boolean {
  if (!a || !b || a.kind !== b.kind) {
    return !a && !b;
  }
  return a.kind === "rgb" && b.kind === "rgb"
    ? a.rgb === b.rgb
    : a.kind === "palette" && b.kind === "palette" && a.slot === b.slot;
}

function sameStyle(a: GridCellStyle | undefined, b: GridCellStyle | undefined): boolean {
  return (
    a === b ||
    (a !== undefined &&
      b !== undefined &&
      a.bold === b.bold &&
      a.italic === b.italic &&
      a.strikethrough === b.strikethrough &&
      a.align === b.align &&
      sameColor(a.textColor, b.textColor) &&
      sameColor(a.fillColor, b.fillColor) &&
      a.borderTop === b.borderTop &&
      a.borderRight === b.borderRight &&
      a.borderBottom === b.borderBottom &&
      a.borderLeft === b.borderLeft)
  );
}

/** One style of the sheet, ready to lay out and paint with. */
export interface ResolvedStyle {
  /** Variant of the sheet's font. */
  readonly font: number;
  readonly align: number;
  readonly strikethrough: boolean;
  /** `NO_COLOR` for the theme's. */
  readonly text: PackedColor;
  /** `NO_COLOR` for none. */
  readonly fill: PackedColor;
  readonly borderTop: number;
  readonly borderRight: number;
  readonly borderBottom: number;
  readonly borderLeft: number;
}

const DEFAULT_STYLE: ResolvedStyle = {
  font: 0,
  align: ALIGN_AUTO,
  strikethrough: false,
  text: NO_COLOR,
  fill: NO_COLOR,
  borderTop: 0,
  borderRight: 0,
  borderBottom: 0,
  borderLeft: 0,
};

/**
 * The sheet's styles with a theme's colours. A table the engine sent again with
 * more styles at its end keeps its version: what was laid out with it holds.
 */
export class StyleTable {
  version = 0;
  #colors: Colors;
  #theme: Theme | null = null;
  #source: readonly GridCellStyle[] = [];
  #resolved: ResolvedStyle[] = [];

  constructor(colors: Colors) {
    this.#colors = colors;
  }

  get length(): number {
    return this.#resolved.length;
  }

  /** The style with an identifier; the default one when it has not arrived. */
  get(id: number): ResolvedStyle {
    return this.#resolved[id] ?? DEFAULT_STYLE;
  }

  use(styles: readonly GridCellStyle[], theme: Theme): void {
    if (styles === this.#source && theme === this.#theme && styles.length === this.length) {
      return;
    }
    const extended =
      theme === this.#theme &&
      styles.length >= this.length &&
      this.#source.every((style, id) => sameStyle(style, styles[id]));
    if (!extended) {
      this.version++;
      this.#resolved = [];
    }
    for (const style of styles.slice(this.length)) {
      this.#resolved.push(this.#resolve(style, theme));
    }
    this.#source = styles;
    this.#theme = theme;
  }

  #resolve(style: GridCellStyle, theme: Theme): ResolvedStyle {
    return {
      font: (style.bold ? BOLD : 0) | (style.italic ? ITALIC : 0),
      align: ALIGNS[style.align],
      strikethrough: style.strikethrough,
      text: this.#color(style.textColor, theme.paletteText),
      fill: this.#color(style.fillColor, theme.paletteFill),
      borderTop: WEIGHTS[style.borderTop],
      borderRight: WEIGHTS[style.borderRight],
      borderBottom: WEIGHTS[style.borderBottom],
      borderLeft: WEIGHTS[style.borderLeft],
    };
  }

  #color(color: CellColor, palette: readonly string[]): PackedColor {
    const css =
      color?.kind === "rgb"
        ? rgbToHex(color.rgb)
        : color?.kind === "palette"
          ? palette[color.slot & 7]
          : undefined;
    return css === undefined ? NO_COLOR : this.#colors.parse(css);
  }
}
