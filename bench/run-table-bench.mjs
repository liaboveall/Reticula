// B14: data tables of ten million rows, in the app.
//
//   node bench/run-table-bench.mjs [options]
//
// Files that are made up (bench/table-data.mjs) are imported by way of the app's
// dialog, and what comes of them is sorted, filtered, added up and scrolled by
// way of its hooks, in a browser with a profile of its own. The worker behind
// tables is served with a few lines before its own that take the time of every
// call of its module.
//
// A report is written to target/bench-tables/ in any case, and archived if
// nothing is wrong with it: not with changes that are not committed, an emulated
// GPU, frames that do not come at the display's rate, errors on a page, or a run
// that is shorter than it is meant to be. Runs of the same commit in other
// browsers go into the same archive.
//
// `cargo xtask bench --suite table` builds first and then runs this.
//
// Options:
//   --browser <name>   chrome (the installed Google Chrome, the default),
//                      chromium or firefox (the builds Playwright downloads)
//   --headless         no window. For trying things out: a headless browser has
//                      no display, and Firefox has no GPU then
//   --quick            a million rows and one round, to see that everything works
//   --rows <n>         other rows than 10,000,000 (such a report is not archived)
//   --rounds <n>       rounds of everything that is not scrolling (default 3)
//   --seconds <s>      how long is scrolled (default 8)
//   --only <parts>     some of import, query, scroll, hard, stress, with commas
//                      between them; such a report is not archived
//   --stress           also fifty million rows, for which there have to be 12 GB
//   --duckdb           also DuckDB-WASM with the same file in the same session, a
//                      round of it after each of ours. The results are written to
//                      target/bench-tables/ and nowhere else
//   --software         an emulated GPU (Chromium only), where there is no other
//   --affinity <mask>  pin this process, and so the browser, to a set of logical
//                      processors: a hexadecimal mask
//   --pages <dir>, --site <dir>  other builds than apps/web/dist-spikes and dist
//   --results <dir>    where to archive (default bench/results)
//   --no-archive       only write the report to target/bench-tables/

