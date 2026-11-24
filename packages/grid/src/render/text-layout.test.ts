import { FLAG_BOOL, FLAG_ERROR, FLAG_NUMBER } from "@reticula/protocol/viewport";
import { describe, expect, test } from "vitest";
import { SparseAxis } from "../layout.ts";
import {
  advanceOf,
  axes,
  cacheOf,
  colors,
  frameOf,
  measure,
  PLAIN,
  pageOf,
  sceneOf,
  type TestCells,
  theme,
} from "./fixtures.ts";
import type { GridCellStyle } from "./frame.ts";
import { makeLook, StyleTable } from "./look.ts";
import { PageLayout, type PageSetting } from "./page-layout.ts";
import { bandsOf, type Scene } from "./scene.ts";
import { TextRuns } from "./text-layout.ts";

function settingOf(scale = 1, styles: readonly GridCellStyle[] = [], frozenCols = 0): PageSetting {
  const look = makeLook(theme, scale, measure, colors);
  const table = new StyleTable(colors);
  table.use(styles, theme);
  return { measure, look, styles: table, frozenCols };
}

interface Run {
  readonly text: string;
  readonly x: number;
  readonly width: number;
  readonly baseline: number;
  readonly font: number;
  readonly color: number;
  readonly row: number;
  readonly clip: readonly number[] | null;
}

/** The runs of a page, in cells of 20 by 75 CSS pixels unless told otherwise. */
function runsOf(
  cells: TestCells,
  {
    scale = 1,
    styles = [],
    frozenCols = 0,
    layout = axes(),
    pageCol = 0,
  }: {
    scale?: number;
    styles?: readonly GridCellStyle[];
    frozenCols?: number;
    layout?: ReturnType<typeof axes>;
    pageCol?: number;
  } = {},
): Run[] {
  const setting = settingOf(scale, styles, frozenCols);
  const page = new PageLayout(pageOf(cells, 0, pageCol), layout, setting);
  const runs: Run[] = [];
  for (let band = 0; band < page.bandCount; band++) {
    const { texts } = page;
    const { first, end } = page.band(band);
    for (let run = first; run < end; run++) {
      runs.push({
        text: texts.texts[run] ?? "",
        x: texts.x[run] ?? 0,
        width: texts.width[run] ?? 0,
        baseline: texts.baseline[run] ?? 0,
        font: texts.font[run] ?? 0,
        color: texts.color[run] ?? 0,
        row: texts.row[run] ?? 0,
        clip: texts.clipped[run] ? [...texts.clip.subarray(run * 4, run * 4 + 4)] : null,
      });
    }
  }
  return runs.sort((a, b) => a.row - b.row || a.x - b.x);
}

const LONG = "a text that is long enough to run on over several columns";

describe("at a zoom", () => {
  const looks = [0.25, 0.5, 1, 1.5, 2].map((zoom) => makeLook(theme, 1.5, measure, colors, zoom));

  test("text keeps the same part of the distance from its cell's edges", () => {
    expect(looks.map((look) => look.padding)).toEqual([2, 5, 9, 14, 18]);
    for (const look of looks) {
      const styles = new StyleTable(colors);
      styles.use([PLAIN], theme);
      const { zoom } = look;
      const layout = {
        ...axes(),
        rowAxis: new SparseAxis(20, [], 1.5, zoom),
        colAxis: new SparseAxis(75, [], 1.5, zoom),
      };
      const cells = { "0:0": "x", "0:1": { text: "1", flags: FLAG_NUMBER } };
      const page = new PageLayout(pageOf(cells, 0, 0), layout, {
        measure,
        look,
        styles,
        frozenCols: 0,
      });
      const { texts } = page;
      expect(page.band(0).end).toBe(2);
      const edge = 2 * Math.round(75 * zoom * 1.5);
      expect(texts.x[0]).toBe(look.padding);
      expect(edge - (texts.x[1] ?? 0) - (texts.width[1] ?? 0)).toBeCloseTo(look.padding, 9);
    }
  });

  test("the line through a text is as heavy as the text, and no lighter than a pixel", () => {
    const heights = looks.map((look) => {
      const styles = new StyleTable(colors);
      styles.use([PLAIN, { ...PLAIN, strikethrough: true }], theme);
      const layout = {
        ...axes(),
        rowAxis: new SparseAxis(20, [], 1.5, look.zoom),
        colAxis: new SparseAxis(75, [], 1.5, look.zoom),
      };
      const page = new PageLayout(pageOf({ "0:0": { text: "gone", style: 1 } }, 0, 0), layout, {
        measure,
        look,
        styles,
        frozenCols: 0,
      });
      page.band(0);
      return page.texts.strikes.geometry[3];
    });
    expect(heights).toEqual([1, 1, 2, 2, 3]);
  });
});

