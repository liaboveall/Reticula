import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Provenance, RendererComparison, TableRun } from "../generated/benchmarks.ts";
import { BENCHMARKS, PROVENANCE } from "../generated/benchmarks.ts";
import { pageTitle } from "../ui/page.ts";
import { Architecture } from "./Architecture.tsx";
import { Articles } from "./Articles.tsx";
import { Checks } from "./Checks.tsx";
import { pitch } from "./Hero.tsx";
import { Numbers } from "./Numbers.tsx";
import { measured, mib, ms, percent, seconds, us } from "./numbers.ts";
import { Renderers } from "./Renderers.tsx";
import { Source } from "./Source.tsx";

const FROM: Provenance = {
  report: "bench/results/2026-11-25-abc1234-gpu.json",
  commit: "abc1234",
  date: "2026-11-25",
  uncommittedChanges: false,
  cpu: "Some CPU",
  logicalProcessors: 8,
  memoryGb: 16,
  os: "Some OS",
  browser: "Chrome 1.2",
  headless: false,
  affinity: null,
};

/** What a reader gets, without the markup. */
const textOf = (markup: string) =>
  markup
    .replace(/<[^>]+>/g, " ")
    .replace(/\s+/g, " ")
    .trim();

const count = (markup: string, pattern: RegExp) => markup.match(pattern)?.length ?? 0;

afterEach(() => {
  vi.doUnmock("../generated/benchmarks.ts");
  vi.resetModules();
});

