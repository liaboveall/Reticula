// B13: frames of a thousand and of more than ten thousand visible cells, on a
// 2D canvas and with WebGPU.
//
//   node bench/run-render-bench.mjs [options]
//
// Two parts, both with the renderers of the product. In the first, they draw
// pages made up on the measurement page (apps/web/dist-spikes), so that nothing
// but drawing is timed: two sizes, four motions, two kinds of content, the
// renderers in turns for several rounds in one browser. The second is the app
// itself (apps/web/dist) with its engine, scrolling a wide workbook zoomed out
// to 25%.
//
// A report is written to target/bench-render/ in any case, and archived if
// nothing is wrong with it: not with changes that are not committed, an
// emulated GPU, frames that do not come at the display's rate, errors on a page,
// or a workload that is not what it is meant to be. Runs of the same commit in
// other browsers, or on a slowed main thread, go into the same archive.
//
// `cargo xtask bench --suite render` builds both first and then runs this.
//
// Options:
//   --browser <name>   chrome (the installed Google Chrome, the default),
//                      chromium or firefox (the builds Playwright downloads)
//   --headless         no window. For trying things out: a headless browser has
//                      no display, and Firefox has no GPU then
//   --slowdown <n>     throttle the pages' main thread by this factor (Chromium
//                      only), as a rough stand-in for a slower machine
//   --dpr <number>     device pixels per CSS pixel (default 1)
//   --rounds <n>       rounds of the first part (default 3)
//   --seconds <s>      what is measured of each run of it, after a warm-up of
//                      0.25 s (default 0.5)
//   --product-seconds <s>  what is measured of the second part (default 8)
//   --only <part>      synthetic or product; such a report is not archived
//   --quick            a short run, to see that everything works
//   --software         an emulated GPU (Chromium only), where there is no other
//   --affinity <mask>  pin this process, and so the browser, to a set of logical
//                      processors: a hexadecimal mask
//   --pages <dir>, --site <dir>  other builds than apps/web/dist-spikes and dist
//   --results <dir>    where to archive (default bench/results)
//   --no-archive       only write the report to target/bench-render/

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { parseArgs } from "node:util";
import { chromium, firefox } from "@playwright/test";
import { csvOf } from "../apps/web/src/bench/b13-content.ts";
import { matchingRate, mergeRuns, refreshRates, refusals, summarizeFrames } from "../apps/web/src/bench/report.ts";
import { summarize } from "../apps/web/src/bench/stats.ts";
import { root, serve } from "../spikes/grid/serve.mjs";
import { commit, machine } from "../spikes/machine.mjs";
import { pin } from "../spikes/pin.mjs";
import { archive, builtEarlier, PUBLISHED, writeReport } from "./archive.mjs";

/** The window, in CSS pixels. Around the grid, the app takes about 170 of the height. */
const VIEWPORT = { width: 2560, height: 1300 };
const WARMUP_SECONDS = 0.25;
const SIZES = ["1k", "10k"];
const MOTIONS = ["scroll", "jump", "zoom", "edit"];
const CONTENTS = ["repeating", "distinct"];
const RENDERERS = ["canvas2d", "webgpu"];
/** The workbook that the app scrolls: wider than the window at 25%, and longer than it gets. */
const SHEET = { rows: 14_000, cols: 120 };
const LEAST_CELLS = 10_000;
/** What WebGPU is to come to in the app, on the reference machine. */
const TARGETS = { workP99Ms: 8, shareOver60Hz: 0.01 };
const SCREENS_PER_SECOND = 5;
const PRODUCT_WARMUP_SECONDS = 2;

// --- options ---------------------------------------------------------------

function fail(message) {
  console.error(message);
  process.exit(2);
}

