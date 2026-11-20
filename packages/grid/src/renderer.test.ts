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
