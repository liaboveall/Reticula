import { afterEach, expect, test, vi } from "vitest";
import { PageCache } from "./cache.ts";
import { defaultLayout, visibleCells } from "./layout.ts";
import { type Frame, Renderer } from "./renderer.ts";
import { readTheme } from "./theme.ts";

function setup() {
  vi.stubGlobal("getComputedStyle", () => ({ getPropertyValue: () => "" }));
  const theme = readTheme({} as HTMLElement);
  const context = {
    fillStyle: "",
    strokeStyle: "",
    beginPath: vi.fn(),
    clip: vi.fn(),
    closePath: vi.fn(),
    fill: vi.fn(),
    fillRect: vi.fn(function (this: { fillStyle: string }) {
      fills.push(this.fillStyle);
    }),
    fillText: vi.fn(),
    lineTo: vi.fn(),
    measureText: (text: string) => ({ width: text.length * 7 }),
    moveTo: vi.fn(),
    rect: vi.fn(),
    restore: vi.fn(),
    roundRect:
      vi.fn<(left: number, top: number, width: number, height: number, radius: number) => void>(),
    save: vi.fn(),
    setLineDash: vi.fn(),
    setTransform: vi.fn(),
    stroke: vi.fn(),
    strokeRect: vi.fn(),
  };
  const fills: string[] = [];
  const renderer = new Renderer({ getContext: () => context } as unknown as HTMLCanvasElement);
  const metrics = {
    rowHeight: 20,
    colWidth: 75,
    headerHeight: 30,
    headerWidth: 45,
    scrollbarSize: 10,
  };
  const size = { width: 350, height: 190 };
  const sheet = { rows: 40, cols: 40 };
  const layout = { ...defaultLayout(metrics), frozenRows: 1, frozenCols: 1 };
  const scroll = { top: 10, left: 5 };
  const frame: Frame = {
    size,
    sheet,
    layout,
    metrics,
    scroll,
    theme,
    scale: 1,
    visible: visibleCells(metrics, sheet, scroll, size, layout),
    cache: new PageCache(),
    selection: null,
  };
  return { context, fills, frame, renderer, theme };
}

afterEach(() => vi.unstubAllGlobals());

test("draws a cross-pane remote selection and only one label for its visible cursor", () => {
  const { context, fills, frame, renderer, theme } = setup();

  renderer.draw({
    ...frame,
    remotePresences: [
      {
        replica: 7,
        displayName: "Ada",
        colorSlot: 3,
        cursorRow: 0,
        cursorCol: 0,
        selection: { firstRow: 0, endRow: 3, firstCol: 0, endCol: 3 },
      },
    ],
  });

  expect(fills.filter((fill) => fill === theme.refFills[3])).toHaveLength(4);
  expect(context.roundRect).toHaveBeenCalledExactlyOnceWith(46, 31, 33, 18, 5);
  expect(context.fillText).toHaveBeenCalledWith("Ada", 52, 40, 21);
});

test("keeps a visible selection when its cursor has scrolled out of view", () => {
  const { context, fills, frame, renderer, theme } = setup();

  renderer.draw({
    ...frame,
    remotePresences: [
      {
        replica: 7,
        displayName: "Ada",
        colorSlot: 3,
        cursorRow: 30,
        cursorCol: 30,
        selection: { firstRow: 1, endRow: 31, firstCol: 1, endCol: 31 },
      },
    ],
  });

  expect(fills.filter((fill) => fill === theme.refFills[3])).toHaveLength(1);
  expect(context.roundRect).not.toHaveBeenCalled();
});

test("renders AutoFilter row numbers, gap dividers, and header chevrons", async () => {
  const { FilterRowMap } = await import("./filter.ts");
  const { context, fills, frame, renderer, theme } = setup();

  const fillTexts: { text: string; fillStyle: string | CanvasGradient | CanvasPattern }[] = [];
  const origFillText = context.fillText.bind(context);
  context.fillText = ((text: string, x: number, y: number, maxWidth?: number) => {
    fillTexts.push({ text, fillStyle: context.fillStyle });
    return origFillText(text, x, y, maxWidth);
  }) as typeof context.fillText;

  const ctxState = context as unknown as { lineWidth?: number | undefined };
  const pathFills: (string | CanvasGradient | CanvasPattern)[] = [];
  const origFill = context.fill.bind(context);
  context.fill = ((...args: Parameters<CanvasRenderingContext2D["fill"]>) => {
    pathFills.push(context.fillStyle);
    return origFill(...args);
  }) as typeof context.fill;

  const strokes: {
    strokeStyle: string | CanvasGradient | CanvasPattern;
    lineWidth: number | undefined;
  }[] = [];
  const origStroke = context.stroke.bind(context);
  context.stroke = (() => {
    strokes.push({ strokeStyle: context.strokeStyle, lineWidth: ctxState.lineWidth });
    return origStroke();
  }) as typeof context.stroke;

  renderer.draw({
    ...frame,
    filterState: {
      headerRow: 0,
      lastRow: 10,
      firstCol: 0,
      lastCol: 2,
      activeCols: [1],
      hiddenRowSpans: [[1, 3]],
    },
    filterRowMap: new FilterRowMap(20, [[1, 3]]),
  });

  const rowLabels = fillTexts.map((entry) => entry.text);
  expect(rowLabels).toContain("1");
  expect(rowLabels).toContain("5");
  expect(rowLabels).toContain("6");
  expect(rowLabels).not.toContain("2");
  expect(rowLabels).not.toContain("3");
  expect(rowLabels).not.toContain("4");
  expect(
    fillTexts.some((entry) => entry.text === "5" && entry.fillStyle === theme.filterHeaderText),
  ).toBe(true);
  expect(
    strokes.some((entry) => entry.strokeStyle === theme.filterHeaderText && entry.lineWidth === 2),
  ).toBe(true);
  expect(pathFills).toContain(theme.filterIconActiveColor);
  expect(pathFills).toContain(theme.filterIconColor);
  expect(fills).toContain(theme.selectionFill);
});

test("renders dashed border outline around the active spill range", () => {
  const { context, frame, renderer, theme } = setup();

  const dashes: number[][] = [];
  const origSetLineDash = context.setLineDash.bind(context);
  context.setLineDash = ((segments: number[]) => {
    dashes.push([...segments]);
    return origSetLineDash(segments);
  }) as typeof context.setLineDash;

  const ctxState = context as unknown as { lineWidth?: number | undefined };
  const strokeRects: {
    strokeStyle: string | CanvasGradient | CanvasPattern;
    lineWidth: number | undefined;
  }[] = [];
  const origStrokeRect = context.strokeRect.bind(context);
  context.strokeRect = ((x: number, y: number, w: number, h: number) => {
    strokeRects.push({ strokeStyle: context.strokeStyle, lineWidth: ctxState.lineWidth });
    return origStrokeRect(x, y, w, h);
  }) as typeof context.strokeRect;

  renderer.draw({
    ...frame,
    activeSpillRange: { firstRow: 1, lastRow: 3, firstCol: 1, lastCol: 2 },
  });

  expect(dashes).toContainEqual([4, 3]);
  expect(
    strokeRects.some(
      (entry) => entry.strokeStyle === theme.spillBorderColor && entry.lineWidth === 1.5,
    ),
  ).toBe(true);
});