const { values: given } = parseArgs({
  options: {
    browser: { type: "string", default: "chrome" },
    headless: { type: "boolean", default: false },
    slowdown: { type: "string", default: "1" },
    dpr: { type: "string", default: "1" },
    rounds: { type: "string" },
    seconds: { type: "string" },
    "product-seconds": { type: "string" },
    only: { type: "string", default: "" },
    quick: { type: "boolean", default: false },
    software: { type: "boolean", default: false },
    affinity: { type: "string", default: "" },
    pages: { type: "string", default: join(root, "apps/web/dist-spikes") },
    site: { type: "string", default: join(root, "apps/web/dist") },
    results: { type: "string", default: PUBLISHED },
    "no-archive": { type: "boolean", default: false },
  },
});
const { browser: browserName, headless, only, quick, software, affinity } = given;
const slowdown = Number(given.slowdown);
const dpr = Number(given.dpr);
const rounds = Number(given.rounds ?? (quick ? 1 : 3));
const seconds = Number(given.seconds ?? (quick ? 0.25 : 0.5));
const productSeconds = Number(given["product-seconds"] ?? (quick ? 2 : 8));
if (!["chrome", "chromium", "firefox"].includes(browserName)) {
  fail(`unknown browser ${browserName}`);
}
if (!["", "synthetic", "product"].includes(only)) {
  fail("--only takes synthetic or product");
}
if (![slowdown, dpr, rounds, seconds, productSeconds].every((value) => value > 0)) {
  fail("--slowdown, --dpr, --rounds, --seconds and --product-seconds take positive numbers");
}
const isFirefox = browserName === "firefox";
if (isFirefox && (slowdown > 1 || software)) {
  fail("--slowdown and --software work in Chromium only");
}
const results = resolve(given.results);
const pagesDirectory = resolve(given.pages);
const siteDirectory = resolve(given.site);

const cases = SIZES.flatMap((size) =>
  MOTIONS.flatMap((motion) => CONTENTS.map((content) => ({ size, motion, content }))),
);
// Opening a run and getting it ready take about as long as its warm-up; in the
// app, it is loading the workbook that takes time.
const estimate = {
  synthetic: only === "product" ? 0 : cases.length * RENDERERS.length * rounds * (2 * WARMUP_SECONDS + seconds),
  product: only === "synthetic" ? 0 : RENDERERS.length * (4 + PRODUCT_WARMUP_SECONDS + productSeconds),
};
console.log(
  `about ${Math.ceil((estimate.synthetic + estimate.product) / 10) * 10} s${headless ? "" : ", with a window on the screen that must stay in view"}`,
);

// The browser is started by this process and inherits the setting.
if (affinity) {
  pin(affinity);
}

// --- the browser -----------------------------------------------------------

const pages = await serve(0, { site: pagesDirectory });
const site = await serve(0, { site: siteDirectory, document: "index.html" });
const browser = await (isFirefox ? firefox : chromium).launch({
  headless,
  ...(browserName === "chrome" ? { channel: "chrome" } : {}),
  // The headless shell has no GPU; the full browser's headless mode has.
  ...(browserName === "chromium" ? { channel: "chromium" } : {}),
  ...(isFirefox
    ? { firefoxUserPrefs: { "layout.css.devPixelsPerPx": String(dpr) } }
    : {
        args: [
          // An emulated density would have the compositor scale the window.
          `--force-device-scale-factor=${dpr}`,
          "--window-position=0,0",
          ...(software ? ["--enable-unsafe-webgpu", "--use-webgpu-adapter=swiftshader"] : []),
        ],
      }),
});
const context = await browser.newContext({
  locale: "en-US",
  viewport: VIEWPORT,
  deviceScaleFactor: dpr,
  serviceWorkers: "block",
});
/** What makes a report worthless, and what the app does worse than it should. */
const problems = [];
const misses = [];

async function open(address) {
  const page = await context.newPage();
  page.on("pageerror", (error) => problems.push(String(error)));
  page.on("console", (message) => {
    if (message.type() === "error") {
      problems.push(message.text());
    }
  });
  await page.goto(address);
  const session = isFirefox ? null : await context.newCDPSession(page);
  if (session && slowdown > 1) {
    await session.send("Emulation.setCPUThrottlingRate", { rate: slowdown });
  }
  /** Bytes of the page's JavaScript objects after collecting the garbage, where the browser says so. */
  const heapBytes = async () => {
    if (!session) {
      return null;
    }
    await session.send("Performance.enable");
    await session.send("HeapProfiler.collectGarbage");
    const { metrics } = await session.send("Performance.getMetrics");
    return metrics.find(({ name }) => name === "JSHeapUsedSize")?.value ?? null;
  };
  return { page, heapBytes };
}

