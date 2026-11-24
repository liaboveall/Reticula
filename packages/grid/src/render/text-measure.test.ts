import { describe, expect, test } from "vitest";
import { advanceOf, cacheOf, measure, PLAIN, theme } from "./fixtures.ts";
import { fontOf, TextMeasurer } from "./text-measure.ts";

/** A context that counts what it is asked to measure. Fonts named "joined" join letters. */
function fakeContext() {
  const asked: string[] = [];
  const context = {
    font: "",
    measureText(text: string) {
      asked.push(`${this.font}|${text}`);
      const joined = this.font.includes("joined") && text.length > 1;
      return {
        width: measure.width(this.font, text) - (joined ? text.length * 0.5 : 0),
        fontBoundingBoxAscent: 10,
        fontBoundingBoxDescent: 3,
      };
    },
  };
  return { asked, open: () => context as unknown as CanvasRenderingContext2D };
}

const FONT = "13px sans-serif";

describe("widths", () => {
  test("add up from the characters of texts that allow it, each measured once", () => {
    const { asked, open } = fakeContext();
    const measurer = new TextMeasurer(open);
    for (const text of ["1234.50", "-0.5", "Widget", "数据线", "SKU 数 7", "café", "Учёт", ""]) {
      expect(measurer.width(FONT, text)).toBeCloseTo(measure.width(FONT, text), 9);
    }
    expect(new Set(asked).size).toBe(asked.length);
    expect(measurer.size).toBe(0);
    const before = asked.length;
    measurer.width(FONT, "5.1 数 4321 Widget");
    expect(asked).toHaveLength(before);
  });

  test("are of whole texts where characters depend on each other", () => {
    const { asked, open } = fakeContext();
    const measurer = new TextMeasurer(open);
    for (const text of ["e\u{301}", "\u{5e9}\u{5dc}", "ไทย", "👩‍🚀", "数\u{3001}"]) {
      expect(measurer.width(FONT, text)).toBeCloseTo(measure.width(FONT, text), 9);
      expect(asked).toContain(`${FONT}|${text}`);
    }
    expect(measurer.size).toBe(5);
    const before = asked.length;
    measurer.width(FONT, "ไทย");
    expect(asked).toHaveLength(before);
  });

  test("are of whole texts in a font that joins characters", () => {
    const { open } = fakeContext();
    const measurer = new TextMeasurer(open);
    const font = "13px joined";
    expect(measurer.width(font, "fi")).toBeCloseTo(measure.width(font, "fi") - 1, 9);
    expect(measurer.size).toBe(1);
  });

  test("are remembered for each font apart", () => {
    const { asked, open } = fakeContext();
    const measurer = new TextMeasurer(open);
    const texts = ["ไทย", "한국어"];
    for (let round = 0; round < 3; round++) {
      for (const font of [FONT, "700 13px sans-serif", "26px sans-serif"]) {
        for (const text of texts) {
          expect(measurer.width(font, text)).toBeCloseTo(measure.width(font, text), 9);
        }
      }
    }
    for (const text of texts) {
      expect(asked.filter((entry) => entry.endsWith(`|${text}`))).toHaveLength(3);
    }
  });

  test("are never more than the limit, and those in use survive", () => {
    const { asked, open } = fakeContext();
    const measurer = new TextMeasurer(open, 100);
    const used = "ไทย";
    for (let index = 0; index < 1000; index++) {
      measurer.width(index % 2 ? FONT : "700 13px sans-serif", `한${index}`);
      measurer.width(FONT, used);
      expect(measurer.size).toBeLessThanOrEqual(100);
    }
    expect(asked.filter((entry) => entry === `${FONT}|${used}`)).toHaveLength(1);
    expect(measurer.size).toBeGreaterThan(50);
  });

  test("of fonts no longer used are forgotten", () => {
    const { open } = fakeContext();
    const measurer = new TextMeasurer(open);
    for (let size = 1; size < 200; size++) {
      measurer.width(`${size}px sans-serif`, "ไทย");
      expect(measurer.size).toBeLessThanOrEqual(32);
    }
  });

  test("are measured again after everything was forgotten", () => {
    const { asked, open } = fakeContext();
    const measurer = new TextMeasurer(open);
    measurer.width(FONT, "ไทย");
    measurer.line(FONT);
    const before = asked.length;
    measurer.reset();
    expect(measurer.size).toBe(0);
    measurer.width(FONT, "ไทย");
    measurer.line(FONT);
    expect(asked).toHaveLength(2 * before);
  });
});

describe("fitting a column", () => {
  test("goes by its widest text in the font it is drawn in, and no further than a limit", () => {
    const { open } = fakeContext();
    const measurer = new TextMeasurer(open);
    const bold = { ...PLAIN, bold: true };
    const cells = {
      "0:1": "ab",
      "1:1": { text: "a longer text", style: 1 },
      "2:1": "a longer text",
      "70:1": "out of the rows that are asked about, and much longer",
      "3:2": "x".repeat(500),
    };
    const cache = cacheOf(cells, 2, 1);
    const font = fontOf(theme, 1);
    const wanted = Math.ceil(
      [..."a longer text"].reduce((sum, c) => sum + advanceOf(font, c), 0) + 18,
    );
    expect(measurer.columnFitWidth(theme, cache, [PLAIN, bold], 1, 0, 64)).toBe(wanted);
    expect(measurer.columnFitWidth(theme, cache, [PLAIN, bold], 2, 0, 64)).toBe(600);
    expect(measurer.columnFitWidth(theme, cache, undefined, 3, 0, 64)).toBe(48);
  });
});
