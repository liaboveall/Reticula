/** What the canvas needs to know about how the grid looks. */
export interface Theme {
  readonly background: string;
  readonly text: string;
  readonly errorText: string;
  readonly line: string;
  /** Fill of cells whose data has not arrived yet. */
  readonly placeholder: string;
  readonly headerBackground: string;
  readonly headerText: string;
  readonly headerLine: string;
  /** Header of the row and of the column of the selected cell or range. */
  readonly headerSelected: string;
  readonly selection: string;
  readonly selectionFill: string;
  /** The CSS `font` shorthand that text is drawn with. */
  readonly font: string;
  readonly fontSize: number;
}

/** Used for any colour the stylesheet does not provide, as when it was not loaded. */
const FALLBACK: Theme = {
  background: "#ffffff",
  text: "#1f2328",
  errorText: "#b42318",
  line: "#e3e5e8",
  placeholder: "#f1f2f4",
  headerBackground: "#f6f7f8",
  headerText: "#59636e",
  headerLine: "#d1d5da",
  headerSelected: "#dbe7ff",
  selection: "#1a5fd0",
  selectionFill: "rgba(26, 95, 208, 0.12)",
  font: "13px sans-serif",
  fontSize: 13,
};

/** Reads the theme from the custom properties and the font in effect on the grid's element. */
export function readTheme(host: HTMLElement): Theme {
  const style = getComputedStyle(host);
  const color = (name: string, fallback: string) => style.getPropertyValue(name).trim() || fallback;
  const fontSize = Number.parseFloat(style.fontSize) || FALLBACK.fontSize;
  return {
    background: color("--grid-background", FALLBACK.background),
    text: color("--grid-text", FALLBACK.text),
    errorText: color("--grid-error-text", FALLBACK.errorText),
    line: color("--grid-line", FALLBACK.line),
    placeholder: color("--grid-placeholder", FALLBACK.placeholder),
    headerBackground: color("--grid-header-background", FALLBACK.headerBackground),
    headerText: color("--grid-header-text", FALLBACK.headerText),
    headerLine: color("--grid-header-line", FALLBACK.headerLine),
    headerSelected: color("--grid-header-selected", FALLBACK.headerSelected),
    selection: color("--grid-selection", FALLBACK.selection),
    selectionFill: color("--grid-selection-fill", FALLBACK.selectionFill),
    font: style.fontFamily ? `${fontSize}px ${style.fontFamily}` : FALLBACK.font,
    fontSize,
  };
}