const bench = await open(`${pages.address}/bench.html`);
await bench.page.waitForFunction(() => window.__bench !== undefined);
const environment = await bench.page.evaluate(() => window.__bench.environment());
const adapter = await bench.page.evaluate(() => window.__bench.b13.adapter());
const facts = machine();
const displays = facts.system?.displays ?? [];
const refreshHz = matchingRate(environment.frameInterval.p50, refreshRates(displays));
console.log(
  `${browserName} ${browser.version()}${headless ? ", headless" : ""}${slowdown > 1 ? `, main thread slowed ${slowdown}x` : ""}, DPR ${environment.devicePixelRatio}, ${environment.viewport.width}x${environment.viewport.height}`,
);
console.log(
  `idle frames every ${environment.frameInterval.p50.toFixed(2)} ms; displays: ${displays.join("; ") || "unknown"}; adapter: ${adapter ? `${adapter.vendor} ${adapter.architecture} ${adapter.description}${adapter.software ? " (software)" : ""}` : "none"}`,
);
if (environment.devicePixelRatio !== dpr) {
  problems.push(`there are ${environment.devicePixelRatio} device pixels to a CSS pixel, not ${dpr}`);
}
const renderers = RENDERERS.filter((renderer) => renderer === "canvas2d" || adapter);
if (renderers.length < RENDERERS.length) {
  problems.push("there is no WebGPU adapter");
}

/**
 * Whether frames still come when they should: a browser stops drawing a window
 * that is covered or minimised, and nothing else would say so.
 */
async function checkIdle(page) {
  // The app was in front of it.
  await page.bringToFront();
  const idle = await page.evaluate(() => window.__bench.environment());
  if (idle.visibility !== "visible") {
    problems.push("the window is not visible");
  }
  return idle.frameInterval.p50;
}

const ms = (value) => value.toFixed(2).padStart(6);
const describe = ({ work, interval, frames, framesOver60Hz, placeholderFrames }) =>
  `work p50 ${ms(work.p50)} p95 ${ms(work.p95)} p99 ${ms(work.p99)} max ${ms(work.max)} ms; frames every ${ms(interval.p50)} ms, p99 ${ms(interval.p99)}; ${framesOver60Hz} of ${frames} over 16.7 ms, ${placeholderFrames} with placeholders`;

// --- the renderers by themselves -------------------------------------------

