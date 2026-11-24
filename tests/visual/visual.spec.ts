import { readFileSync, writeFileSync } from "node:fs";
import { expect, type Page, type TestInfo, test } from "@playwright/test";
import type { CaseOptions, CompareOptions, Report } from "../../apps/web/src/visual/hooks.ts";
import {
  CASE_NAMES,
  CASES,
  type CaseName,
  MUTATIONS,
  type Mutation,
  SCALES,
} from "../../apps/web/src/visual/names.ts";
import { difference, GOLDEN, goldenNames, readGolden, UPDATE, writeGolden } from "./golden.ts";
import { encodePng, type Picture } from "./png.ts";

/** Whether there is a WebGPU renderer, whose pictures must then be compared. */
const WEBGPU_COMPARED = false;

/** Tests that wait for that renderer. */
const onWebGpu = WEBGPU_COMPARED ? test : test.fixme;

const FONT = readFileSync(new URL("fonts/reticula-test.ttf", import.meta.url));

/** What went wrong in the page itself; nothing may. */
let complaints: string[] = [];

test.beforeEach(async ({ page }) => {
  complaints = [];
  page.on("pageerror", (error) => complaints.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error") {
      complaints.push(message.text());
    }
  });
  await page.goto("/visual.html");
  await page.evaluate((font) => window.__visual?.loadFont(font), FONT.toString("base64"));
});

test.afterEach(() => {
  expect(complaints).toEqual([]);
});

function emulate(page: Page, dark: boolean): Promise<void> {
  return page.emulateMedia({ colorScheme: dark ? "dark" : "light" });
}

/** Leaves a PNG with the results of a test. */
function keep(info: TestInfo, label: string, png: Buffer): void {
  writeFileSync(info.outputPath(`${label}.png`), png);
}

/** Compares two pictures of a case; a third, of their difference, is kept on failure. */
async function compare(
  page: Page,
  info: TestInfo,
  name: CaseName,
  options: CompareOptions = {},
): Promise<Report> {
  await emulate(page, options.dark ?? CASES[name].dark);
  const report = await page.evaluate(
    ([name, options]) => {
      if (!window.__visual) {
        throw new Error("the page is not ready");
      }
      return window.__visual.compare(name, options);
    },
    [name, options] as const,
  );
  if (report.diff && !options.mutation) {
    const { scale, dark, font, left, right } = report;
    const label = `${name}-${font}-${scale}-${dark ? "dark" : "light"}-${left}-${right}`;
    keep(info, label, Buffer.from(report.diff, "base64"));
  }
  return report;
}

function expectSame(report: Report): void {
  const { name, font, scale, dark, worst, firstStray } = report;
  const label = `${name} (${font} font, ${scale}x, ${dark ? "dark" : "light"})`;
  expect.soft(report.failures, `${label}, worst: ${JSON.stringify(worst)}`).toEqual([]);
  const stray = `${label}: pixels of text outside its boxes, first at ${JSON.stringify(firstStray)}`;
  expect.soft(report.strayInk, stray).toBe(0);
}

/** A case at every scale, in alternating themes, and at two of them in the product's font. */
function variants(name: CaseName): CaseOptions[] {
  const { dark } = CASES[name];
  return [
    ...SCALES.map((scale, index) => ({ scale, dark: index % 2 === 0 ? dark : !dark })),
    { font: "product" as const, scale: 1, dark },
    { font: "product" as const, scale: 1.5, dark: !dark },
  ];
}

async function picture(page: Page, name: CaseName): Promise<Picture> {
  await emulate(page, CASES[name].dark);
  const drawn = await page.evaluate((name) => window.__visual?.pixels(name, "canvas2d"), name);
  if (!drawn) {
    throw new Error("the page is not ready");
  }
  return { ...drawn, rgba: Buffer.from(drawn.rgba, "base64") };
}

// --- one renderer ------------------------------------------------------------

test.describe("Canvas 2D", () => {
  for (const name of CASE_NAMES) {
    test(`draws ${name} alike, its text within its boxes`, async ({ page }, info) => {
      for (const options of variants(name)) {
        const report = await compare(page, info, name, options);
        expectSame(report);
        const most = Math.max(report.inside.most, report.outside.most);
        expect.soft(most, "levels between two pictures of one renderer").toBeLessThanOrEqual(1);
        expect.soft(report.inkedPixels, "text to compare").toBeGreaterThan(0);
      }
    });
  }
});

// --- golden images -----------------------------------------------------------

