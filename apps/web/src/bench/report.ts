// What measured frames come to, and whether a report of them may be archived.
// The scripts in `bench/` import this as well as the measurement page: nothing
// in here needs a browser.

import type { CodecResult, JumpResult, RoundTripResult, ScrollResult } from "./s1.ts";
import type { DrawResult, PaceResult } from "./s2.ts";
import { type Summary, summarize } from "./stats.ts";

/** Frames that take longer than this to come are seen as late at 60 Hz. */
export const OVER_60HZ_MS = 1000 / 60 + 0.5;

/** One run's frames: the main thread's milliseconds, and the time since the frame before. */
export interface FrameSamples {
  readonly work: readonly number[];
  readonly interval: readonly number[];
  /** Frames in which bars stood for cells or their texts. */
  readonly placeholderFrames: number;
}

export interface FrameSummary {
  readonly frames: number;
  readonly work: Summary;
  readonly interval: Summary;
  /** Frames that came more than half an interval late, going by the median one. */
  readonly lateFrames: number;
  readonly framesOver60Hz: number;
  readonly shareOver60Hz: number;
  readonly placeholderFrames: number;
}

export function summarizeFrames(runs: readonly FrameSamples[]): FrameSummary {
  const intervals = runs.flatMap((run) => run.interval);
  const interval = summarize(intervals);
  const over = intervals.filter((sample) => sample > OVER_60HZ_MS).length;
  return {
    frames: intervals.length,
    work: summarize(runs.flatMap((run) => run.work)),
    interval,
    lateFrames: intervals.filter((sample) => sample > interval.p50 * 1.5).length,
    framesOver60Hz: over,
    shareOver60Hz: intervals.length > 0 ? over / intervals.length : 0,
    placeholderFrames: runs.reduce((sum, run) => sum + run.placeholderFrames, 0),
  };
}

/** The refresh rates in descriptions of displays, such as "Some GPU 2560x1600 240 Hz". */
export function refreshRates(displays: readonly string[]): number[] {
  return displays.flatMap((display) => {
    const rate = Number(/(\d+(?:\.\d+)?) Hz$/.exec(display)?.[1]);
    return rate > 0 ? [rate] : [];
  });
}

/** How far frames may be from a display's rate, as a share of it. */
const RATE_TOLERANCE = 0.05;

/** The display's rate that frames come at, or `null` if they come at none. */
export function matchingRate(intervalMs: number, rates: readonly number[]): number | null {
  const rate = 1000 / intervalMs;
  return rates.find((known) => Math.abs(rate - known) <= known * RATE_TOLERANCE) ?? null;
}

/** A build that is measured: its name, and when it was made, or `null` if it is not there. */
export interface Build {
  readonly name: string;
  readonly madeMs: number | null;
}

/** What is wrong with builds that are to be a commit's, made at a time. */
export function staleBuilds(builds: readonly Build[], commitMs: number): string[] {
  return builds
    .filter(({ madeMs }) => madeMs === null || madeMs < commitMs)
    .map(({ name }) => `${name} was not built after the commit`);
}

/** What decides whether a report may be archived. */
export interface Conditions {
  readonly uncommittedChanges: boolean;
  /** Whether the GPU is an emulated one; `null` where there is none to ask. */
  readonly softwareAdapter: boolean | null;
  /** The median time between frames with nothing being drawn. */
  readonly idleIntervalMs: number;
  readonly displays: readonly string[];
  readonly headless: boolean;
  /** Whether the run was cut short, to see that everything works. */
  readonly quick: boolean;
  /** Whether it is the repository's own results the report would be among. */
  readonly published: boolean;
  /** Errors of the pages, and workloads that were not what they are meant to be. */
  readonly problems: readonly string[];
}

/** The reasons for not archiving a report. */
export function refusals(conditions: Conditions): string[] {
  const { idleIntervalMs, displays, problems } = conditions;
  const reasons: string[] = [];
  if (conditions.uncommittedChanges) {
    reasons.push("the working tree has uncommitted changes");
  }
  if (conditions.softwareAdapter) {
    reasons.push("the WebGPU adapter is emulated in software");
  }
  const rates = refreshRates(displays);
  if (rates.length === 0) {
    reasons.push("the system does not say how often the display refreshes");
  } else if (matchingRate(idleIntervalMs, rates) === null) {
    reasons.push(
      `idle frames come every ${idleIntervalMs.toFixed(2)} ms, which is no display's rate (${rates.join(", ")} Hz)`,
    );
  }
  if (conditions.published && conditions.headless) {
    reasons.push("a headless run is not archived with the published results");
  }
  if (conditions.published && conditions.quick) {
    reasons.push("a quick run is not archived with the published results");
  }
  return [...reasons, ...problems];
}