async function synthetic() {
  const runs = new Map();
  for (let round = 0; round < rounds; round++) {
    for (const one of cases) {
      // Whoever comes first in one round comes last in the next.
      const turns = round % 2 === 0 ? renderers : renderers.toReversed();
      for (const renderer of turns) {
        const options = { ...one, renderer, seconds, warmupSeconds: WARMUP_SECONDS, software };
        const result = await bench.page.evaluate((options) => window.__bench.b13.run(options), options);
        const key = JSON.stringify([one, renderer]);
        runs.set(key, [...(runs.get(key) ?? []), result]);
      }
    }
  }
  return cases.flatMap((one) =>
    renderers.map((renderer) => {
      const made = runs.get(JSON.stringify([one, renderer]));
      const [first] = made;
      const last = made.at(-1);
      const frames = summarizeFrames(made);
      const edits = first.edits && {
        frames: made.reduce((sum, run) => sum + run.edits.frames, 0),
        excessive: made.reduce((sum, run) => sum + run.edits.excessive, 0),
        unbuilt: made.reduce((sum, run) => sum + run.edits.unbuilt, 0),
        mostBands: Math.max(...made.map((run) => run.edits.mostBands)),
        bandsInView: Math.max(...made.map((run) => run.edits.bandsInView)),
      };
      const label = `${one.size} ${one.motion}, ${one.content}, ${renderer}`;
      if (edits?.excessive > 0) {
        problems.push(`${label}: ${edits.excessive} of ${edits.frames} edits had more built than the bands of the edited page (${edits.mostBands} of ${edits.bandsInView})`);
      }
      if (edits?.unbuilt > 0) {
        problems.push(`${label}: ${edits.unbuilt} of ${edits.frames} edits had nothing built, so nothing of them was seen`);
      }
      const least = one.motion === "zoom" ? 0 : one.size === "10k" ? LEAST_CELLS : 1000;
      if (!made.every((run) => run.cells.min >= least)) {
        problems.push(`${label}: fewer than ${least} cells were drawn`);
      }
      console.log(`  ${label.padEnd(34)} ${String(first.cells.p50).padStart(6)} cells  ${describe(frames)}`);
      return {
        ...one,
        renderer,
        width: first.width,
        height: first.height,
        zoom: first.zoom,
        cells: first.cells,
        ...frames,
        // Each round by itself: what its neighbours are within is noise.
        rounds: made.map((run) => {
          const { p50, p95, p99, max } = summarize(run.work);
          return { p50, p95, p99, max };
        }),
        cold: first.cold,
        edits,
        counters: last.counters,
      };
    }),
  );
}

/** What the GPU takes for a frame, measured apart: timing it costs the main thread something. */
async function gpuTimes() {
  // Tried in Chromium alone: an error there would be the device's end, and the report's.
  if (!adapter?.timestamps || isFirefox) {
    return [];
  }
  const times = [];
  for (const size of SIZES) {
    for (const motion of ["scroll", "jump"]) {
      const one = { size, motion, content: "distinct" };
      const options = { ...one, renderer: "webgpu", seconds, warmupSeconds: WARMUP_SECONDS, software, timeGpu: true };
      const { gpu } = await bench.page.evaluate((options) => window.__bench.b13.run(options), options);
      if (gpu) {
        times.push({ ...one, gpu });
        console.log(`  ${`${size} ${motion}`.padEnd(34)} GPU p50 ${ms(gpu.p50)} p99 ${ms(gpu.p99)} max ${ms(gpu.max)} ms, ${gpu.count} passes`);
      }
    }
  }
  return times;
}

// --- the app ---------------------------------------------------------------

/** Scrolls the app's grid through its hooks and records what it reports of every frame. */
async function scrollApp({ renderer, seconds, warmupSeconds, screensPerSecond }) {
  const hooks = window.__reticula;
  const nextFrame = () => new Promise((resolve) => requestAnimationFrame(resolve));
  const work = [];
  const interval = [];
  const cells = [];
  let placeholderFrames = 0;
  let otherFrames = 0;
  let warm = null;
  let top = 0;
  let { frames: drawn, renderer: before } = hooks.stats();
  const started = await nextFrame();
  for (let last = started; ; ) {
    const now = await nextFrame();
    const elapsed = now - started;
    // The grid has drawn this frame already: its callback was asked for first.
    const stats = hooks.stats();
    if (warm === null && elapsed >= warmupSeconds * 1000) {
      warm = stats;
    } else if (warm !== null) {
      if (stats.frames !== drawn + 1) {
        throw new Error(`the grid drew ${stats.frames - drawn} frames in one`);
      }
      work.push(stats.frameWork.last);
      interval.push(now - last);
      cells.push(stats.cellsDrawn);
      // Bars stand for cells whose pages are missing, and with WebGPU for texts
      // that the frame had no time for.
      const bars = (stats.renderer.counters?.deferredFrames ?? 0) - (before.counters?.deferredFrames ?? 0);
      placeholderFrames += stats.missingCells > 0 || bars > 0 ? 1 : 0;
      otherFrames += stats.renderer.kind === renderer ? 0 : 1;
    }
    drawn = stats.frames;
    before = stats.renderer;
    if (elapsed >= (warmupSeconds + seconds) * 1000) {
      return { work, interval, cells, placeholderFrames, otherFrames, warm, end: stats };
    }
    const screen = (stats.visible.endRow - stats.visible.firstRow) * stats.metrics.rowHeight;
    top += (screensPerSecond * screen * (now - last)) / 1000;
    if (top >= stats.scrollLimits.top) {
      throw new Error("the workbook is too short to scroll for so long");
    }
    hooks.scrollTo({ top });
    last = now;
  }
}

