import type { Frame, Page } from "@reticula/grid/internals";
import { type BuiltCase, cacheOf } from "./cases.ts";
import type { Mutation } from "./names.ts";
import type { Style } from "./sheet.ts";

const CLEAR = "rgba(0, 0, 0, 0)";

interface Found {
  readonly page: Page;
  readonly index: number;
}

/** The first cell in view, in reading order, that a test accepts. */
function findCell(built: BuiltCase, accept: (found: Found) => boolean): Found {
  const { visible, selection } = built.frame;
  for (let row = visible.firstRow; row < visible.endRow; row++) {
    for (let col = visible.firstCol; col < visible.endCol; col++) {
      const page = built.frame.cache.pageOf(row, col);
      if (!page || (selection?.row === row && selection.col === col)) {
        continue;
      }
      const found = { page, index: (row - page.firstRow) * page.colCount + col - page.firstCol };
      if (found.index < page.texts.length && accept(found)) {
        return found;
      }
    }
  }
  throw new Error("the case has nothing this mutation can leave out");
}

function styleOf(built: BuiltCase, { page, index }: Found): Style | undefined {
  return built.frame.styles?.[page.styleIds[index] ?? 0];
}

/** The frame with one cell changed and everything else as it was. */
function withCell(
  built: BuiltCase,
  { page, index }: Found,
  change: { readonly text?: string; readonly style?: Style },
): Frame {
  const { frame } = built;
  const styles = [...(frame.styles ?? [])];
  const styleIds = new Uint16Array(page.styleIds);
  if (change.style) {
    styleIds[index] = styles.push(change.style) - 1;
  }
  const texts = page.texts.map((text, at) => (at === index ? (change.text ?? text) : text));
  const pages = built.pages.map((each) => (each === page ? { ...page, styleIds, texts } : each));
  return {
    ...frame,
    styles,
    cache: cacheOf(
      pages,
      frame.visible,
      frame.layout?.frozenRows ?? 0,
      frame.layout?.frozenCols ?? 0,
    ),
  };
}

/**
 * The frame as it would be with nothing struck through: lines through text are
 * not text, so finding the ink of text needs frames without them.
 */
export function withoutStrikes(frame: Frame): Frame {
  const styles = frame.styles?.map((style) => ({ ...style, strikethrough: false })) ?? [];
  return { ...frame, styles };
}

/**
 * The frame without the text of its cells and headers. The labels of filtered
 * rows stay, for the marks of hidden rows have their colour; so do the name
 * tags of collaborators, which are no text runs.
 */
export function withoutText(built: BuiltCase): Frame {
  const { frame } = built;
  const pages = built.pages.map((page) => ({
    ...page,
    texts: page.texts.map(() => ""),
    overflows: [],
  }));
  return {
    ...withoutStrikes(frame),
    theme: { ...frame.theme, headerText: CLEAR },
    cache: cacheOf(
      pages,
      frame.visible,
      frame.layout?.frozenRows ?? 0,
      frame.layout?.frozenCols ?? 0,
    ),
  };
}

/** The frame without the outlines that are drawn with dashes. */
export function withoutDashes(frame: Frame): Frame {
  return { ...frame, clipboardRange: null, activeSpillRange: null, fillPreview: null };
}

/** The frame as a renderer would draw it that leaves one thing out. */
export function mutated(built: BuiltCase, mutation: Exclude<Mutation, "dash">): Frame {
  const { frame } = built;
  switch (mutation) {
    case "grid-lines":
      return { ...frame, theme: { ...frame.theme, line: frame.theme.background } };
    case "selection-fill":
      return { ...frame, theme: { ...frame.theme, selectionFill: CLEAR } };
    case "cell-background": {
      const found = findCell(built, (cell) => styleOf(built, cell)?.fillColor !== undefined);
      const style = styleOf(built, found);
      return withCell(built, found, style ? { style: { ...style, fillColor: undefined } } : {});
    }
    case "border": {
      const found = findCell(built, (cell) => styleOf(built, cell)?.borderTop === "thin");
      const style = styleOf(built, found);
      return withCell(built, found, style ? { style: { ...style, borderTop: "none" } } : {});
    }
    case "text-run":
      return withCell(
        built,
        findCell(built, ({ page, index }) => (page.texts[index] ?? "").length >= 4),
        { text: "" },
      );
  }
}
