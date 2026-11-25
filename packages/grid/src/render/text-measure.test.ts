import { describe, expect, test } from "vitest";
import { advanceOf, cacheOf, measure, PLAIN, theme } from "./fixtures.ts";
import type { GridCellStyle } from "./frame.ts";
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

describe("widths at another size", () => {
  const TEXTS = ["1234.50", "-0.5", "Widget", "数据线", "SKU 数 7", "café", "Учёт"];
  const SIZES = [13, 26, 19.5, 6.5, 13, 14];

  /** A context where a character is as wide as it is said to be, for a size. */
  function open(advance: (character: string, size: number) => number) {
    const asked: string[] = [];
    const context = {
      font: "",
      measureText(text: string) {
        asked.push(`${this.font}|${text}`);
        const size = Number.parseFloat(this.font);
        const advances = Array.from(text, (character) => advance(character, size));
        return { width: advances.reduce((sum, each) => sum + each, 0) };
      },
    };
    const measurer = new TextMeasurer(() => context as unknown as CanvasRenderingContext2D);
    return { asked, measurer };
  }
  const isHan = (character: string) => character.charCodeAt(0) >= 0x4e00;

  test("are what they would be if there had been no other size before", () => {
    const { open } = fakeContext();
    const measurer = new TextMeasurer(open);
    for (const size of SIZES) {
      const font = `${size}px sans-serif`;
      for (const text of TEXTS) {
        // The pen of a text that ends somewhere starts a width before that, and
        // its picture depends on where exactly.
        expect(measurer.width(font, text)).toBe(new TextMeasurer(open).width(font, text));
        expect(measurer.width(font, text)).toBeCloseTo(measure.width(font, text), 9);
      }
    }
  });

  test("of characters as wide as they are high are told by one of them", () => {
    const { asked, measurer } = open((character, size) => (isHan(character) ? size : size / 2));
    const texts = Array.from({ length: 500 }, (_, index) =>
      String.fromCharCode(0x4e00 + 7 * index, 0x4e01 + 7 * index),
    );
    for (const text of texts) {
      expect(measurer.width("13px serif", text)).toBe(26);
    }
    const before = asked.length;
    expect(before).toBeGreaterThan(1000);
    for (const text of texts) {
      expect(measurer.width("26px serif", text)).toBe(52);
    }
    expect(asked.length - before).toBeLessThan(40);
    // Those of another font may be other ones.
    expect(measurer.width("26px other", texts[0] ?? "")).toBe(52);
    expect(asked.filter((each) => each === `26px other|${texts[0]?.[0]}`)).toHaveLength(1);
  });

  test("of characters that are not are measured at each size", () => {
    const { measurer } = open((character, size) =>
      isHan(character) ? size + (character === "数" ? (size > 20 ? 3 : 1) : 0) : size / 2,
    );
    expect(measurer.width("13px serif", "数据")).toBe(27);
    expect(measurer.width("26px serif", "数据")).toBe(55);
    expect(measurer.width("13px serif", "数据")).toBe(27);
  });

  test("are measured where they are rounded, or spaced by size", () => {
    const ems: Record<string, number> = { f: 0.278, t: 0.278, i: 0.222, l: 0.222, " ": 0.278 };
    const em = (character: string) => (isHan(character) ? 1 : (ems[character] ?? 0.556));
    for (const advance of [
      (character: string, size: number) => Math.round(em(character) * size),
      (character: string, size: number) => Math.round((em(character) * size + 4 / size) * 64) / 64,
    ]) {
      const { measurer } = open(advance);
      for (const size of SIZES) {
        for (const text of ["1234.50", "Widget", "fill it", "5.1 of 数据"]) {
          const width = Array.from(text).reduce((sum, each) => sum + advance(each, size), 0);
          // Exactly: what is a hair more than whole pixels would make columns wider.
          expect(measurer.width(`${size}px sans-serif`, text)).toBe(width);
        }
      }
    }
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
    const fit = (styles: GridCellStyle[] | undefined, col: number, name = "B", room = 0) =>
      measurer.columnFitWidth(theme, cache, styles, name, room, col, 0, 64);
    expect(fit([PLAIN, bold], 1)).toBe(wanted);
    expect(fit([PLAIN, bold], 2)).toBe(600);
    expect(fit(undefined, 3)).toBe(48);

    // A name may be longer than the texts, and has a button next to it.
    const header = [..."a name of some length"].reduce(
      (sum, c) => sum + advanceOf("600 13px sans-serif", c),
      0,
    );
    expect(fit([PLAIN, bold], 1, "a name of some length")).toBe(Math.ceil(header + 24));
    expect(fit([PLAIN, bold], 1, "a name of some length", 33)).toBe(Math.ceil(header + 24 + 33));
    expect(fit(undefined, 3, "x".repeat(400), 33)).toBe(600);
  });
});
