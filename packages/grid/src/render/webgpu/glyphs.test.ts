import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { packColor } from "../color.ts";
import { advanceOf, measure } from "../fixtures.ts";
import { Atlas } from "./atlas.ts";
import { FakeCanvas, FakeDevice, fakeBrowser, quadsOf } from "./fakes.ts";
import { InstanceWriter } from "./frame-buffer.ts";
import { type FontCheck, Glyphs, isSeparable, type RunPlace, unitsPerPixel } from "./glyphs.ts";
import type { Raster } from "./raster.ts";

const FONT = "13px sans-serif";
const BLACK = packColor(0, 0, 0);

let now = 0;

beforeEach(() => {
  fakeBrowser();
  now = 0;
  vi.spyOn(performance, "now").mockImplementation(() => now);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function open(check: FontCheck = { phases: 4, ascii: true }, units = 0) {
  const device = new FakeDevice();
  const canvas = new FakeCanvas();
  const upload = vi.fn<Raster["upload"]>();
  const raster = {
    kind: "copy" as const,
    context: canvas.context as unknown as CanvasRenderingContext2D,
    clear: () => {},
    upload,
  };
  const checked: string[] = [];
  const glyphs = new Glyphs(
    (onEvict) => new Atlas(device.asDevice, raster, (allocate) => allocate(), onEvict),
    (font) => {
      checked.push(font);
      return check;
    },
    () => units,
  );
  glyphs.begin(1000);
  const out = new InstanceWriter();
  const run = (text: string, place: Partial<RunPlace> = {}, color = BLACK, font = FONT) => {
    out.length = 0;
    glyphs.run(out, font, color, text, {
      origin: 0,
      x: 10,
      baseline: 20,
      width: measure.width(font, text),
      clip: null,
      row: 4,
      left: 0,
      right: 1000,
      ...place,
    });
    return quadsOf(out.bytes());
  };
  /** Texts that were drawn into the atlas, and where from. */
  const drawn = () => canvas.recorder.named("fillText").map(({ args }) => args);
  return { glyphs, run, drawn, checked, upload, canvas, out };
}

describe("a text is put together from its characters", () => {
  test("if none of them depends on the ones around it", () => {
    for (const text of [
      "Total: 1,234.50 €",
      "Łódź, Ærø, Việt",
      "Ωμέγα",
      "Учёт",
      "永东国酱爱",
      "カタカナ・ひらがな",
      "（全角）。￥１２",
      "한국어",
      "“quoted” – dash…",
      "",
    ]) {
      expect(isSeparable(text, true), text).toBe(true);
    }
    for (const text of [
      "e\u{301}",
      "שלום",
      "مرحبا",
      "नमस्ते",
      "สวัสดี",
      "\u{1112}\u{1161}\u{11ab}",
      "a\u{200d}b",
      "☺\u{fe0f}",
      "😀",
      "a\u{ad}b",
      "か\u{3099}",
      "tab\there",
      "a\u{202e}b",
    ]) {
      expect(isSeparable(text, true), JSON.stringify(text)).toBe(false);
    }
  });

  test("but not from those of ASCII in a font that joins some", () => {
    expect(isSeparable("a", false)).toBe(false);
    expect(isSeparable("永a", false)).toBe(false);
    expect(isSeparable("永é", false)).toBe(true);
    const { run, drawn } = open({ phases: 4, ascii: false });
    run("->");
    expect(drawn().map(([text]) => text)).toEqual(["->"]);
  });
});

test("the unit of a browser that counts in one shows in what it measures", () => {
  const widths = [279, 368, 264, 446, 305, 518, 397].map((count) => Math.fround(count / 40));
  expect(unitsPerPixel(widths)).toBe(40);
  expect(unitsPerPixel(widths.map((width) => Math.fround((width * 40) / 55)))).toBe(55);
  expect(unitsPerPixel([7.0078125, 9.23456, 11.317])).toBe(0);
});

describe("the characters of a text", () => {
  test("are drawn once for each phase of a pixel, and in each colour", () => {
    const { glyphs, run, drawn } = open();
    const x = 10.3;
    run("aa", { x });
    const second = (Math.floor(4 * (x + advanceOf(FONT, "a")) + 0.5) % 4) / 4;
    expect(second).not.toBe(0.25);
    expect(drawn().map(([text, at]) => [text, Number(at) % 1])).toEqual([
      ["a", 0.25],
      ["a", second],
    ]);
    const { misses } = glyphs;
    expect(run("aa", { x })).toHaveLength(2);
    expect(glyphs.misses).toBe(misses);
    run("a", { x }, packColor(255, 0, 0));
    expect(glyphs.misses).toBe(misses + 1);
    expect(drawn().at(-1)?.at(-1)).toBe("#ff0000");
  });

  test("are where a browser draws them that moves the pen by whole units", () => {
    // Forty to a pixel: some pens are half way between two phases.
    const units = 40;
    const advance = 279;
    const text = "1".repeat(200);
    const width = (text.length * advance) / units;
    const placed = (counted: number) => {
      const { run, canvas } = open(undefined, counted);
      const { measureText } = canvas.context as { measureText: (text: string) => object };
      canvas.context.measureText = (measured: string) => ({
        ...measureText(measured),
        width: Math.fround((measured.length * advance) / units),
      });
      return run(text, { x: 142, width }).map(({ x }) => x);
    };
    const steps = Array.from(text, (_, index) =>
      Math.floor((4 * (142 * units + index * advance) + units / 2) / units),
    );
    const [first = 0] = placed(units);
    const expected = steps.map((step) => Math.floor(step / 4) - 142 + first);
    expect(placed(units)).toEqual(expected);
    // Widths alone do not say it precisely enough.
    expect(placed(0)).not.toEqual(expected);
  });

  test("are where a 2D canvas draws them: at the nearest quarter of a pixel", () => {
    const { run } = open();
    for (const [x, pen] of [
      [10, 10],
      [10.12, 10],
      [10.125, 10.25],
      [10.37, 10.25],
      [10.88, 11],
      [-3.1, -3],
    ] as const) {
      // One pixel more than the ink on every side.
      expect(run("a", { x })[0]?.x, String(x)).toBe(Math.floor(pen) - 1);
    }
    const { run: whole } = open({ phases: 1, ascii: true });
    for (const [x, pen] of [
      [10.4, 10],
      [10.5, 11],
      [-3.6, -4],
    ] as const) {
      expect(whole("a", { x })[0]?.x).toBe(pen - 1);
    }
  });

  test("are rounded in the precision of a 2D canvas, which falls with the distance", () => {
    const x = 0.125 - 2 ** -20;
    const { run, drawn } = open();
    run("a", { x });
    run("b", { x, origin: 1024 });
    const phases = drawn().map(([, at]) => Number(at) % 1);
    expect(phases).toEqual([0, 0.25]);
  });

  test("are cut to the clip of their text, and say which row they are in", () => {
    const { run } = open();
    const clip = { left: 12, top: 15, right: 18, bottom: 30 };
    const quads = run("aaaa", { clip, row: 7 });
    expect(quads.length).toBeGreaterThan(0);
    for (const { x, y, width, height, row, textured } of quads) {
      expect(x).toBeGreaterThanOrEqual(12);
      expect(x + width).toBeLessThanOrEqual(18);
      expect(y).toBeGreaterThanOrEqual(15);
      expect(y + height).toBeLessThanOrEqual(30);
      expect([row, textured]).toEqual([7, true]);
    }
  });

  test("without ink have no quads", () => {
    const { run, drawn } = open();
    expect(run("a a")).toHaveLength(2);
    expect(drawn().map(([text]) => text)).not.toContain(" ");
  });

  test("do not make up a text whose width they do not add up to", () => {
    const { run, drawn } = open();
    run("ab", { width: measure.width(FONT, "ab") + 1 });
    expect(drawn().at(-1)?.[0]).toBe("ab");
  });

  test("of nothing that can be seen are not drawn", () => {
    const { run, drawn } = open();
    expect(run("a", {}, packColor(0, 0, 0, 0))).toEqual([]);
    expect(drawn()).toEqual([]);
  });
});

describe("a text that is drawn whole", () => {
  const emoji = "😀".repeat(300);

  test("is kept by its fraction of a pixel", () => {
    const { glyphs, run } = open();
    run("שלום", { x: 10 });
    run("שלום", { x: 300 });
    expect(glyphs.misses).toBe(1);
    run("שלום", { x: 10.3 });
    expect(glyphs.misses).toBe(2);
  });

  test("is drawn in blocks, and only those that may come into view", () => {
    const { glyphs, run } = open();
    const width = measure.width(FONT, emoji);
    expect(width).toBeGreaterThan(3 * 512);
    const quads = run(emoji, { x: 0, left: 600, right: 1000 });
    expect(quads.map(({ x, width }) => [x, width])).toEqual([[511, 512]]);
    expect(glyphs.partial).toBe(true);
    expect(glyphs.misses).toBe(1);
    run(emoji, { x: 0, left: 600, right: 1100 });
    expect(glyphs.misses).toBe(2);
  });

  test("does not count as cut short by what its clip hides", () => {
    const { glyphs, run } = open();
    const clip = { left: 0, top: 0, right: 400, bottom: 100 };
    expect(run(emoji, { x: 0, clip, left: -500, right: 900 })).toHaveLength(1);
    expect(glyphs.partial).toBe(false);
  });
});

describe("a frame that has no time left", () => {
  test("shows bars instead of texts it would have to draw, and those it has", () => {
    const { glyphs, run, drawn, out } = open();
    run("known");
    glyphs.begin(4);
    vi.spyOn(performance, "now").mockImplementation(() => (now += 3));
    const before = drawn().length;
    run("永");
    expect(glyphs.hasTime).toBe(false);
    const [bar] = run("国酱");
    expect(drawn().length - before).toBe(1);
    expect(glyphs.deferred).toBe(1);
    expect(bar).toMatchObject({ x: 10, textured: false, row: 4 });
    expect((bar?.color ?? 0) >>> 24).toBe(63);
    expect(run("known").every(({ textured }) => textured)).toBe(true);
    expect(out.length).toBe(5);
    // Next time.
    glyphs.begin(1000);
    vi.spyOn(performance, "now").mockImplementation(() => 0);
    expect(run("国酱").every(({ textured }) => textured)).toBe(true);
  });

  test("has none for what would take it to upload", () => {
    const { glyphs, run, drawn } = open();
    glyphs.begin(4);
    vi.spyOn(glyphs.atlas, "due", "get").mockImplementation(() => 3 * drawn().length);
    expect(run("永东国")[0]?.textured).toBe(false);
    expect(drawn()).toHaveLength(2);
  });

  test("may be allowed more", () => {
    const { glyphs } = open();
    glyphs.begin(4);
    now = 10;
    expect(glyphs.hasTime).toBe(false);
    glyphs.allow(2);
    expect(glyphs.hasTime).toBe(true);
    now = 12;
    expect(glyphs.hasTime).toBe(false);
    // Not less than it had.
    glyphs.begin(8);
    glyphs.allow(2);
    now = 19;
    expect(glyphs.hasTime).toBe(true);
  });

  test("draws some of a text's characters, but shows none of them", () => {
    const { glyphs, run } = open();
    glyphs.begin(4);
    vi.spyOn(performance, "now").mockImplementation(() => (now += 3));
    const quads = run("永东国");
    expect(quads).toHaveLength(1);
    expect(quads[0]?.textured).toBe(false);
    expect(glyphs.layers).toBe(0);
  });
});

test("what was in a layer that had to make room is drawn again", () => {
  const { glyphs, run } = open();
  const { atlas } = glyphs;
  glyphs.begin(Number.POSITIVE_INFINITY);
  run("永");
  const { layers } = glyphs;
  const { generations } = atlas.plan;
  expect(layers).toBe(1);
  // Every frame uses other pictures, and large ones.
  for (let seed = 0; atlas.evictions === 0; seed++) {
    glyphs.layers = 0;
    atlas.plan.now++;
    run(String.fromCodePoint(0x1f300 + seed), {}, BLACK, "900px sans-serif");
    atlas.plan.use(glyphs.layers);
    expect(seed).toBeLessThan(500);
  }
  expect(atlas.plan.holds(layers, generations)).toBe(false);
  const { misses } = glyphs;
  run("永");
  expect(glyphs.misses).toBe(misses + 1);
});

test("what texts of the same rows use does not make room for the next of them", () => {
  const { glyphs, run, drawn } = open();
  const { atlas } = glyphs;
  glyphs.begin(Number.POSITIVE_INFINITY);
  for (let seed = 0; atlas.evictions < 8; seed++) {
    glyphs.layers = 0;
    atlas.plan.now++;
    run("永");
    run(String.fromCodePoint(0x1f300 + seed), {}, BLACK, "900px sans-serif");
    expect(glyphs.layers & 1).toBe(1);
    expect(seed).toBeLessThan(1000);
  }
  expect(drawn().filter(([text]) => text === "永")).toHaveLength(1);
});

test("a font is checked once, whatever its size", () => {
  const { run, checked } = open();
  run("a");
  run("a", {}, BLACK, "26.5px sans-serif");
  run("a", {}, BLACK, "700 13px sans-serif");
  expect(checked).toEqual([FONT, "700 13px sans-serif"]);
});

test("small pictures are drawn once", () => {
  const { glyphs, canvas } = open();
  const spec = { kind: "corner", width: 8, height: 8, color: BLACK } as const;
  const sprite = glyphs.picture(spec);
  expect(sprite).toMatchObject({ layer: 0, width: 8, height: 8 });
  expect(glyphs.picture({ ...spec })).toBe(sprite);
  expect(canvas.recorder.named("drawImage")).toHaveLength(1);
});