describe("numbers", () => {
  it("are shown with as many digits as mean something", () => {
    expect([ms(0.638), ms(19.6), ms(1033.2), us(4.71)]).toEqual([
      "0.64 ms",
      "19.6 ms",
      "1,033 ms",
      "4.7 µs",
    ]);
    expect([seconds(1033.2), mib(552.62), percent(0.9965)]).toEqual([
      "1.03 s",
      "553 MiB",
      "99.65%",
    ]);
  });

  it("that were not measured are not made up", () => {
    expect(measured(null, ms)).toBe("not available");
    expect(measured(0, ms)).toBe("0.00 ms");
  });

  it("all say where they come from", () => {
    const markup = renderToStaticMarkup(createElement(Numbers));
    const cells = count(markup, /data-testid="number-/g);
    expect(cells).toBeGreaterThan(6);
    expect(count(markup, /data-testid="provenance"/g)).toBe(cells);
    expect(count(markup, /\/commit\/[0-9a-f]{7}"/g)).toBe(cells);
    expect(count(markup, /\/tree\/main\/bench\/results\/[^"]+\.json"/g)).toBe(cells);
    expect(markup).not.toMatch(/NaN|null|undefined/);
  });

  it("are not available one by one when the reports have none", async () => {
    vi.resetModules();
    vi.doMock("../generated/benchmarks.ts", () => ({
      BENCHMARKS: Object.fromEntries(Object.keys(BENCHMARKS).map((key) => [key, null])),
      PROVENANCE: Object.fromEntries(Object.keys(PROVENANCE).map((key) => [key, null])),
      TABLES: null,
      TABLES_PROVENANCE: null,
    }));
    const { Numbers: Empty } = await import("./Numbers.tsx");
    const markup = renderToStaticMarkup(createElement(Empty));
    const values = [...markup.matchAll(/class="landing-value">([^<]*)</g)].map(([, text]) => text);
    expect(values.length).toBeGreaterThan(6);
    expect(new Set(values)).toEqual(new Set(["not available"]));
    expect(markup).not.toMatch(/NaN|null|undefined|\b0(?:\.0+)? ms/);
  });

  it("are of data tables as well, once there is a report about them", async () => {
    const run: TableRun = {
      report: FROM.report,
      date: FROM.date,
      commit: FROM.commit,
      suite: "some-tables",
      cpu: FROM.cpu,
      browser: FROM.browser,
      headless: false,
      affinity: null,
      rows: 10_000_000,
      columns: 13,
      textBytes: 988_455_324,
      fileBytes: 209_715_200,
      importMs: 8_120,
      importTwiceMs: 14_000,
      importLastMs: 15_000,
      importHardMs: 40_000,
      importFrameGapMs: 21,
      sortNumberMs: 251,
      sortTextMs: 900,
      sortTwoMs: 500,
      filterNumberMs: 40.2,
      filterValuesMs: 41,
      filterTextMs: 200,
      listValuesMs: 250,
      summaryMs: 300,
      scrollWorkP99Ms: 3.4,
      scrollPlaceholderFrames: 0,
      jumpP95Ms: 61,
      pageWhileSortingP95Ms: 19,
      peakMemoryMiB: 300,
      longestStepMs: null,
    };
    const render = async (tables: TableRun | null) => {
      vi.resetModules();
      vi.doMock("../generated/benchmarks.ts", () => ({
        BENCHMARKS,
        PROVENANCE,
        TABLES: tables,
        TABLES_PROVENANCE: tables && FROM,
      }));
      const { Numbers: Both } = await import("./Numbers.tsx");
      return renderToStaticMarkup(createElement(Both));
    };
    expect(await render(null)).not.toContain("number-table");
    const markup = await render(run);
    expect(count(markup, /data-testid="number-table/g)).toBe(3);
    const text = textOf(markup);
    expect(text).toContain("Import 10,000,000 rows of CSV 8.12 s 943 MiB in 13 columns");
    expect(text).toContain("into a table of 200 MiB abc1234 2026-11-25");
    expect(text).toContain("251 ms by a text 900 ms; filter by a number 40.2 ms");
    expect(text).toContain("3.40 ms p99 of scrolling");
    expect(text).toContain("; 0 frames with placeholders");
    expect(textOf(await render({ ...run, importMs: null, fileBytes: null }))).toContain(
      "rows of CSV not available 943 MiB in 13 columns, into a table of not available",
    );
  });
});

describe("where numbers come from", () => {
  const source = (of: Provenance | null, ran?: "in a browser" | "natively" | "anywhere") =>
    textOf(renderToStaticMarkup(createElement(Source, ran ? { of, ran } : { of })));

  it("is told with what is known of it", () => {
    expect(source(FROM)).toBe("abc1234 2026-11-25 Chrome 1.2 Some CPU report");
    expect(source({ ...FROM, headless: true, affinity: "FFFF", uncommittedChanges: true })).toBe(
      "abc1234 (uncommitted changes) 2026-11-25 Chrome 1.2 (headless) Some CPU (affinity FFFF) report",
    );
  });

  it("is told without what is not", () => {
    expect(source(null)).toBe("not available");
    expect(source({ ...FROM, cpu: null, browser: null })).toBe(
      "abc1234 2026-11-25 not available machine not recorded report",
    );
    expect(source({ ...FROM, cpu: null, browser: null }, "natively")).toBe(
      "abc1234 2026-11-25 machine not recorded report",
    );
    expect(source({ ...FROM, cpu: null, browser: null }, "anywhere")).toBe(
      "abc1234 2026-11-25 report",
    );
  });
});

describe("the comparison of the renderers", () => {
  const data: RendererComparison = {
    provenance: FROM,
    visibleCells: 12_345,
    width: 2560,
    height: 1300,
    refreshHz: 240,
    results: [
      { renderer: "canvas2d", workP50Ms: 8, workP99Ms: 16, lateFrames: 0.25 },
      { renderer: "webgpu", workP50Ms: 1.5, workP99Ms: 4, lateFrames: null },
    ],
  };

  it("is not there before there has been one", () => {
    expect(renderToStaticMarkup(createElement(Renderers, { data: null }))).toBe("");
  });

  it("is drawn to scale, with where it comes from", () => {
    const markup = renderToStaticMarkup(createElement(Renderers, { data }));
    const text = textOf(markup);
    expect(text).toContain("12,345 cells in view, 2560 × 1300");
    expect(text).toContain("Canvas 2D 16.0 ms 8.00 ms 25.00%");
    expect(text).toContain("WebGPU 4.00 ms 1.50 ms not available");
    expect(text).toContain("abc1234 2026-11-25");
    const lengths = [...markup.matchAll(/inline-size:([\d.]+)%/g)].map(([, size]) => Number(size));
    expect(lengths).toEqual([60, 15]);
  });

  it("copes with a run that measured nothing", () => {
    const results = data.results.map((one) => ({ ...one, workP50Ms: null, workP99Ms: null }));
    const markup = renderToStaticMarkup(createElement(Renderers, { data: { ...data, results } }));
    expect(markup).not.toContain("NaN");
  });
});

describe("articles", () => {
  it("leave nothing behind when there are none", () => {
    expect(renderToStaticMarkup(createElement(Articles, { articles: [] }))).toBe("");
    const chinese = [{ url: "https://example.com", date: "2026-11-25", title: { "zh-CN": "题" } }];
    expect(renderToStaticMarkup(createElement(Articles, { articles: chinese }))).toBe("");
  });

  it("are listed in the page's language", () => {
    const articles = [
      { url: "https://example.com/a", date: "2026-11-25", title: { en: "One", "zh-CN": "一" } },
    ];
    const markup = renderToStaticMarkup(createElement(Articles, { articles }));
    expect(textOf(markup)).toBe("Articles One 2026-11-25");
  });
});

describe("the diagram", () => {
  it("keeps everything inside its frame, in both shapes", () => {
    const markup = renderToStaticMarkup(createElement(Architecture));
    const drawings = [...markup.matchAll(/viewBox="([^"]+)"[\s\S]*?<\/svg>/g)];
    expect(drawings.length).toBe(2);
    for (const [drawing, viewBox = ""] of drawings) {
      const [left = 0, top = 0, width = 0, height = 0] = viewBox.split(" ").map(Number);
      const points = [
        ...[...drawing.matchAll(/ x="([\d.-]+)" y="([\d.-]+)"/g)].map((match) => match.slice(1, 3)),
        ...[...drawing.matchAll(/points="([^"]+)"/g)].flatMap(([, list = ""]) =>
          list.split(" ").map((point) => point.split(",")),
        ),
        ...[
          ...drawing.matchAll(/ x="([\d.-]+)" y="([\d.-]+)" width="([\d.]+)" height="([\d.]+)"/g),
        ].map(([, x, y, w, h]) => [String(Number(x) + Number(w)), String(Number(y) + Number(h))]),
      ];
      expect(points.length).toBeGreaterThan(50);
      for (const [x, y] of points.map((point) => point.map(Number))) {
        expect(x).toBeGreaterThanOrEqual(left);
        expect(x).toBeLessThanOrEqual(left + width);
        expect(y).toBeGreaterThanOrEqual(top);
        expect(y).toBeLessThanOrEqual(top + height);
      }
    }
  });
});

describe("the checks", () => {
  it("name their sizes and where they are", () => {
    const markup = renderToStaticMarkup(createElement(Checks));
    expect(markup).not.toMatch(/NaN|null|undefined|not available/);
    expect(count(markup, /\/tree\/main\/[a-z][^"]*"/g)).toBeGreaterThan(8);
    expect(textOf(markup)).toMatch(/[0-9a-f]{16}/);
  });
});

describe("the document", () => {
  const html = readFileSync(fileURLToPath(new URL("../../index.html", import.meta.url)), "utf8");
  const content = (attribute: string) =>
    new RegExp(`${attribute}"\\s+content="([^"]*)"`).exec(html)?.[1];

  it("says what the page says", () => {
    const [heading, lead] = pitch();
    expect(/<title>([^<]*)</.exec(html)?.[1]).toBe(pageTitle("landing"));
    expect(content('property="og:title')).toBe(pageTitle("landing"));
    expect(content('name="description')).toBe(`${heading} ${lead}`);
    expect(content('property="og:description')).toBe(heading);
    expect(content('property="og:image')).toBe("/media/og.webp");
  });

  it("has the inline script's place before everything that loads", () => {
    const marker = html.indexOf("<!-- boot -->");
    expect(marker).toBeGreaterThan(0);
    for (const tag of ["<script", "<link", "<style"]) {
      const first = html.indexOf(tag);
      expect(first === -1 || first > marker, tag).toBe(true);
    }
  });
});
