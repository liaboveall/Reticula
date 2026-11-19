/** What the canvas needs to know about how the grid looks. */
export interface Theme {
  readonly dark: boolean;
  readonly background: string;
  readonly text: string;
  readonly errorText: string;
  readonly line: string;
  readonly freezeLine: string;
  /** Fill of cells whose data has not arrived yet. */
  readonly placeholder: string;
  readonly headerBackground: string;
  readonly headerText: string;
  readonly headerLine: string;
  /** Header of the row and of the column of the selected cell or range. */
  readonly headerSelected: string;
  readonly selection: string;
  readonly selectionFill: string;
  /** Semantic 8-slot text colors (`Palette(0..7)`). */
  readonly paletteText: readonly string[];
  /** Semantic 8-slot background fill colors (`Palette(0..7)`). */
  readonly paletteFill: readonly string[];
  /** Formula reference highlight stroke colors (`0..7`). */
  readonly refStrokes: readonly string[];
  /** Formula reference highlight translucent fills (`0..7`). */
  readonly refFills: readonly string[];
  /** The CSS `font` shorthand that text is drawn with. */
  readonly font: string;
  readonly fontFamily: string;
  readonly fontSize: number;
}

export const LIGHT_PALETTE_TEXT = [
  "#334155",
  "#dc2626",
  "#c2410c",
  "#15803d",
  "#1d4ed8",
  "#6d28d9",
  "#be185d",
  "#0f766e",
] as const;

export const DARK_PALETTE_TEXT = [
  "#cbd5e1",
  "#f87171",
  "#fb923c",
  "#4ade80",
  "#60a5fa",
  "#c084fc",
  "#f472b6",
  "#2dd4bf",
] as const;

export const LIGHT_PALETTE_FILL = [
  "#f1f5f9",
  "#fee2e2",
  "#ffedd5",
  "#dcfce7",
  "#dbeafe",
  "#ede9fe",
  "#fce7f3",
  "#ccfbf1",
] as const;

export const DARK_PALETTE_FILL = [
  "#252b36",
  "#3f1d22",
  "#3f2817",
  "#173625",
  "#1a2d4c",
  "#2b2147",
  "#3e1c33",
  "#153533",
] as const;

export const REF_COLORS_LIGHT = [
  "#2563eb",
  "#dc2626",
  "#7c3aed",
  "#059669",
  "#d97706",
  "#db2777",
  "#0891b2",
  "#4f46e5",
] as const;

export const REF_COLORS_DARK = [
  "#60a5fa",
  "#f87171",
  "#a78bfa",
  "#34d399",
  "#fbbf24",
  "#f472b6",
  "#22d3ee",
  "#818cf8",
] as const;

const REF_FILLS_LIGHT = [
  "rgba(37, 99, 235, 0.10)",
  "rgba(220, 38, 38, 0.10)",
  "rgba(124, 58, 237, 0.10)",
  "rgba(5, 150, 105, 0.10)",
  "rgba(217, 119, 6, 0.10)",
  "rgba(219, 39, 119, 0.10)",
  "rgba(8, 145, 178, 0.10)",
  "rgba(79, 70, 229, 0.10)",
] as const;

const REF_FILLS_DARK = [
  "rgba(96, 165, 250, 0.16)",
  "rgba(248, 113, 113, 0.16)",
  "rgba(167, 139, 250, 0.16)",
  "rgba(52, 211, 153, 0.16)",
  "rgba(251, 191, 36, 0.16)",
  "rgba(244, 114, 182, 0.16)",
  "rgba(34, 211, 238, 0.16)",
  "rgba(129, 140, 248, 0.16)",
] as const;

const FALLBACK: Theme = {
  dark: false,
  background: "#ffffff",
  text: "#1f2328",
  errorText: "#b42318",
  line: "#e3e5e8",
  freezeLine: "#9ba3ae",
  placeholder: "#f1f2f4",
  headerBackground: "#f6f7f8",
  headerText: "#59636e",
  headerLine: "#d1d5da",
  headerSelected: "#dbe7ff",
  selection: "#1a5fd0",
  selectionFill: "rgba(26, 95, 208, 0.12)",
  paletteText: LIGHT_PALETTE_TEXT,
  paletteFill: LIGHT_PALETTE_FILL,
  refStrokes: REF_COLORS_LIGHT,
  refFills: REF_FILLS_LIGHT,
  font: "13px sans-serif",
  fontFamily: "sans-serif",
  fontSize: 13,
};

/** Reads the theme from the custom properties and the font in effect on the grid's element. */
export function readTheme(host: HTMLElement): Theme {
  const style = getComputedStyle(host);
  const color = (name: string, fallback: string) => style.getPropertyValue(name).trim() || fallback;
  const fontSize = Number.parseFloat(style.fontSize) || FALLBACK.fontSize;
  const fontFamily = style.fontFamily || FALLBACK.fontFamily;
  const dark =
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-color-scheme: dark)").matches;
  return {
    dark,
    background: color("--grid-background", FALLBACK.background),
    text: color("--grid-text", FALLBACK.text),
    errorText: color("--grid-error-text", FALLBACK.errorText),
    line: color("--grid-line", FALLBACK.line),
    freezeLine: color("--grid-freeze-line", dark ? "#545d6b" : FALLBACK.freezeLine),
    placeholder: color("--grid-placeholder", FALLBACK.placeholder),
    headerBackground: color("--grid-header-background", FALLBACK.headerBackground),
    headerText: color("--grid-header-text", FALLBACK.headerText),
    headerLine: color("--grid-header-line", FALLBACK.headerLine),
    headerSelected: color("--grid-header-selected", FALLBACK.headerSelected),
    selection: color("--grid-selection", FALLBACK.selection),
    selectionFill: color("--grid-selection-fill", FALLBACK.selectionFill),
    paletteText: dark ? DARK_PALETTE_TEXT : LIGHT_PALETTE_TEXT,
    paletteFill: dark ? DARK_PALETTE_FILL : LIGHT_PALETTE_FILL,
    refStrokes: dark ? REF_COLORS_DARK : REF_COLORS_LIGHT,
    refFills: dark ? REF_FILLS_DARK : REF_FILLS_LIGHT,
    font: `${fontSize}px ${fontFamily}`,
    fontFamily,
    fontSize,
  };
}

/** Converts a `0x00RRGGBB` integer to `#rrggbb`. */
export function rgbToHex(rgb: number): string {
  return `#${(rgb & 0xffffff).toString(16).padStart(6, "0")}`;
}