describe("the place of a text", () => {
  test("is left, right or in the middle of its cell, as its kind or its style has it", () => {
    const styles = [
      PLAIN,
      { ...PLAIN, align: "center" as const },
      { ...PLAIN, align: "left" as const },
    ];
    const runs = runsOf(
      {
        "0:0": "text",
        "1:0": { text: "12", flags: FLAG_NUMBER },
        "2:0": { text: "TRUE", flags: FLAG_BOOL },
        "3:0": { text: "#REF!", flags: FLAG_ERROR },
        "4:0": { text: "mid", style: 1 },
        "5:0": { text: "34", flags: FLAG_NUMBER, style: 2 },
      },
      { styles },
    );
    const places = runs.map((run) => [run.x, 75 - run.x - run.width]);
    expect(places[0]?.[0]).toBe(6);
    expect(places[1]?.[1]).toBeCloseTo(6, 9);
    for (const place of [2, 3, 4]) {
      expect(places[place]?.[0]).toBeCloseTo(places[place]?.[1] ?? 0, 9);
    }
    expect(places[5]?.[0]).toBe(6);
    expect(runs[3]?.color).toBe(colors.parse(theme.errorText));
    expect(runs.every((run) => run.clip === null)).toBe(true);
  });

  test("has a baseline on a whole device pixel and a pen that is anywhere", () => {
    for (const scale of [1, 1.1, 1.25, 1.5, 2]) {
      const cells: Record<string, { text: string; flags: number }> = {};
      for (let row = 0; row < 30; row++) {
        cells[`${row}:3`] = { text: `${row * 37.7}`, flags: FLAG_NUMBER };
      }
      const runs = runsOf(cells, { scale });
      expect(runs).toHaveLength(30);
      expect(runs.every((run) => Number.isInteger(run.baseline))).toBe(true);
      expect(runs.some((run) => !Number.isInteger(run.x))).toBe(true);
    }
  });

  test("puts the last digits of numbers on the same part of a pixel in every row", () => {
    for (const scale of [1, 1.1, 1.25, 1.5, 2]) {
      const cells: Record<string, { text: string; flags: number }> = {};
      for (let row = 0; row < 40; row++) {
        cells[`${row}:2`] = { text: `${row * row * 13}.${row % 10}5`, flags: FLAG_NUMBER };
      }
      const { look } = settingOf(scale);
      const last = advanceOf(look.fonts[0] ?? "", "5");
      const pens = runsOf(cells, { scale }).map((run) => run.x + run.width - last);
      for (const pen of pens) {
        expect(pen).toBeCloseTo(pens[0] ?? 0, 9);
      }
    }
  });

  test("is measured in the font it is drawn in", () => {
    const styles = [PLAIN, { ...PLAIN, bold: true, italic: true }];
    const cells = { "0:0": { text: "bold", style: 1 }, "1:0": "bold" };
    const [bold, plain] = runsOf(cells, { styles });
    expect(bold?.font).toBe(3);
    expect(bold?.width).toBeGreaterThan(plain?.width ?? 0);
  });
});

describe("text wider than its cell", () => {
  test("is cut on both sides in the middle and on the left at the right", () => {
    const styles = [PLAIN, { ...PLAIN, align: "center" as const }];
    const [centred, number] = runsOf(
      {
        "0:1": { text: LONG, style: 1 },
        "1:1": { text: "123456789012345678", flags: FLAG_NUMBER },
      },
      { styles },
    );
    expect(centred?.clip).toEqual([81, 0, 144, 20]);
    expect(centred?.x).toBeLessThan(81);
    expect(number?.clip).toEqual([81, 20, 144, 40]);
    expect((number?.x ?? 0) + (number?.width ?? 0)).toBeCloseTo(144, 9);
  });

  test("runs on over empty cells, up to one with text of its own", () => {
    const [run] = runsOf({ "0:1": LONG, "0:4": { text: "1", flags: FLAG_NUMBER } });
    expect(run?.clip).toEqual([75, 0, 4 * 75 - 6, 20]);
  });

  test("needs no clip when the empty cells have room for it", () => {
    const text = "just over one cell";
    const [run] = runsOf({ "0:0": text });
    expect(run?.width).toBeGreaterThan(75);
    expect(run?.clip).toBeNull();
  });

  test("does not run on from a hidden column, but does from a narrow one", () => {
    const layout = axes(
      [],
      [
        [1, 0],
        [3, 8],
      ],
    );
    const cells = { "0:1": LONG, "1:3": LONG };
    const runs = runsOf(cells, { layout });
    expect(runs.map((run) => run.row)).toEqual([1]);
    expect(
      runsOf({ "0:15": LONG, "1:15": LONG }, { pageCol: 1, layout: axes([], [[15, 0]]) }),
    ).toEqual([]);
  });

  test("is cut to its row when that is lower than a line of text", () => {
    const layout = axes([[1, 12]]);
    const [, low, high] = runsOf({ "0:0": "Ag", "1:0": "Ag", "2:0": "Ag" }, { layout });
    expect(low?.clip).toEqual([0, 20, 69, 32]);
    expect(high?.clip).toBeNull();
  });
});