test.describe("golden image", () => {
  test.skip(!GOLDEN, "golden images are of the CI runner; only there can they be compared");

  for (const name of CASE_NAMES) {
    test(`of ${name}`, async ({ page }, info) => {
      const actual = await picture(page, name);
      const golden = readGolden(name);
      if (UPDATE || (!golden && goldenNames().length === 0)) {
        const path = writeGolden(name, actual);
        info.annotations.push({ type: "golden", description: `written to ${path}` });
        return;
      }
      const save = (label: string, image: Picture) =>
        keep(info, `${name}-${label}`, encodePng(image));
      if (!golden) {
        writeGolden(name, actual);
        save("actual", actual);
        throw new Error(`there is no golden image of ${name}; the one drawn now is kept`);
      }
      const { over, most, first, picture: diff } = difference(actual, golden);
      if (over > 0) {
        writeGolden(name, actual);
        save("actual", actual);
        save("golden", golden);
        if (diff) {
          save("diff", diff);
        }
      }
      expect(over, `by up to ${most}, first at ${JSON.stringify(first)}`).toBe(0);
    });
  }

  test("of nothing but the cases", () => {
    if (goldenNames().length === 0) {
      console.log("there are no golden images yet: the ones drawn now are kept to become them");
    }
    const names: readonly string[] = CASE_NAMES;
    expect(goldenNames().filter((name) => !names.includes(name))).toEqual([]);
  });
});

/** The characters a TrueType font has: the ranges of its format 12 `cmap`. */
function charactersOf(font: Buffer): (codePoint: number) => boolean {
  const ranges: [number, number][] = [];
  for (let table = 0; table < font.readUInt16BE(4); table++) {
    const record = 12 + table * 16;
    if (font.toString("latin1", record, record + 4) !== "cmap") {
      continue;
    }
    const cmap = font.readUInt32BE(record + 8);
    for (let encoding = 0; encoding < font.readUInt16BE(cmap + 2); encoding++) {
      const subtable = cmap + font.readUInt32BE(cmap + 4 + encoding * 8 + 4);
      if (font.readUInt16BE(subtable) !== 12) {
        continue;
      }
      for (let group = 0; group < font.readUInt32BE(subtable + 12); group++) {
        const at = subtable + 16 + group * 12;
        ranges.push([font.readUInt32BE(at), font.readUInt32BE(at + 4)]);
      }
    }
  }
  return (codePoint) => ranges.some(([first, last]) => codePoint >= first && codePoint <= last);
}

// A character the font lacks would be drawn in whatever font the system has,
// and a golden image would change with the system.
test("the test font has every character the cases draw", async ({ page }) => {
  const has = charactersOf(FONT);
  for (const name of CASE_NAMES) {
    await emulate(page, CASES[name].dark);
    const characters =
      (await page.evaluate((name) => window.__visual?.characters(name), name)) ?? "";
    const lacking = [...characters].filter((character) => !has(character.codePointAt(0) ?? 0));
    expect.soft(lacking, name).toEqual([]);
  }
});

// --- two renderers -----------------------------------------------------------

test.describe("WebGPU", () => {
  test("is compared if this build has it", async ({ page }) => {
    const adapter = await page.evaluate(() => window.__visual?.adapter());
    expect(adapter?.backend).toBe(WEBGPU_COMPARED);
    console.log(`WebGPU adapter: ${JSON.stringify(adapter)}`);
    if (WEBGPU_COMPARED) {
      expect(adapter?.available, "the browser offers no WebGPU adapter").toBe(true);
    }
  });

  for (const name of CASE_NAMES) {
    onWebGpu(`draws ${name} as Canvas 2D does`, async ({ page }, info) => {
      for (const options of variants(name)) {
        expectSame(await compare(page, info, name, { ...options, right: "webgpu" }));
      }
    });
  }
});

// --- the comparison itself ---------------------------------------------------

const MUTATED: Record<Mutation, CaseName> = {
  "grid-lines": "plain-text",
  "selection-fill": "selection",
  "cell-background": "styles",
  border: "borders",
  dash: "dashes",
  "text-run": "plain-text",
};

test.describe("a picture that lacks", () => {
  for (const mutation of MUTATIONS) {
    for (const right of ["canvas2d", "webgpu"] as const) {
      const declare = right === "webgpu" ? onWebGpu : test;
      declare(`its ${mutation} is noticed (${right})`, async ({ page }, info) => {
        const name = MUTATED[mutation];
        // Below one device pixel per CSS pixel, dashes run into one another.
        for (const scale of mutation === "dash" ? SCALES.filter((s) => s >= 1) : SCALES) {
          for (const dark of [false, true]) {
            // Without its mistake the picture has to pass, or noticing means nothing.
            expectSame(await compare(page, info, name, { scale, dark, right }));
            const report = await compare(page, info, name, { scale, dark, right, mutation });
            expect.soft(report.ok, `${scale}x, ${dark ? "dark" : "light"}`).toBe(false);
            expect.soft(report.diff).not.toBeNull();
          }
        }
      });
    }
  }
});
