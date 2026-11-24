import { describe, expect, test } from "vitest";
import { axes, colors, measure, PLAIN, pageOf, type TestCells, theme } from "./fixtures.ts";
import type { GridCellStyle } from "./frame.ts";
import { makeLook, StyleTable } from "./look.ts";
import { PageLayout, PageLayouts, type PageSetting } from "./page-layout.ts";
import type { RectList } from "./rects.ts";

function settingOf(styles: readonly GridCellStyle[], scale = 1, frozenCols = 0): PageSetting {
  const table = new StyleTable(colors);
  table.use(styles, theme);
  return { measure, look: makeLook(theme, scale, measure, colors), styles: table, frozenCols };
}

const fill = (slot: number): GridCellStyle => ({ ...PLAIN, fillColor: { kind: "palette", slot } });

/** How often each pixel is painted, and with what colour last. */
function painted(rects: RectList): Map<string, { count: number; color: number }> {
  const pixels = new Map<string, { count: number; color: number }>();
  for (let index = 0; index < rects.length; index++) {
    const [left = 0, top = 0, width = 0, height = 0] = rects.geometry.subarray(index * 4);
    for (let y = top; y < top + height; y++) {
      for (let x = left; x < left + width; x++) {
        const count = (pixels.get(`${x},${y}`)?.count ?? 0) + 1;
        pixels.set(`${x},${y}`, { count, color: rects.colors[index] ?? 0 });
      }
    }
  }
  return pixels;
}

describe("the fills of a page", () => {
  test("cover each cell with its colour once, in as few rectangles as it takes", () => {
    const cells: Record<string, { text: string; style: number }> = {};
    for (let row = 0; row < 10; row++) {
      for (let col = 0; col < 16; col++) {
        cells[`${row}:${col}`] = { text: "", style: row === 0 ? 1 : 0 };
      }
    }
    for (let row = 4; row < 8; row++) {
      cells[`${row}:3`] = { text: "", style: 2 };
      cells[`${row}:4`] = { text: "", style: 2 };
    }
    const setting = settingOf([PLAIN, fill(4), fill(1)]);
    const { fills } = new PageLayout(pageOf(cells, 0, 0), axes(), setting);
    expect(fills.length).toBe(2);
    expect([...fills.geometry.subarray(0, 8)]).toEqual([0, 0, 1200, 20, 225, 80, 150, 80]);
    expect([...fills.colors.subarray(0, 2)]).toEqual([
      colors.parse(theme.paletteFill[4] ?? ""),
      colors.parse(theme.paletteFill[1] ?? ""),
    ]);
  });

  test("are not merged across a change of colour or width", () => {
    const cells: TestCells = {
      "0:0": { text: "", style: 1 },
      "1:0": { text: "", style: 2 },
      "2:0": { text: "", style: 1 },
      "2:1": { text: "", style: 1 },
      "3:0": { text: "", style: 1 },
    };
    const { fills } = new PageLayout(
      pageOf(cells, 0, 0),
      axes(),
      settingOf([PLAIN, fill(4), fill(1)]),
    );
    expect(fills.length).toBe(4);
    expect(Math.max(...[...painted(fills).values()].map((pixel) => pixel.count))).toBe(1);
  });
});

describe("the borders of a page", () => {
  const boxed = (weight: "thin" | "medium"): GridCellStyle => ({
    ...PLAIN,
    borderTop: weight,
    borderRight: weight,
    borderBottom: weight,
    borderLeft: weight,
  });

  test("of a cell are a frame with its corners, over the grid lines before its edges", () => {
    const cells = { "2:3": { text: "", style: 1 } };
    const { borders } = new PageLayout(
      pageOf(cells, 0, 0),
      axes(),
      settingOf([PLAIN, boxed("thin")]),
    );
    const pixels = painted(borders);
    expect(pixels.size).toBe(76 * 21 - 74 * 19);
    expect(pixels.has("224,39")).toBe(true);
    expect(pixels.has("299,59")).toBe(true);
    expect(pixels.has("300,60")).toBe(false);
  });

  test("are as wide as whole pixels allow", () => {
    for (const [scale, thin, medium] of [
      [1, 1, 2],
      [1.25, 1, 3],
      [1.5, 2, 3],
      [2, 2, 4],
    ] as const) {
      for (const [style, width] of [
        [boxed("thin"), thin],
        [boxed("medium"), medium],
      ] as const) {
        const cells = { "2:3": { text: "", style: 1 } };
        const { borders } = new PageLayout(
          pageOf(cells, 0, 0),
          axes(),
          settingOf([PLAIN, style], scale),
        );
        const middle = Math.round(50 * scale);
        const left = [...painted(borders).keys()].filter(
          (key) => key.endsWith(`,${middle}`) && Number.parseInt(key, 10) < 250 * scale,
        );
        expect(left).toHaveLength(width);
      }
    }
  });

  test("are never painted twice, whatever meets whatever", () => {
    const weights = ["none", "thin", "medium"] as const;
    let state = 1;
    const any = () => {
      state = Math.imul(state, 48271) >>> 0;
      return weights[(state >>> 8) % 3] ?? "none";
    };
    const styles = [PLAIN];
    for (let style = 1; style < 60; style++) {
      styles.push({
        ...PLAIN,
        borderTop: any(),
        borderRight: any(),
        borderBottom: any(),
        borderLeft: any(),
      });
    }
    const cells: Record<string, { text: string; style: number }> = {};
    for (let row = 0; row < 12; row++) {
      for (let col = 0; col < 16; col++) {
        state = Math.imul(state, 48271) >>> 0;
        cells[`${row}:${col}`] = { text: "", style: (state >>> 8) % 60 };
      }
    }
    for (const scale of [1, 1.25, 1.5, 2]) {
      const page = pageOf(cells, 0, 0, 12);
      const { borders, geometry } = new PageLayout(page, axes(), settingOf(styles, scale));
      const pixels = painted(borders);
      expect(Math.max(...[...pixels.values()].map((pixel) => pixel.count))).toBe(1);
      // Every edge is there in the heavier of its two weights.
      const thin = Math.max(1, Math.round(scale));
      const widths = { none: 0, thin, medium: Math.round(2 * scale) };
      for (let row = 0; row < 12; row++) {
        for (let col = 1; col < 16; col++) {
          const of = (other: number) => styles[page.styleIds[row * 16 + other] ?? 0] ?? PLAIN;
          const left = widths[of(col - 1).borderRight];
          const right = widths[of(col).borderLeft];
          const x = geometry.xs[col] ?? 0;
          const y = ((geometry.ys[row] ?? 0) + (geometry.ys[row + 1] ?? 0)) >> 1;
          let width = 0;
          for (let at = x - 6; at < x + 6; at++) {
            width += pixels.has(`${at},${y}`) ? 1 : 0;
          }
          expect(width).toBe(Math.max(left, right));
        }
      }
    }
  });
});