import { existsSync, mkdirSync, rmSync, statfsSync, statSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { extname, join, resolve } from "node:path";
import { parseArgs } from "node:util";
import { chromium, firefox } from "@playwright/test";
import { matchingRate, mergeRuns, refreshRates, refusals, summarizeFrames } from "../apps/web/src/bench/report.ts";
import { Random, summarize } from "../apps/web/src/bench/stats.ts";
import { root, serve } from "../spikes/grid/serve.mjs";
import { commit, machine } from "../spikes/machine.mjs";
import { pin } from "../spikes/pin.mjs";
import { serve as serveMounts } from "../spikes/tables/serve.mjs";
import { archive, builtEarlier, PUBLISHED, writeReport } from "./archive.mjs";
import { COLUMNS, copyWithSurprise, writeHard, writeOrders } from "./table-data.mjs";
import { EXTENSION, MODULES } from "./table-diff/duckdb.mjs";

const VIEWPORT = { width: 1920, height: 1080 };
const ROWS = 10_000_000;
const STRESS_ROWS = 50_000_000;
/** The bytes of a row, about, and of what else is needed for it. */
const ROW_BYTES = 99;
const PARTS = ["import", "query", "scroll", "hard", "stress"];
const RENDERERS = ["canvas2d", "webgpu"];
const SCREENS_PER_SECOND = 5;
/** Where the first mebibyte, which the types are guessed from, is over for certain. */
const SURPRISE_AT = 2 << 20;
const DOC = "bench-tables";
/** What it is to come to on the reference machine, in Chrome, with the rows it is meant for. */
const TARGETS = {
  importOnceMs: 10_000,
  importTwiceRatio: 1.8,
  fileBytes: 215_000_000,
  frameGapMs: 50,
  workP99Ms: 8,
  jumpP95Ms: 100,
  sortNumberMs: 300,
  sortTextMs: 1_000,
  sortTwoMs: 600,
  filterMs: 60,
  filterTextMs: 250,
  listMs: 300,
  summaryMs: 400,
  wasmBytes: 400_000_000,
  tripP95Ms: 50,
  stepMs: 30,
  hardImportMs: 60_000,
  hardSortMs: 20_000,
  refusalMs: 1_000,
};

// --- options ---------------------------------------------------------------

function fail(message) {
  console.error(message);
  process.exit(2);
}

const { values: given } = parseArgs({
  options: {
    browser: { type: "string", default: "chrome" },
    headless: { type: "boolean", default: false },
    quick: { type: "boolean", default: false },
    rows: { type: "string" },
    rounds: { type: "string" },
    seconds: { type: "string" },
    only: { type: "string", default: "" },
    stress: { type: "boolean", default: false },
    duckdb: { type: "boolean", default: false },
    software: { type: "boolean", default: false },
    affinity: { type: "string", default: "" },
    pages: { type: "string", default: join(root, "apps/web/dist-spikes") },
    site: { type: "string", default: join(root, "apps/web/dist") },
    results: { type: "string", default: PUBLISHED },
    "no-archive": { type: "boolean", default: false },
  },
});
const { browser: browserName, headless, quick, stress, software, affinity } = given;
const rows = Number(given.rows ?? (quick ? 1_000_000 : ROWS));
const rounds = Number(given.rounds ?? (quick ? 1 : 3));
const seconds = Number(given.seconds ?? (quick ? 2 : 8));
const only = given.only === "" ? [] : given.only.split(",");
if (!["chrome", "chromium", "firefox"].includes(browserName)) {
  fail(`unknown browser ${browserName}`);
}
if (only.some((part) => !PARTS.includes(part))) {
  fail(`--only takes ${PARTS.join(", ")}`);
}
if (![rows, rounds, seconds].every((value) => value > 0)) {
  fail("--rows, --rounds and --seconds take positive numbers");
}
const isFirefox = browserName === "firefox";
if (isFirefox && software) {
  fail("--software works in Chromium only");
}
const has = (part) => (only.length === 0 ? part !== "stress" || stress : only.includes(part));
const isComplete = !quick && rows === ROWS && PARTS.slice(0, 4).every(has);
const results = resolve(given.results);
const pagesDirectory = resolve(given.pages);
const siteDirectory = resolve(given.site);
const work = join(root, "target/bench-tables");
mkdirSync(work, { recursive: true });

const needed = (has("stress") ? 2.2 * STRESS_ROWS : 0) * ROW_BYTES + 4 * rows * ROW_BYTES;
const free = statfsSync(work);
if (free.bavail * free.bsize < needed) {
  fail(`${work} needs ${(needed / 2 ** 30).toFixed(0)} GB, of which ${((free.bavail * free.bsize) / 2 ** 30).toFixed(0)} are free`);
}

console.log(`data tables of ${rows.toLocaleString("en-US")} rows in ${browserName}${headless ? "" : ", in a window that must stay in view"}`);

// The browser is started by this process and inherits the setting.
if (affinity) {
  pin(affinity);
}

// --- data ------------------------------------------------------------------

/** Files that are made once, and are there for the next time. */
function dataFile(name, write) {
  const path = join(work, name);
  if (!existsSync(path)) {
    console.log(`  making ${name}`);
    try {
      write(path);
    } catch (error) {
      rmSync(path, { force: true });
      throw error;
    }
  }
  return path;
}

const orders = dataFile(`orders-${rows}.csv`, (path) => writeOrders(path, rows));
const textBytes = statSync(orders).size;

// --- the browser -----------------------------------------------------------

/**
 * Put before the worker's script: takes the time of every call of the module
 * from outside, of which none may be long.
 */
const STEPS = `{
  const steps = (self.__steps = { calls: 0, longest: 0, over8: 0, over30: 0 });
  let depth = 0;
  const time = (call) => (...args) => {
    if (depth > 0) return call(...args);
    depth++;
    const started = performance.now();
    try {
      return call(...args);
    } finally {
      const ms = performance.now() - started;
      depth--;
      steps.calls++;
      steps.longest = Math.max(steps.longest, ms);
      steps.over8 += ms > 8;
      steps.over30 += ms > 30;
    }
  };
  for (const name of ["instantiate", "instantiateStreaming"]) {
    const native = WebAssembly[name];
    WebAssembly[name] = async (...args) => {
      const { instance, module } = await native.apply(WebAssembly, args);
      const exports = {};
      for (const [key, value] of Object.entries(instance.exports)) {
        exports[key] = typeof value === "function" ? time(value) : value;
      }
      return { instance: { exports }, module };
    };
  }
}
`;

const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript", ".css": "text/css", ".wasm": "application/wasm", ".json": "application/json", ".svg": "image/svg+xml" };

/** The app, with the worker's script as it is measured. */
async function serveApp() {
  const server = createServer(async (request, response) => {
    const { pathname } = new URL(request.url, "http://localhost");
    const path = extname(pathname) === "" ? "/index.html" : pathname;
    let body;
    try {
      body = await readFile(join(siteDirectory, path));
    } catch {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, {
      "content-type": TYPES[extname(path)] ?? "application/octet-stream",
      "cache-control": "no-store",
      // Cross-origin isolation: browsers give an isolated page a finer clock.
      "cross-origin-opener-policy": "same-origin",
      "cross-origin-embedder-policy": "require-corp",
    });
    response.end(/\/table-worker-[^/]+\.js$/.test(path) ? STEPS + body : body);
  });
  await new Promise((ready) => server.listen(0, "127.0.0.1", ready));
  return { address: `http://localhost:${server.address().port}`, close: () => server.close() };
}

const pages = await serve(0, { site: pagesDirectory });
const site = await serveApp();
const profile = join(work, `profile-${process.pid}`);
const context = await (isFirefox ? firefox : chromium).launchPersistentContext(profile, {
  headless,
  ...(browserName === "chrome" ? { channel: "chrome" } : {}),
  // The headless shell has no GPU; the full browser's headless mode has.
  ...(browserName === "chromium" ? { channel: "chromium" } : {}),
  ...(isFirefox
    ? { firefoxUserPrefs: { "layout.css.devPixelsPerPx": "1" } }
    : {
        args: [
          "--force-device-scale-factor=1",
          "--window-position=0,0",
          ...(software ? ["--enable-unsafe-webgpu", "--use-webgpu-adapter=swiftshader"] : []),
        ],
      }),
  locale: "en-US",
  viewport: VIEWPORT,
  deviceScaleFactor: 1,
  serviceWorkers: "block",
});
const version = context.browser()?.version() ?? "";
await context.addInitScript({ path: join(root, "bench/table-page.js") });
/** What makes a report worthless, and what the app does worse than it should. */
const problems = [];
const misses = [];

async function open(address) {
  const page = await context.newPage();
  page.setDefaultTimeout(120_000);
  page.on("pageerror", (error) => problems.push(String(error)));
  page.on("console", (message) => {
    if (message.type() === "error") {
      problems.push(message.text());
    }
  });
  await page.goto(address);
  return page;
}

const bench = await open(`${pages.address}/bench.html`);
await bench.waitForFunction(() => window.__bench !== undefined);
const environment = await bench.evaluate(() => window.__bench.environment());
const adapter = await bench.evaluate(() => window.__bench.b13.adapter());
const facts = machine();
const displays = facts.system?.displays ?? [];
const refreshHz = matchingRate(environment.frameInterval.p50, refreshRates(displays));
console.log(
  `${browserName} ${version}${headless ? ", headless" : ""}, ${environment.viewport.width}x${environment.viewport.height}; idle frames every ${environment.frameInterval.p50.toFixed(2)} ms; displays: ${displays.join("; ") || "unknown"}; adapter: ${adapter ? `${adapter.vendor} ${adapter.architecture}${adapter.software ? " (software)" : ""}` : "none"}`,
);
const renderers = RENDERERS.filter((renderer) => renderer === "canvas2d" || adapter);
if (renderers.length < RENDERERS.length) {
  problems.push("there is no WebGPU adapter");
}

// --- the app ---------------------------------------------------------------

const ms = (value) => `${value.toFixed(value < 100 ? 1 : 0)} ms`;
const mib = (bytes) => `${(bytes / 2 ** 20).toFixed(0)} MiB`;
const p50 = (summary) => summary.p50;

async function openApp(renderer) {
  const page = await open(`${site.address}/app?hooks&doc=${DOC}${renderer ? `&renderer=${renderer}` : ""}`);
  await page.bringToFront();
  // Not every frame, of which there are none while the tab is not in front.
  await page.waitForFunction(() => window.__reticula?.docStatus().engineStatus === "ready", undefined, { polling: 100 });
  return page;
}

const call = (page, name, ...args) => page.evaluate(([fn, rest]) => window.__b14[fn](...rest), [name, args]);

/** What the module's calls took since the last time. */
async function takeSteps(page) {
  const worker = page.workers().find((one) => /table-worker/.test(one.url()));
  if (!worker) {
    return null;
  }
  return worker.evaluate(() => {
    const steps = { ...self.__steps };
    Object.assign(self.__steps, { calls: 0, longest: 0, over8: 0, over30: 0 });
    return steps;
  });
}

const steps = {};
async function noteSteps(page, phase) {
  const taken = await takeSteps(page);
  if (taken) {
    const before = steps[phase];
    steps[phase] = before
      ? {
          calls: before.calls + taken.calls,
          longest: Math.max(before.longest, taken.longest),
          over8: before.over8 + taken.over8,
          over30: before.over30 + taken.over30,
        }
      : taken;
  }
}

async function importFile(page, path, name, phase) {
  // The dialog asks for a file at once, for which there is nobody.
  page.once("filechooser", () => {});
  await page.keyboard.press("ControlOrMeta+/");
  await page.getByTestId("command-importTable").click();
  await page.getByTestId("table-import-input").setInputFiles(path);
  await page.getByTestId("table-import-name").fill(name);
  await page.getByTestId("table-import-preview").waitFor();
  await takeSteps(page);
  const made = await call(page, "importTable", name);
  await noteSteps(page, phase);
  const { gaps, longTasks, ...rest } = made;
  return {
    ...rest,
    longestFrameGapMs: Math.max(...gaps),
    framesOver50Ms: gaps.filter((gap) => gap > 50).length,
    longTasks,
  };
}

async function remove(page, id) {
  await page.evaluate(async (table) => {
    const client = window.__reticula.table()?.client();
    window.__reticula.showTable(null);
    await client?.delete(table);
  }, id);
}

/** Imports a file several times, and keeps the table of the last time. */
async function imports(page, path, name, times, phase) {
  const made = [];
  for (let round = 0; round < times; round++) {
    if (round > 0) {
      await remove(page, made.at(-1).id);
    }
    made.push(await importFile(page, path, name, phase));
  }
  const memory = await call(page, "memory");
  const summary = {
    ms: summarize(made.map((one) => one.ms)),
    shownMs: summarize(made.map((one) => one.shownMs)),
    longestFrameGapMs: Math.max(...made.map((one) => one.longestFrameGapMs)),
    framesOver50Ms: made.reduce((sum, one) => sum + one.framesOver50Ms, 0),
    longTasksMs: made[0].longTasks && made.flatMap((one) => one.longTasks),
    fileBytes: made.at(-1).bytes,
    rows: made.at(-1).rows,
    wasmBytes: memory.total,
  };
  console.log(
    `  ${name.padEnd(28)} ${ms(summary.ms.p50).padStart(10)} (${made.map((one) => ms(one.ms)).join(", ")}), ${(summary.fileBytes / 1e6).toFixed(1)} MB, longest frame gap ${ms(summary.longestFrameGapMs)}, ${mib(memory.total)} of WASM`,
  );
  return { id: made.at(-1).id, summary };
}

const key = (column, descending = false) => ({ column, descending });
const sorted = (...order) => ({ filters: [], order });
const where = (column, condition) => ({ filters: [{ column, condition }], order: [] });

/** Every question, several times, each time of a table that was just opened. */
async function questions(page, id, repeats, phase) {
  const opened = [];
  const timesOf = async (label, run) => {
    const times = [];
    let last;
    for (let round = 0; round < repeats; round++) {
      opened.push(await call(page, "open", id));
      last = await run();
      times.push(last.ms);
      if (last.isShown === false) {
        problems.push(`${label} was refused: ${last.refused}`);
      }
    }
    await noteSteps(page, phase);
    const summary = summarize(times);
    console.log(`  ${label.padEnd(40)} ${ms(summary.p50).padStart(10)}${last.rows === undefined ? "" : `, ${last.rows.toLocaleString("en-US")} rows`}`);
    return summary;
  };
  const ask = (label, query) => timesOf(label, () => call(page, "ask", query));
  await call(page, "open", id);
  const { first } = await call(page, "values", COLUMNS.region);
  const east = first.find(({ text }) => text === "East").key;
  const sort = {
    numberMs: await ask("sort by a number", sorted(key(COLUMNS.amount, true))),
    textMs: await ask("sort by a text", sorted(key(COLUMNS.customer))),
    twoMs: await ask("sort by two columns", sorted(key(COLUMNS.region), key(COLUMNS.amount, true))),
  };
  const filter = {
    numberMs: await ask("filter by a number", where(COLUMNS.amount, { kind: "number", op: "greater", a: "1000" })),
    valuesMs: await ask("filter by a value", where(COLUMNS.region, { kind: "oneOf", keys: [east], blank: false })),
    textMs: await ask("filter by a part of a text", where(COLUMNS.customer, { kind: "contains", text: "777" })),
    listMs: await timesOf("list the values of a column", () => call(page, "values", COLUMNS.product)),
    listManyMs: await timesOf("list those of one with many", () => call(page, "values", COLUMNS.customer)),
  };
  const summaryMs = await timesOf("add up all columns", () => call(page, "summary"));
  return { openMs: summarize(opened), sort, filter, summaryMs };
}

/** The questions, pages while there is one, and one for which there is too little memory. */
async function queries(page, id) {
  const asked = await questions(page, id, rounds, "query");
  const random = new Random(14);
  const positions = Array.from({ length: 400 }, () => random.unit());
  const trips = [];
  for (let round = 0; round < rounds; round++) {
    for (const query of [sorted(key(COLUMNS.customer, true)), sorted(key(COLUMNS.region), key(COLUMNS.note))]) {
      await call(page, "open", id);
      trips.push(...(await call(page, "jumpsDuring", query, positions)).trips);
    }
  }
  await noteSteps(page, "pages while sorting");
  const tripMs = summarize(trips);
  if (trips.length === 0) {
    problems.push("no page was asked for while there was something to sort");
  }
  console.log(`  ${"pages while sorting".padEnd(40)} p50 ${ms(tripMs.p50)}, p95 ${ms(tripMs.p95)}, ${trips.length} of them`);

  // So little memory that it is refused, which is said, and nothing else is the matter.
  await call(page, "open", id);
  const { table: used, total } = await call(page, "memory");
  await page.evaluate((bytes) => window.__reticula.tableMemoryLimit(bytes), used + 1_000_000);
  const refused = await call(page, "ask", sorted(key(COLUMNS.amount)));
  await page.evaluate(() => window.__reticula.tableMemoryLimit(null));
  const again = await call(page, "ask", sorted(key(COLUMNS.amount)));
  if (refused.isShown || !again.isShown) {
    problems.push("a limit to the memory was not kept to, or outlived");
  }
  console.log(`  ${"without memory".padEnd(40)} ${ms(refused.ms).padStart(10)}: ${refused.refused}`);
  return {
    ...asked,
    tripMs,
    memoryLimit: { ms: refused.ms, said: refused.refused, works: again.isShown },
    wasmBytes: Math.max(total, (await call(page, "memory")).total),
  };
}

const VIEWS = {
  file: { filters: [], order: [] },
  filtered: where(COLUMNS.amount, { kind: "number", op: "greater", a: "1000" }),
  sorted: sorted(key(COLUMNS.amount, true)),
};

async function scrolling(id, views = VIEWS) {
  const scrolled = [];
  const jumped = [];
  for (const renderer of renderers) {
    const page = await openApp(renderer);
    await page.waitForFunction((table) => window.__reticula.tableEntries().some((one) => one.id === table), id);
    await call(page, "open", id);
    await page.mouse.move(VIEWPORT.width / 2, VIEWPORT.height / 2);
    await page
      .waitForFunction((kind) => window.__reticula.table()?.stats().renderer.kind === kind, renderer, { timeout: 60_000 })
      .catch(() => {});
    for (const [view, query] of Object.entries(views)) {
      const { isShown, refused } = await call(page, "ask", query);
      if (!isShown) {
        problems.push(`the view ${view} was refused: ${refused}`);
        continue;
      }
      const options = { renderer, seconds, screensPerSecond: SCREENS_PER_SECOND };
      // Once before, so that what is measured is not read for the first time.
      await call(page, "scroll", options);
      const { work, interval, placeholderFrames, otherFrames } = await call(page, "scroll", options);
      const label = `${renderer}, ${view}`;
      if (otherFrames > 0) {
        problems.push(`${label}: another renderer drew ${otherFrames} of ${work.length} frames`);
      }
      const frames = summarizeFrames([{ work, interval, placeholderFrames }]);
      const random = new Random(7);
      const filled = await call(page, "jumps", Array.from({ length: quick ? 20 : 60 }, () => random.unit()));
      const filledMs = summarize(filled);
      console.log(
        `  ${label.padEnd(28)} work p50 ${ms(frames.work.p50)} p99 ${ms(frames.work.p99)}; frames every ${ms(frames.interval.p50)}, p99 ${ms(frames.interval.p99)}; ${placeholderFrames} of ${frames.frames} with placeholders; jumps filled in p95 ${ms(filledMs.p95)}`,
      );
      scrolled.push({
        renderer,
        view,
        workP50Ms: frames.work.p50,
        workP95Ms: frames.work.p95,
        workP99Ms: frames.work.p99,
        ...frames,
      });
      jumped.push({ renderer, view, filledP95Ms: filledMs.p95, filledMs });
    }
    await noteSteps(page, "scroll");
    await page.close();
  }
  return { scroll: scrolled, jump: jumped };
}

/** What does not compress, imported and put in every order there is. */
async function hard(page) {
  const path = dataFile(`hard-${rows}.csv`, (file) => writeHard(file, rows));
  const { id, summary } = await imports(page, path, "hard", 1, "hard");
  const sorts = [];
  for (const [column, name] of ["whole numbers", "texts", "floats", "other floats"].entries()) {
    await call(page, "open", id);
    const { ms: took, isShown, refused } = await call(page, "ask", sorted(key(column, true)));
    sorts.push({ name, ms: took, isShown, refused });
    console.log(`  ${`sort by ${name}`.padEnd(40)} ${ms(took).padStart(10)}${isShown ? "" : `, refused: ${refused}`}`);
  }
  await noteSteps(page, "hard");
  const { total } = await call(page, "memory");
  await remove(page, id);
  return { import: summary, sorts, wasmBytes: total };
}

/** Five times as many rows. */
async function stressed() {
  const path = join(work, `orders-${STRESS_ROWS}.csv`);
  const stressRows = quick ? rows * 2 : STRESS_ROWS;
  try {
    writeOrders(path, stressRows);
    const page = await openApp();
    const { id, summary } = await imports(page, path, "stress", 1, "stress");
    await call(page, "open", id);
    const { ms: took, isShown, refused } = await call(page, "ask", sorted(key(COLUMNS.amount, true)));
    console.log(`  ${"sort by a number".padEnd(40)} ${ms(took).padStart(10)}${isShown ? "" : `, refused: ${refused}`}`);
    const { total } = await call(page, "memory");
    await noteSteps(page, "stress");
    await page.close();
    const { scroll, jump } = await scrolling(id, isShown ? VIEWS : { file: VIEWS.file });
    return { rows: stressRows, import: summary, sort: { ms: took, isShown, refused }, scroll, jump, wasmBytes: total };
  } finally {
    rmSync(path, { force: true });
  }
}

// --- DuckDB ----------------------------------------------------------------

/** The same of DuckDB-WASM: a table of its own in memory, and row numbers in an order. */
async function duckdb(duck) {
  const page = await context.newPage();
  page.setDefaultTimeout(0);
  await page.goto(`${duck.address}/duckdb.html`);
  await page.waitForFunction(() => window.s8);
  const s8 = (name, ...rest) => page.evaluate(([n, a]) => window.s8[n](...a), [name, rest]);
  const time = async (sql, name = "column") => (await s8(name, sql)).ms;
  await s8("boot", {});
  await page.locator("#file").setInputFiles(orders);
  await s8("registerFile", "in.csv");
  const numbers = (rest) => `SELECT rowid::INT FROM t ${rest}`;
  const times = { importMs: await time("CREATE TABLE t AS SELECT * FROM read_csv('in.csv')", "sql") };
  times.sortNumberMs = await time(numbers("ORDER BY amount DESC"));
  times.sortTextMs = await time(numbers("ORDER BY customer"));
  times.sortTwoMs = await time(numbers("ORDER BY region, amount DESC"));
  times.filterNumberMs = await time(numbers("WHERE amount > 1000"));
  times.filterValuesMs = await time(numbers("WHERE region = 'East'"));
  times.filterTextMs = await time(numbers("WHERE contains(customer, '777')"));
  times.listMs = await time("SELECT product, count(*) FROM t GROUP BY product ORDER BY 2 DESC LIMIT 1000", "sql");
  times.listManyMs = await time("SELECT customer, count(*) FROM t GROUP BY customer ORDER BY 2 DESC LIMIT 1000", "sql");
  const columns = ["id", "store", "quantity", "unit_price", "discount", "amount"];
  const all = [...columns, "ordered_at", "day", "region", "product", "customer", "paid", "note"];
  times.summaryMs = await time(
    `SELECT ${all.map((c) => `count(${c}), min(${c}), max(${c})`).join(", ")}, ${columns.map((c) => `sum(${c}), avg(${c})`).join(", ")}, ${["region", "product", "customer", "note"].map((c) => `count(DISTINCT ${c})`).join(", ")} FROM t`,
    "sql",
  );
  await page.close();
  return times;
}

// --- the report ------------------------------------------------------------

const part = {};
try {
  const page = await openApp();
  let id = null;
  if (has("import") || has("query") || has("scroll")) {
    console.log("\nimport");
    const once = await imports(page, orders, "orders", has("import") ? rounds : 1, "import");
    id = once.id;
    part.import = { onceMs: once.summary.ms, once: once.summary };
    part.fileBytes = once.summary.fileBytes;
    if (has("import")) {
      part.import.longestFrameGapMs = once.summary.longestFrameGapMs;
      for (const [name, label, at] of [
        ["twice", "a surprise after 2 MiB", SURPRISE_AT],
        ["last", "a surprise in the last row", Number.POSITIVE_INFINITY],
      ]) {
        const path = join(work, `orders-${rows}-${name}.csv`);
        try {
          await copyWithSurprise(orders, path, at);
          const made = await imports(page, path, label, rounds, `import ${name}`);
          part.import[`${name}Ms`] = made.summary.ms;
          part.import[name] = made.summary;
          part.import.longestFrameGapMs = Math.max(part.import.longestFrameGapMs, made.summary.longestFrameGapMs);
          await remove(page, made.id);
        } finally {
          rmSync(path, { force: true });
        }
      }
    }
  }
  if (has("query")) {
    console.log("\nquestions");
    const { wasmBytes, ...asked } = await queries(page, id);
    Object.assign(part, asked, { queryWasmBytes: wasmBytes });
  }
  if (has("hard")) {
    console.log("\nwhat does not compress");
    part.hard = await hard(page);
  }
  if (given.duckdb) {
    console.log("\nDuckDB-WASM, in turns");
    const duck = await serveMounts({ "/": join(root, "spikes/tables"), "/nm/": join(MODULES, "node_modules"), "/ext/": () => EXTENSION });
    const ours = [];
    const theirs = [];
    for (let round = 0; round < rounds; round++) {
      const { id: again, summary } = await imports(page, orders, `round ${round + 1}`, 1, "import");
      const { sort, filter, summaryMs } = await questions(page, again, 1, "query");
      ours.push({
        importMs: summary.ms.p50,
        sortNumberMs: sort.numberMs.p50,
        sortTextMs: sort.textMs.p50,
        sortTwoMs: sort.twoMs.p50,
        filterNumberMs: filter.numberMs.p50,
        filterValuesMs: filter.valuesMs.p50,
        filterTextMs: filter.textMs.p50,
        listMs: filter.listMs.p50,
        listManyMs: filter.listManyMs.p50,
        summaryMs: summaryMs.p50,
      });
      await remove(page, again);
      theirs.push(await duckdb(duck));
      await page.bringToFront();
    }
    duck.close();
    const compared = {};
    for (const of of Object.keys(ours[0])) {
      const [a, b] = [ours, theirs].map((runs) => summarize(runs.map((run) => run[of])).p50);
      compared[of] = { ours: a, duckdb: b, runs: { ours: ours.map((run) => run[of]), duckdb: theirs.map((run) => run[of]) } };
      console.log(`  ${of.padEnd(20)} ${ms(a)} against ${ms(b)}`);
      if (a >= b) {
        misses.push(`${of}: ${ms(a)}, and ${ms(b)} with DuckDB`);
      }
    }
    writeReport(join(work, `duckdb-${browserName}.json`), { date: new Date().toISOString(), ...commit(), rows, browser: { name: browserName, version, headless }, compared });
  }
  // A workbook is open in one tab at a time.
  await page.close();
  if (id !== null && has("scroll")) {
    console.log(`\nscrolling ${SCREENS_PER_SECOND} screens a second for ${seconds} s`);
    Object.assign(part, await scrolling(id));
  }
  if (has("stress")) {
    console.log("\nstress");
    part.stress = await stressed();
  }

  await bench.bringToFront();
  const idle = await bench.evaluate(() => window.__bench.environment());
  if (idle.visibility !== "visible") {
    problems.push("the window is not visible");
  }
  if (matchingRate(idle.frameInterval.p50, refreshRates(displays)) !== refreshHz) {
    problems.push(`idle frames came every ${environment.frameInterval.p50.toFixed(2)} ms at first and every ${idle.frameInterval.p50.toFixed(2)} ms in the end`);
  }

  // Of the rows it is meant for; the hard ones and the stress have targets of their own.
  const peakWasmBytes = Math.max(0, ...[part.import?.once.wasmBytes, part.import?.twice?.wasmBytes, part.import?.last?.wasmBytes, part.queryWasmBytes].filter((bytes) => bytes !== undefined));
  const longestStepMs = Math.max(0, ...Object.entries(steps).filter(([phase]) => !["hard", "stress"].includes(phase)).map(([, one]) => one.longest));
  const miss = (label, value, limit, show = ms) => {
    if (value !== undefined && value > limit) {
      misses.push(`${label}: ${show(value)}, more than ${show(limit)}`);
    }
  };
  const bytes = (value) => `${(value / 1e6).toFixed(1)} MB`;
  miss("import", part.import?.onceMs.p50, isFirefox ? 11_500 : TARGETS.importOnceMs);
  miss("import in two passes", part.import?.twiceMs?.p50, TARGETS.importTwiceRatio * (part.import?.onceMs.p50 ?? 0));
  miss("the file", part.fileBytes, TARGETS.fileBytes, bytes);
  miss("the longest time between frames", part.import?.longestFrameGapMs, TARGETS.frameGapMs);
  for (const one of part.scroll ?? []) {
    miss(`work, ${one.renderer}, ${one.view}, p99`, one.workP99Ms, TARGETS.workP99Ms);
    miss(`placeholders, ${one.renderer}, ${one.view}`, one.placeholderFrames, 0, String);
  }
  for (const one of part.jump ?? []) {
    miss(`jumps, ${one.renderer}, ${one.view}, p95`, one.filledP95Ms, TARGETS.jumpP95Ms);
  }
  miss("sort by a number", part.sort?.numberMs.p50, TARGETS.sortNumberMs);
  miss("sort by a text", part.sort?.textMs.p50, TARGETS.sortTextMs);
  miss("sort by two columns", part.sort?.twoMs.p50, TARGETS.sortTwoMs);
  miss("filter by a number", part.filter?.numberMs.p50, TARGETS.filterMs);
  miss("filter by a value", part.filter?.valuesMs.p50, TARGETS.filterMs);
  miss("filter by a part of a text", part.filter?.textMs.p50, TARGETS.filterTextMs);
  miss("list values", part.filter?.listMs.p50, TARGETS.listMs);
  miss("list many values", part.filter?.listManyMs.p50, TARGETS.listMs);
  miss("add up", part.summaryMs?.p50, TARGETS.summaryMs);
  miss("pages while sorting, p95", part.tripMs?.p95, TARGETS.tripP95Ms);
  miss("the longest step", longestStepMs, TARGETS.stepMs);
  miss("memory", peakWasmBytes, TARGETS.wasmBytes, bytes);
  miss("import of what does not compress", part.hard?.import.ms.p50, TARGETS.hardImportMs);
  for (const { name, ms: took, isShown } of part.hard?.sorts ?? []) {
    miss(`sort by ${name} that do not compress`, took, isShown ? TARGETS.hardSortMs : TARGETS.refusalMs);
  }
  if (part.stress) {
    miss("a refusal", part.stress.sort.isShown ? 0 : part.stress.sort.ms, TARGETS.refusalMs);
    for (const one of part.stress.scroll) {
      miss(`stress, work, ${one.renderer}, ${one.view}, p99`, one.workP99Ms, TARGETS.workP99Ms);
      miss(`stress, placeholders, ${one.renderer}, ${one.view}`, one.placeholderFrames, 0, String);
    }
  }

  const state = commit();
  const run = {
    date: new Date().toISOString(),
    ...state,
    affinity: affinity || null,
    machine: facts,
    browser: { name: browserName, version, headless },
    // There is nothing of the kind here, but it tells runs apart.
    slowdown: 1,
    dpr: 1,
    refreshHz,
    adapter,
    environment,
    options: { rows, rounds, seconds, quick, only, viewport: VIEWPORT },
    data: { rows, columns: 13, textBytes },
    ...part,
    peakWasmBytes,
    longestStepMs,
    steps,
    misses,
    problems,
  };
  const own = join(work, `${browserName}${headless ? "-headless" : ""}${quick ? "-quick" : ""}.json`);
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
      ...(isComplete || quick ? [] : ["it is not all of it, or of other rows"]),
      ...builtEarlier([join(pagesDirectory, "bench.html"), join(siteDirectory, "index.html")]),
    ],
  });
  if (!given["no-archive"]) {
    archive(results, run, "tables", reasons, (earlier) => {
      const runs = mergeRuns(earlier?.runs ?? [], run);
      // Where the report is said to come from is where the first run does.
      const [{ date, commit, uncommittedChanges, affinity, machine, browser, data }] = runs;
      return { suite: "m5-tables", date, commit, uncommittedChanges, affinity, machine, browser, data, runs };
    });
  }
  if (misses.length > 0) {
    console.log(`misses its targets:\n  ${misses.join("\n  ")}`);
  }
  if (problems.length > 0) {
    console.log(`problems:\n  ${problems.join("\n  ")}`);
  }
  // With fewer rows the targets mean nothing.
  process.exitCode = problems.length > 0 || (browserName === "chrome" && isComplete && misses.length > 0) ? 1 : 0;
} finally {
  await context.close();
  pages.close();
  site.close();
  rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 300 });
}
