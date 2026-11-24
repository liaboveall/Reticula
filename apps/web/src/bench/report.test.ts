import { describe, expect, it } from "vitest";
import { csvOf, makePool } from "./b13-content.ts";
import {
  type Conditions,
  compareSides,
  type Measured,
  matchingRate,
  mergeRuns,
  refreshRates,
  refusals,
  renderReport,
  type S1Results,
  type S2Results,
  staleBuilds,
  summarizeFrames,
} from "./report.ts";
import { summarize } from "./stats.ts";

describe("frames", () => {
  it("of all rounds count together", () => {
    const summary = summarizeFrames([
      { work: [1, 2], interval: [4, 4], placeholderFrames: 1 },
      { work: [3, 40], interval: [4, 20], placeholderFrames: 2 },
    ]);
    expect(summary).toMatchObject({
      frames: 4,
      work: { count: 4, p50: 2, p99: 40, max: 40 },
      interval: { p50: 4, max: 20 },
      lateFrames: 1,
      framesOver60Hz: 1,
      shareOver60Hz: 0.25,
      placeholderFrames: 3,
    });
  });

  it("that come every 16.7 ms are on time at 60 Hz", () => {
    const frame = (interval: number) => ({ work: [1], interval: [interval], placeholderFrames: 0 });
    expect(summarizeFrames([frame(17.1)]).framesOver60Hz).toBe(0);
    expect(summarizeFrames([frame(17.3)]).framesOver60Hz).toBe(1);
    expect(summarizeFrames([]).shareOver60Hz).toBe(0);
  });
});

describe("the display's rate", () => {
  it("is read off what the system says", () => {
    expect(refreshRates(["Virtual 3840x2160 144 Hz", "Some GPU 2560x1600 240 Hz", "none"])).toEqual(
      [144, 240],
    );
  });

  it("is the one that frames come at, or none", () => {
    expect(matchingRate(4.17, [144, 240])).toBe(240);
    expect(matchingRate(6.9, [144, 240])).toBe(144);
    expect(matchingRate(5.97, [144, 240])).toBeNull();
    expect(matchingRate(16.7, [])).toBeNull();
  });
});

describe("archiving", () => {
  const fine: Conditions = {
    uncommittedChanges: false,
    softwareAdapter: false,
    idleIntervalMs: 4.17,
    displays: ["Some GPU 2560x1600 240 Hz"],
    headless: false,
    quick: false,
    published: true,
    problems: [],
  };

  it("is for reports with nothing wrong with them", () => {
    expect(refusals(fine)).toEqual([]);
    expect(refusals({ ...fine, softwareAdapter: null })).toEqual([]);
  });

  it.each<[string, Partial<Conditions>, RegExp]>([
    ["of uncommitted changes", { uncommittedChanges: true }, /uncommitted/],
    ["of an emulated GPU", { softwareAdapter: true }, /software/],
    ["with the display off", { idleIntervalMs: 5.97 }, /5\.97 ms.*240 Hz/],
    ["where nobody knows the display", { displays: [] }, /does not say/],
    ["with errors", { problems: ["an error on the page"] }, /an error on the page/],
    ["without a window", { headless: true }, /headless/],
    ["cut short", { quick: true }, /quick/],
  ])("refuses a run %s", (_, change, reason) => {
    const reasons = refusals({ ...fine, ...change });
    expect(reasons).toHaveLength(1);
    expect(reasons[0]).toMatch(reason);
  });

  it("is less strict about a directory of one's own, but not about what is measured", () => {
    const own = { ...fine, published: false, headless: true, quick: true };
    expect(refusals(own)).toEqual([]);
    expect(refusals({ ...own, uncommittedChanges: true })).toHaveLength(1);
  });

  it("is only of builds that were made of the commit", () => {
    const builds = [
      { name: "dist", madeMs: 2000 },
      { name: "dist-spikes", madeMs: 999 },
      { name: "nowhere", madeMs: null },
    ];
    expect(staleBuilds(builds.slice(0, 1), 1000)).toEqual([]);
    expect(staleBuilds(builds, 1000)).toEqual([
      "dist-spikes was not built after the commit",
      "nowhere was not built after the commit",
    ]);
  });

  it("gathers the runs of a commit, the main one first", () => {
    const run = (name: string, slowdown: number, dpr = 1, note = "") => ({
      browser: { name },
      slowdown,
      dpr,
      note,
    });
    const firefox = run("firefox", 1);
    const slow = run("chrome", 4);
    const main = run("chrome", 1);
    const dense = run("chrome", 1, 1.5);
    expect(mergeRuns([], firefox)).toEqual([firefox]);
    expect(mergeRuns([firefox, slow], main)).toEqual([main, firefox, slow]);
    const again = run("chrome", 1, 1, "again");
    expect(mergeRuns([main, firefox, dense], again)).toEqual([again, firefox, dense]);
  });
});