describe("what is kept of a page", () => {
  const cells = { "0:0": "a", "70:20": "far", "64:15": "runs into the next page of columns" };
  const styles = [PLAIN];

  test("holds wherever the page is scrolled to, and while sizes around it change", () => {
    const layouts = new PageLayouts();
    const setting = settingOf(styles);
    const page = pageOf(cells, 1, 1);
    const kept = layouts.get(page, axes(), setting);
    expect(layouts.get(page, axes(), setting)).toBe(kept);
    // Rows above it, columns and rows after it: the page itself looks the same.
    expect(
      layouts.get(
        page,
        axes(
          [
            [3, 50],
            [200, 30],
          ],
          [[40, 200]],
        ),
        setting,
      ),
    ).toBe(kept);
  });

  test("does not hold when a size inside the page or under its overflow changes", () => {
    const layouts = new PageLayouts();
    const setting = settingOf(styles);
    const page = pageOf(cells, 1, 1);
    expect(page.overflows).toHaveLength(1);
    let kept = layouts.get(page, axes(), setting);
    for (const layout of [axes([[65, 50]]), axes([], [[20, 80]]), axes([], [[15, 80]])]) {
      const next = layouts.get(page, layout, setting);
      expect(next).not.toBe(kept);
      kept = next;
    }
  });

  test("does not hold for another page, scale, theme or freeze line", () => {
    const layouts = new PageLayouts();
    const setting = settingOf(styles);
    const page = pageOf(cells, 1, 1);
    const kept = layouts.get(page, axes(), setting);
    expect(layouts.get(pageOf(cells, 1, 1), axes(), setting)).not.toBe(kept);
    const { look } = settingOf(styles, 2);
    expect(layouts.get(page, axes(), { ...setting, look })).not.toBe(kept);
    const again = layouts.get(page, axes(), setting);
    expect(layouts.get(page, axes(), { ...setting, frozenCols: 1 })).not.toBe(again);
  });

  test("holds when styles are added, but not when one changes or had been missing", () => {
    const layouts = new PageLayouts();
    const setting = settingOf([PLAIN, fill(1)]);
    const page = pageOf({ "0:0": { text: "a", style: 1 } }, 0, 0);
    const late = pageOf({ "0:0": { text: "a", style: 2 } }, 0, 0);
    const kept = layouts.get(page, axes(), setting);
    const early = layouts.get(late, axes(), setting);
    expect(early.fills.length).toBe(0);

    setting.styles.use([{ ...PLAIN }, fill(1), fill(2)], theme);
    expect(layouts.get(page, axes(), setting)).toBe(kept);
    const arrived = layouts.get(late, axes(), setting);
    expect(arrived).not.toBe(early);
    expect(arrived.fills.length).toBe(1);

    setting.styles.use([PLAIN, fill(3), fill(2)], theme);
    expect(layouts.get(page, axes(), setting)).not.toBe(kept);
  });

  test("counts the cells that show text of their own", () => {
    const layout = axes([], [[2, 8]]);
    const page = pageOf({ "0:0": "a", "1:1": "b", "1:2": "hidden", "5:9": "c" }, 0, 0);
    const kept = new PageLayout(page, layout, settingOf(styles));
    expect(kept.cellsWithText(0, 64, 0, 16)).toBe(3);
    expect(kept.cellsWithText(1, 6, 1, 10)).toBe(2);
    expect(kept.cellsWithText(0, 64, 2, 9)).toBe(0);
  });
});