async function product(renderer, directory) {
  // The app opens with the last workbook, which is the other renderer's: a
  // title of its own tells this one from it.
  const title = `wide-${renderer}`;
  const csv = join(directory, `${title}.csv`);
  writeFileSync(csv, csvOf(SHEET.rows, SHEET.cols));
  const { page, heapBytes } = await open(`${site.address}/app?hooks&renderer=${renderer}`);
  await page.bringToFront();
  // The engine says that it is ready where there is a document to show.
  const ready = (rows) =>
    page.waitForFunction(
      (rows) => {
        const stats = window.__reticula?.stats();
        return stats !== undefined && stats.sheet.rows >= rows && stats.cellsDrawn > 0 && stats.missingCells === 0;
      },
      rows,
      { timeout: 120_000 },
    );
  await ready(1);
  await page.getByTestId("workbooks-btn").click();
  await page.getByTestId("import-file-input").setInputFiles(csv);
  await page.getByTestId("workbooks-drawer").waitFor({ state: "detached", timeout: 120_000 });
  await page.waitForFunction((title) => window.__reticula?.docStatus().title === title, title);
  await ready(SHEET.rows);
  const zoomOut = page.getByTestId("zoom-out");
  for (let steps = 0; steps < 20 && (await zoomOut.getAttribute("aria-disabled")) !== "true"; steps++) {
    await zoomOut.click();
  }
  await ready(SHEET.rows);
  await page.waitForFunction((kind) => window.__reticula?.stats().renderer.kind === kind, renderer, { timeout: 60_000 }).catch(() => {});
  // The engine is done with saving the document, which nothing is to wait for later.
  await page.evaluate(() => window.__reticula.compactSnapshot());
  await page.mouse.move(VIEWPORT.width / 2, VIEWPORT.height / 2);
  const options = { renderer, seconds: productSeconds, warmupSeconds: PRODUCT_WARMUP_SECONDS, screensPerSecond: SCREENS_PER_SECOND };
  const { work, interval, cells, placeholderFrames, otherFrames, warm, end } = await page.evaluate(scrollApp, options);
  const heap = await heapBytes();
  await page.close();

  const frames = summarizeFrames([{ work, interval, placeholderFrames }]);
  const drawn = summarize(cells);
  const label = `app, ${renderer}`;
  console.log(`  ${label.padEnd(34)} ${String(drawn.p50).padStart(6)} cells  ${describe(frames)}`);
  if (end.renderer.kind !== renderer || otherFrames > 0) {
    problems.push(`${label}: another renderer drew ${otherFrames} of ${work.length} frames (in the end ${end.renderer.kind}, ${end.renderer.reason})`);
  }
  const visible = { rows: end.visible.endRow - end.visible.firstRow, cols: end.visible.endCol - end.visible.firstCol };
  if (visible.rows * visible.cols < LEAST_CELLS) {
    problems.push(`${label}: only ${visible.rows * visible.cols} cells are in view`);
  }
  // With cells of all kinds in view, fewer are drawn only where pages are missing.
  if (drawn.min < LEAST_CELLS) {
    misses.push(`${label}: a frame drew ${drawn.min} cells, fewer than ${LEAST_CELLS}`);
  }
  if (placeholderFrames > 0) {
    misses.push(`${label}: ${placeholderFrames} frames had placeholders in them`);
  }
  if (renderer === "webgpu" && frames.work.p99 > TARGETS.workP99Ms) {
    misses.push(`${label}: a p99 of ${frames.work.p99.toFixed(2)} ms of work, more than ${TARGETS.workP99Ms}`);
  }
  if (renderer === "webgpu" && frames.shareOver60Hz > TARGETS.shareOver60Hz) {
    misses.push(`${label}: ${frames.framesOver60Hz} of ${frames.frames} frames over 16.7 ms, more than ${TARGETS.shareOver60Hz * 100}%`);
  }
  return {
    renderer,
    adapter: end.renderer.adapter,
    ...VIEWPORT,
    zoom: end.zoom,
    visible,
    sheet: end.sheet,
    screensPerSecond: SCREENS_PER_SECOND,
    seconds: productSeconds,
    cells: drawn,
    ...frames,
    pagesReceived: end.pagesReceived - warm.pagesReceived,
    bytesReceived: end.bytesReceived - warm.bytesReceived,
    counters: end.renderer.counters && { warm: warm.renderer.counters, end: end.renderer.counters },
    heapBytes: heap,
  };
}