describe("two builds in turns", () => {
  it("are compared by the middle of their runs", () => {
    const sides = compareSides([1, 1.2, 1.1, 1.3], [2, 2.4, 9, 2.4]);
    expect(sides.baseMedian).toBeCloseTo(1.15);
    expect(sides.currentMedian).toBeCloseTo(2.4);
    expect(sides.ratio).toBeCloseTo(2.4 / 1.15);
  });

  it("say how much the earlier one differs from itself", () => {
    const { baseSpread, sameRatio } = compareSides([1, 1.2, 1, 1.2, 1, 1.2], [1, 1, 1]);
    expect(baseSpread).toBeCloseTo(0.2 / 1.1);
    expect(sameRatio).toBeCloseTo(1.2);
    expect(compareSides([2, 2], [2]).ratio).toBe(1);
  });
});

describe("the report on the grid", () => {
  const times = summarize([1, 2, 3]);
  const part = <Results>(results: Results): Measured<Results> => ({
    date: "2026-11-26T10:00:00.000Z",
    commit: "abc1234",
    uncommittedChanges: false,
    machine: { cpu: "Some CPU" },
    browser: { name: "chrome", version: "1.2", headless: true },
    options: { affinity: "FFFF", quick: false },
    environment: { frameInterval: times },
    results,
    problems: [],
  });
  const scroll = (rows: number, work = times): S1Results["scroll"][number] => ({
    margin: { rows, cols: 1 },
    visibleCells: 1000,
    screensPerSecond: 5,
    frames: 7200,
    framesWithPlaceholders: 0,
    mostPlaceholders: 0,
    rowsScrolled: 1,
    pagesReceived: 1,
    bytesReceived: 1,
    mostPagesHeld: 1,
    work,
    interval: times,
  });
  const jump: S1Results["jump"] = {
    margin: { rows: 2, cols: 1 },
    jumps: 200,
    filled: times,
    firstFrame: times,
    framesWithPlaceholders: times,
    work: summarize([6.14]),
    samples: [],
  };
  const s1 = part<S1Results>({
    roundTrip: {
      cellsPerPage: 1024,
      bytes: times,
      total: times,
      arrival: times,
      decode: times,
      samples: [],
    },
    codec: {
      pages: 1,
      rounds: 1,
      cellsPerPage: 1024,
      packetBytes: times,
      textBytesUtf8: times,
      textBytesUtf16: times,
      textUnits: times,
      encodeMicros: times,
      decodeMicros: times,
      textFromUtf8Micros: times,
      textFromUtf16Micros: times,
    },
    scroll: [scroll(0.5), scroll(2, summarize([1.25]))],
    jump,
  });
  const s2 = part<S2Results>({
    draw: [
      {
        label: "new text every frame",
        content: "unique",
        rowsPerFrame: 50,
        scale: 1,
        font: "13px sans-serif",
        frames: 600,
        cells: times,
        clipped: times,
        draw: times,
        interval: times,
        samples: [1],
      },
    ],
    pace: {
      content: "unique",
      screensPerSecond: 5,
      scale: 1,
      seconds: 10,
      frames: 2400,
      interval: times,
      lateFrames: 0,
      framesOver60Hz: 0,
      draw: times,
      samples: [1],
    },
  });

  it("has what the pages read off such reports", () => {
    const report = renderReport(s1, s2, part({ jump }));
    expect(report).toMatchObject({
      suite: "m6-render",
      commit: "abc1234",
      uncommittedChanges: false,
      affinity: "FFFF",
      scrollWorkP99Ms: 1.25,
      chromeS1: { scrollMargin2: { frames: 7200, placeholders: 0, work: { p99: 1.25 } } },
      firefoxHeadedJump: { jumpCompleteFrameWorkMs: { p99: 6.14, max: 6.14 } },
    });
    expect(JSON.stringify(report)).not.toContain("samples");
  });

  it("goes without Firefox, and not without the scrolling that is quoted", () => {
    expect(renderReport(s1, s2, null)).not.toHaveProperty("firefoxHeadedJump");
    const short = part({ ...s1.results, scroll: [scroll(0.5)] });
    expect(() => renderReport(short, s2, null)).toThrow(/margin of 2/);
  });

  it("is of uncommitted changes if any part of it is", () => {
    const changed = { ...s2, uncommittedChanges: true };
    expect(renderReport(s1, changed, null).uncommittedChanges).toBe(true);
  });
});

describe("the content", () => {
  it("has thousands of different characters and texts", () => {
    const { texts, flags } = makePool("distinct", 60_000);
    const han = new Set(texts.join("").match(/\p{Script=Han}/gu));
    expect(han.size).toBe(3_000);
    expect(new Set(texts).size).toBeGreaterThan(55_000);
    const numbers = flags.filter((flag) => flag !== 0).length;
    expect(numbers / texts.length).toBeCloseTo(0.5, 1);
    expect(texts.every((text, cell) => flags[cell] === 0 || /^-?\d+\.\d\d$/.test(text))).toBe(true);
  });

  it("repeats what is in short lists", () => {
    expect(new Set(makePool("repeating", 20_000).texts).size).toBeLessThanOrEqual(460);
  });

  it("is the same every time", () => {
    expect(makePool("distinct", 100).texts).toEqual(makePool("distinct", 100).texts);
    const csv = csvOf(3, 4).split("\n");
    expect(csv.map((line) => line.split(",").length)).toEqual([4, 4, 4, 1]);
    expect(csvOf(3, 4).split("\n")).toEqual(csv);
  });
});