/** Two builds measured in turns: what is compared is the middle one of several runs. */
export interface Sides {
  readonly base: readonly number[];
  readonly current: readonly number[];
  readonly baseMedian: number;
  readonly currentMedian: number;
  readonly ratio: number;
  /** How far apart the runs of the earlier build are from each other, as a share of their median. */
  readonly baseSpread: number;
  /** The earlier build against itself: the first run of every round and the last. */
  readonly sameRatio: number;
}

function median(values: readonly number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = sorted.length / 2;
  const [low = Number.NaN, high = low] =
    sorted.length % 2 === 0 ? sorted.slice(middle - 1) : sorted.slice(Math.floor(middle));
  return sorted.length % 2 === 0 ? (low + high) / 2 : low;
}

/** Compares runs that were made in rounds of base, current, current, base. */
export function compareSides(base: readonly number[], current: readonly number[]): Sides {
  const baseMedian = median(base);
  const currentMedian = median(current);
  return {
    base,
    current,
    baseMedian,
    currentMedian,
    ratio: currentMedian / baseMedian,
    baseSpread: (Math.max(...base) - Math.min(...base)) / baseMedian,
    sameRatio:
      median(base.filter((_, run) => run % 2 === 1)) /
      median(base.filter((_, run) => run % 2 === 0)),
  };
}

/** What tells the runs of one commit apart. */
export interface RunKey {
  readonly browser: { readonly name: string };
  readonly slowdown: number;
  readonly dpr: number;
}

const isSame = (a: RunKey, b: RunKey) =>
  a.browser.name === b.browser.name && a.slowdown === b.slowdown && a.dpr === b.dpr;

/** The run a report is about; the others are there to compare it with. */
const isMain = ({ browser, slowdown, dpr }: RunKey) =>
  browser.name === "chrome" && slowdown === 1 && dpr === 1;

/** The runs of a commit with one that was made now, which replaces an earlier one like it. */
export function mergeRuns<Run extends RunKey>(earlier: readonly Run[], run: Run): Run[] {
  const runs = [...earlier.filter((one) => !isSame(one, run)), run];
  return runs.sort((a, b) => Number(isMain(b)) - Number(isMain(a)));
}

/** What `spikes/grid/measure.mjs` writes about a run, as far as it is archived. */
export interface Measured<Results> {
  readonly date: string;
  readonly commit: string;
  readonly uncommittedChanges: boolean;
  readonly machine: object;
  readonly browser: { readonly name: string; readonly version: string; readonly headless: boolean };
  readonly options: { readonly affinity: string; readonly quick: boolean };
  readonly environment: { readonly frameInterval: Summary };
  readonly results: Results;
  readonly problems: readonly string[];
}

export interface S1Results {
  readonly roundTrip: RoundTripResult;
  readonly codec: CodecResult;
  readonly scroll: readonly ScrollResult[];
  readonly jump: JumpResult;
}

export interface S2Results {
  readonly draw: readonly (DrawResult & { readonly label: string })[];
  readonly pace: PaceResult;
}

const scrolled = (results: readonly ScrollResult[], rows: number) => {
  const found = results.find(({ margin }) => margin.rows === rows);
  if (!found) {
    throw new Error(`scrolling with a margin of ${rows} was not measured`);
  }
  return { frames: found.frames, placeholders: found.framesWithPlaceholders, work: found.work };
};

/** One report of what was measured of the grid: pages (S1), drawing (S2), and jumps in Firefox. */
export function renderReport(
  s1: Measured<S1Results>,
  s2: Measured<S2Results>,
  firefox: Measured<Pick<S1Results, "jump">> | null,
) {
  const { roundTrip, codec, scroll, jump } = s1.results;
  const { draw, pace } = s2.results;
  const scrollMargin2 = scrolled(scroll, 2);
  return {
    suite: "m6-render",
    date: s1.date,
    commit: s1.commit,
    uncommittedChanges: [s1, s2, firefox].some((one) => one?.uncommittedChanges),
    affinity: s1.options.affinity || null,
    machine: s1.machine,
    browser: s1.browser,
    scrollWorkP99Ms: scrollMargin2.work.p99,
    chromeS1: {
      roundTripMs: roundTrip.total,
      codecEncodeUs: codec.encodeMicros,
      codecDecodeUs: codec.decodeMicros,
      scrollMargin05: scrolled(scroll, 0.5),
      scrollMargin2,
      jumpFilledMs: jump.filled,
      jumpCompleteFrameWorkMs: jump.work,
    },
    chromeS2: {
      drawAllCases: draw.map(({ label, content, rowsPerFrame, draw }) => ({
        label,
        content,
        rowsPerFrame,
        draw,
      })),
      pace10s: {
        frames: pace.frames,
        lateFrames: pace.lateFrames,
        framesOver60Hz: pace.framesOver60Hz,
        interval: pace.interval,
        draw: pace.draw,
      },
    },
    ...(firefox && {
      firefoxHeadedJump: {
        browser: firefox.browser,
        jumpFilledMs: firefox.results.jump.filled,
        jumpCompleteFrameWorkMs: firefox.results.jump.work,
      },
    }),
  };
}