// --- the report ------------------------------------------------------------

const report = { synthetic: [], gpu: [], product: [] };
try {
  if (only !== "product") {
    console.log(`\nthe renderers by themselves, ${rounds} rounds of ${seconds} s`);
    report.synthetic = await synthetic();
    if (renderers.includes("webgpu")) {
      report.gpu = await gpuTimes();
    }
  }
  const heap = only === "product" ? null : await bench.heapBytes();
  if (only !== "synthetic") {
    console.log(`\nthe app, ${SHEET.rows} by ${SHEET.cols} cells at 25%, scrolling ${SCREENS_PER_SECOND} screens a second for ${productSeconds} s`);
    const directory = mkdtempSync(join(tmpdir(), "reticula-b13-"));
    try {
      for (const renderer of renderers) {
        report.product.push(await product(renderer, directory));
      }
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  }
  const idle = await checkIdle(bench.page);
  if (matchingRate(idle, refreshRates(displays)) !== refreshHz) {
    problems.push(`idle frames came every ${environment.frameInterval.p50.toFixed(2)} ms at first and every ${idle.toFixed(2)} ms in the end`);
  }

  const state = commit();
  const run = {
    date: new Date().toISOString(),
    ...state,
    affinity: affinity || null,
    machine: facts,
    browser: { name: browserName, version: browser.version(), headless },
    slowdown,
    dpr,
    refreshHz,
    adapter,
    environment,
    options: { rounds, seconds, warmupSeconds: WARMUP_SECONDS, productSeconds, quick, only },
    ...report,
    heapBytes: heap,
    misses,
    problems,
  };

  const own = join(root, "target/bench-render", `${browserName}${slowdown > 1 ? `-slow${slowdown}` : ""}${headless ? "-headless" : ""}.json`);
  writeReport(own, run);
  console.log(`\nreport written to ${own}`);

  const reasons = refusals({
    uncommittedChanges: state.uncommittedChanges,
    softwareAdapter: adapter?.software ?? null,
    idleIntervalMs: environment.frameInterval.p50,
    displays,
    headless,
    quick,
    published: results === PUBLISHED,
    problems: [
      ...problems,
      ...(only ? [`only the ${only} part was run`] : []),
      ...builtEarlier([join(pagesDirectory, "bench.html"), join(siteDirectory, "index.html")]),
    ],
  });
  if (!given["no-archive"]) {
    archive(results, run, "gpu", reasons, (earlier) => {
      const runs = mergeRuns(earlier?.runs ?? [], run);
      // Where the report is said to come from is where the first run does.
      const [{ date, commit, uncommittedChanges, affinity, machine, browser }] = runs;
      return { suite: "m6-gpu", date, commit, uncommittedChanges, affinity, machine, browser, runs };
    });
  }
  if (misses.length > 0) {
    console.log(`misses its targets:\n  ${misses.join("\n  ")}`);
  }
  // What slower machines and other browsers show is recorded, not held to it.
  const isMain = browserName === "chrome" && slowdown === 1;
  process.exitCode = problems.length > 0 || (isMain && misses.length > 0) ? 1 : 0;
} finally {
  await browser.close();
  pages.close();
  site.close();
}