describe("a line through a text", () => {
  test("is as long as what is shown of it, on whole pixels in its colour", () => {
    const styles = [PLAIN, { ...PLAIN, strikethrough: true }];
    const setting = settingOf(1.5, styles);
    const cells = { "0:0": { text: "gone", style: 1 }, "1:14": { text: LONG, style: 1 } };
    const strikesOf = (pageCol: number) => {
      const layout = new PageLayout(pageOf(cells, 0, pageCol), axes(), setting);
      expect(layout.band(1)).toMatchObject({ firstStrike: 0, endStrike: 0 });
      layout.band(0);
      return layout.texts.strikes;
    };
    const strikes = strikesOf(0);
    expect(strikes.length).toBe(2);
    const [left, top, width, height] = strikes.geometry;
    expect([left, top, height]).toEqual([9, 15, 2]);
    expect(width).toBe(Math.round(9 + measure.width(setting.look.fonts[0] ?? "", "gone")) - 9);
    // The line ends with the page; the next page goes on with both.
    const end = (strikes.geometry[4] ?? 0) + (strikes.geometry[6] ?? 0);
    const pageEnd = Math.round(16 * 75 * 1.5);
    expect(end).toBe(pageEnd);
    const next = strikesOf(1);
    expect(next.length).toBe(1);
    expect(next.geometry[0]).toBe(0);
  });
});

/** How many times each device pixel column of a row's text is drawn, across the screen. */
function columnsOf(scene: Scene, text: string): Uint8Array {
  const columns = new Uint8Array(scene.width);
  for (const step of scene.steps) {
    if (step.kind !== "texts") {
      continue;
    }
    const bands = bandsOf(step);
    for (let band = bands.first; band < bands.end; band++) {
      const { first, end } = step.layout.band(band);
      const runs = step.layout.texts;
      for (let run = first; run < end; run++) {
        const row = runs.row[run] ?? 0;
        if (runs.texts[run] !== text || row < step.firstRow || row >= step.endRow) {
          continue;
        }
        const x = step.x + (runs.x[run] ?? 0);
        const own = runs.clipped[run] === 1 ? runs.clip.subarray(run * 4) : null;
        const left = Math.max(Math.floor(x), step.clip.left, own ? step.x + (own[0] ?? 0) : 0);
        const right = Math.min(
          Math.ceil(x + (runs.width[run] ?? 0)),
          step.clip.right,
          own ? step.x + (own[2] ?? 0) : scene.width,
        );
        for (let column = left; column < right; column++) {
          columns[column] = (columns[column] ?? 0) + 1;
        }
      }
    }
  }
  return columns;
}

