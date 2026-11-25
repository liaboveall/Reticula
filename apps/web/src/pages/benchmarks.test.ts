import { describe, expect, it } from "vitest";
import type { GpuResult, GpuRun, TableRun } from "../generated/benchmarks.ts";
import {
  comparisons,
  latestComparison,
  latestTables,
  logPosition,
  tableQuestions,
  workloadName,
} from "./benchmarks.ts";

const RESULT: GpuResult = {
  part: "renderer",
  size: "10k",
  motion: "scroll",
  content: "distinct",
  renderer: "canvas2d",
  cells: 11_725,
  frames: 100,
  workP50Ms: 20,
  workP99Ms: 40,
  workMaxMs: 60,
  intervalP50Ms: 33,
  intervalP99Ms: 50,
  shareOver60Hz: 1,
  placeholderFrames: null,
};

const RUN: GpuRun = {
  report: "bench/results/2026-11-26-abc1234-gpu.json",
  date: "2026-11-26",
  commit: "abc1234",
  suite: "m6-gpu",
  cpu: "Some CPU",
  browser: "Chrome 1.2",
  headless: false,
  slowdown: 1,
  dpr: 1,
  refreshHz: 240,
  affinity: null,
  adapter: "some vendor",
  results: [],
};

describe("workloads", () => {
  it("have names that tell them apart", () => {
    expect(workloadName(RESULT)).toBe("10,000+ cells, scrolling");
    expect(workloadName({ ...RESULT, size: "1k", motion: "jump" })).toBe("1,000 cells, jumping");
    expect(workloadName({ ...RESULT, motion: "zoom", cells: 912 })).toBe("10,000+ cells, zooming");
    expect(workloadName({ ...RESULT, motion: "edit", content: "repeating" })).toBe(
      "10,000+ cells, editing, repeating texts",
    );
    expect(workloadName({ ...RESULT, part: "app", size: null, cells: 16_650 })).toBe(
      "App: 16,650 cells, scrolling",
    );
  });
});

describe("the questions to a data table", () => {
  const run = { browser: "Chrome 1.2", sortNumberMs: 250, summaryMs: 0 } as TableRun;
  const empty = Object.fromEntries(
    [
      "sortTextMs",
      "sortTwoMs",
      "filterNumberMs",
      "filterValuesMs",
      "filterTextMs",
      "listValuesMs",
    ].map((key) => [key, null]),
  );

  it("are those of the latest run in Chrome", () => {
    const firefox = { ...run, browser: "Firefox 160.0" };
    expect(latestTables([{ ...run, commit: "abc1234" }, run, firefox])).toBe(run);
    expect(latestTables([firefox, { ...run, browser: null }])).toBeNull();
    expect(latestTables([])).toBeNull();
  });

  it("are left out where they were not measured", () => {
    expect(tableQuestions({ ...run, ...empty })).toEqual([
      { name: "Sort by a number", ms: 250 },
      { name: "Summary of all columns", ms: 0 },
    ]);
  });
});

describe("the comparison of the renderers", () => {
  it("is the latest one in Chrome where nothing is held back", () => {
    const later = { ...RUN, commit: "def5678" };
    const others = [
      { ...later, slowdown: 4 },
      { ...later, dpr: 1.5 },
      { ...later, browser: "Firefox 160.0" },
      { ...later, browser: null },
    ];
    expect(latestComparison([RUN, later, ...others])).toBe(later);
    expect(latestComparison(others)).toBeNull();
    expect(latestComparison([])).toBeNull();
  });

  it("puts the two of a workload next to each other", () => {
    const webgpu: GpuResult = { ...RESULT, renderer: "webgpu", workP50Ms: 1 };
    const repeating: GpuResult[] = [
      { ...RESULT, content: "repeating" },
      { ...webgpu, content: "repeating" },
    ];
    const alone: GpuResult = { ...RESULT, motion: "edit" };
    const results = [webgpu, ...repeating, alone, RESULT];
    expect(comparisons({ ...RUN, results })).toEqual([
      { name: "10,000+ cells, scrolling", canvas2d: RESULT, webgpu },
    ]);
  });

  it("has a scale that tenths and hundreds of milliseconds are both on", () => {
    expect([0.1, 1, 10, 100, 1000].map(logPosition)).toEqual([0, 0.25, 0.5, 0.75, 1]);
    expect([null, 0, 0.01, 5000].map(logPosition)).toEqual([0, 0, 0, 1]);
  });
});