describe("text that runs from page to page", () => {
  const font = `13px sans-serif`;
  const width = measure.width(font, LONG);
  const WIDE = { width: 1400, height: 190 };
  const at = (left: number, top = 0) => ({ left, top });

  /** Where a text of column `col` is on the screen. */
  const shown = (col: number, scroll: number, from: number, to: number) => {
    const start = 45 + col * 75 + 6 - scroll;
    return {
      left: Math.max(from, Math.floor(start)),
      right: Math.min(to, Math.ceil(start + width)),
    };
  };
  const expectOnce = (columns: Uint8Array, left: number, right: number) => {
    expect(right).toBeGreaterThan(left);
    columns.forEach((count, column) => {
      expect(count, `pixel column ${column}`).toBe(column >= left && column < right ? 1 : 0);
    });
  };

  test("is wider than a page has left, or these tests show nothing", () => {
    expect(width).toBeGreaterThan(4 * 75);
  });

  test("is drawn once where its cell is in view", () => {
    const cache = cacheOf({ "3:14": LONG }, 1, 2);
    const scene = sceneOf(frameOf({ size: WIDE, cache }));
    const { left, right } = shown(14, 0, 45, 1390);
    expectOnce(columnsOf(scene, LONG), left, right);
  });

  test("is drawn once where its cell is out of view in a page that is in view", () => {
    const cache = cacheOf({ "3:2": LONG }, 1, 2);
    const scroll = at(3 * 75 + 10);
    const scene = sceneOf(frameOf({ cache, scroll }));
    const { left, right } = shown(2, scroll.left, 45, 340);
    expectOnce(columnsOf(scene, LONG), left, right);
  });

  test("is drawn once where its cell and its page are out of view", () => {
    const cache = cacheOf({ "3:14": LONG }, 1, 2);
    const scroll = at(17 * 75);
    const scene = sceneOf(frameOf({ cache, scroll }));
    const { left, right } = shown(14, scroll.left, 45, 340);
    expectOnce(columnsOf(scene, LONG), left, right);
  });

  test("is drawn once across two whole pages", () => {
    const text = LONG.repeat(5);
    const cache = cacheOf({ "3:14": text }, 1, 3);
    const length = measure.width(font, text);
    expect(length).toBeGreaterThan(18 * 75);
    for (const scroll of [12 * 75, 20 * 75 + 33, 30 * 75]) {
      const scene = sceneOf(frameOf({ size: WIDE, cache, scroll: at(scroll) }));
      const start = 45 + 14 * 75 + 6 - scroll;
      const right = Math.min(1390, Math.ceil(start + length));
      expectOnce(columnsOf(scene, text), Math.max(45, Math.floor(start)), right);
    }
  });

  test("stops at the same cell whichever page it is drawn by", () => {
    const cache = cacheOf({ "3:14": LONG, "3:17": { text: "7", flags: FLAG_NUMBER } }, 1, 2);
    const scene = sceneOf(frameOf({ size: WIDE, cache }));
    expectOnce(columnsOf(scene, LONG), 45 + 14 * 75 + 6, 45 + 17 * 75 - 6);
  });

  test("is cut at the edge of a page that has not arrived, and goes on when it has", () => {
    const cells = { "3:14": LONG };
    const cut = sceneOf(frameOf({ size: WIDE, cache: cacheOf(cells, 1, 2, ["0:1"]) }));
    expectOnce(columnsOf(cut, LONG), 45 + 14 * 75 + 6, 45 + 16 * 75);
    expect(cut.missing).toBeGreaterThan(0);
    const whole = sceneOf(frameOf({ size: WIDE, cache: cacheOf(cells, 1, 2) }));
    const { left, right } = shown(14, 0, 45, 1390);
    expectOnce(columnsOf(whole, LONG), left, right);
  });

  test("is drawn by the page it runs into when the page it is from has not arrived", () => {
    const cache = cacheOf({ "3:14": LONG }, 1, 2, ["0:0"]);
    const scene = sceneOf(frameOf({ size: WIDE, cache }));
    const { right } = shown(14, 0, 45, 1390);
    expectOnce(columnsOf(scene, LONG), 45 + 16 * 75, right);
  });

  test("of a frozen column ends at the freeze line, wherever the rest is scrolled to", () => {
    const cache = cacheOf({ "3:1": LONG }, 1, 2);
    const layout = axes([], [], 0, 2);
    for (const scroll of [0, 40, 75 * 3, 75 * 17]) {
      const scene = sceneOf(frameOf({ size: WIDE, cache, layout, scroll: at(scroll) }));
      expectOnce(columnsOf(scene, LONG), 45 + 75 + 6, 45 + 2 * 75);
    }
  });

  test("of a frozen column is drawn once when the freeze line is in the next page", () => {
    const cache = cacheOf({ "3:14": LONG }, 1, 2);
    const layout = axes([], [], 0, 17);
    const scene = sceneOf(frameOf({ size: { width: 1600, height: 190 }, cache, layout }));
    expectOnce(columnsOf(scene, LONG), 45 + 14 * 75 + 6, 45 + 17 * 75);
  });

  test("of the first column that scrolls is drawn once beside the freeze line", () => {
    const cache = cacheOf({ "3:2": LONG }, 1, 2);
    const layout = axes([], [], 0, 2);
    for (const scroll of [0, 100]) {
      const scene = sceneOf(frameOf({ size: WIDE, cache, layout, scroll: at(scroll) }));
      const { left, right } = shown(2, scroll, 45 + 150, 1390);
      expectOnce(columnsOf(scene, LONG), left, right);
    }
  });

  test("is drawn by the part of the sheet its row is in", () => {
    const cache = cacheOf({ "3:2": LONG }, 1, 2);
    const layout = axes([], [], 2, 0);
    const scene = sceneOf(frameOf({ cache, layout, scroll: at(0, 30) }));
    const step = scene.steps.find(
      (candidate) => candidate.kind === "texts" && candidate.clip.top === 70,
    );
    expect(step).toMatchObject({ firstRow: 3 });
    const { left, right } = shown(2, 0, 45, 340);
    expectOnce(columnsOf(scene, LONG), left, right);
  });
});

describe("a list of runs", () => {
  const listed = (count: number, fontOf: (index: number) => number) => {
    const runs = new TextRuns(2);
    for (let index = 0; index < count; index++) {
      const run = runs.push(`t${index}`, index + 0.25, 10, 100 + index, fontOf(index), 7, index);
      if (index === 4) {
        runs.setClip(run, 1, 2, 3, 4);
      }
    }
    return runs;
  };

  test("puts those of one font and colour together, keeping everything else", () => {
    const runs = listed(9, (index) => index % 2);
    runs.batch(1, 9);
    expect(runs.texts).toEqual(["t0", "t1", "t3", "t5", "t7", "t2", "t4", "t6", "t8"]);
    expect([...runs.font.subarray(0, 9)]).toEqual([0, 1, 1, 1, 1, 0, 0, 0, 0]);
    expect([...runs.x.subarray(0, 9)]).toEqual([
      0.25, 1.25, 3.25, 5.25, 7.25, 2.25, 4.25, 6.25, 8.25,
    ]);
    expect([...runs.row.subarray(0, 9)]).toEqual([0, 1, 3, 5, 7, 2, 4, 6, 8]);
    expect([...runs.clipped.subarray(0, 9)]).toEqual([0, 0, 0, 0, 0, 0, 1, 0, 0]);
    expect([...runs.clip.subarray(24, 28)]).toEqual([1, 2, 3, 4]);
  });

  test("keeps what it has when it grows", () => {
    const runs = listed(40, () => 1);
    expect(runs.length).toBe(40);
    expect(runs.texts).toHaveLength(40);
    expect([...runs.x.subarray(0, 40)]).toEqual(Array.from({ length: 40 }, (_, at) => at + 0.25));
    expect([...runs.clip.subarray(16, 20)]).toEqual([1, 2, 3, 4]);
    runs.batch(0, 40);
    expect(runs.texts[39]).toBe("t39");
  });
});

test("the bands of a page are the same in whatever order they are laid out", () => {
  const styles = [PLAIN, { ...PLAIN, bold: true, strikethrough: true }];
  const cells: Record<string, { text: string; style: number }> = {
    "9:14": { text: LONG, style: 1 },
  };
  for (let row = 0; row < 64; row++) {
    for (let col = 0; col < 12; col++) {
      if ((row * 7 + col * 3) % 5 !== 0) {
        cells[`${row}:${col + 17}`] = { text: `${row}:${col}`, style: (row + col) % 2 };
      }
    }
  }
  const layoutIn = (order: readonly number[]) => {
    const layout = new PageLayout(pageOf(cells, 0, 1), axes(), settingOf(1, styles));
    const bands = new Map(order.map((index) => [index, layout.band(index)]));
    return [...bands]
      .sort(([a], [b]) => a - b)
      .map(([, { first, end, firstStrike, endStrike }]) => ({
        texts: layout.texts.texts.slice(first, end),
        x: [...layout.texts.x.subarray(first, end)],
        rows: [...layout.texts.row.subarray(first, end)],
        strikes: [...layout.texts.strikes.geometry.subarray(firstStrike * 4, endStrike * 4)],
      }));
  };
  const down = layoutIn([0, 1, 2, 3, 4, 5, 6, 7]);
  expect(layoutIn([5, 2, 7, 0, 1, 6, 4, 3])).toEqual(down);
  expect(down[1]?.texts).toContain(LONG);
  expect(down.flatMap((band) => band.texts).length).toBe(Object.keys(cells).length);
});

test("sizes that are not whole pixels leave every edge on one", () => {
  const scale = 1.1;
  const layout = { ...axes(), rowAxis: new SparseAxis(24), colAxis: new SparseAxis(111) };
  const page = new PageLayout(pageOf({ "0:0": "x" }, 0, 1), layout, settingOf(scale));
  const { xs, ys } = page.geometry;
  expect(xs[0]).toBe(0);
  for (let col = 0; col <= 16; col++) {
    expect(xs[col]).toBe(Math.round((16 + col) * 111 * scale) - Math.round(16 * 111 * scale));
  }
  expect(ys[64]).toBe(Math.round(64 * 24 * scale));
});
