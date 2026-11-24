// The current build against an earlier commit's, in turns in one session: what
// drawing a thousand cells on a 2D canvas takes, and a first visit (B10).
//
//   node bench/ab.mjs [options]
//
// The baseline is checked out in a worktree under target/ab and built there,
// with its own tools' settings and the lock file's packages from pnpm's store;
// only the built files are kept, for the next time. Of the current commit,
// apps/web/dist-spikes and apps/web/dist are measured, which
// `cargo xtask bench --suite ab` builds first.
//
// Each side's own measurement page draws (the `s2` of its commit, whose renderer
// differs), with the same parameters; first visits are measured with one script,
// the current one. Every round goes baseline, current, current, baseline: how
// much the baseline differs from itself says what a difference means. In
// drawing, the baseline is measured with grey texts without kerning as well,
// which is how all renderers draw them now: what of a difference is due to that
// is reported by itself.
//
// Options:
//   --baseline <commit>  (default 5b46fff)
//   --rounds <n>         (default 3)
//   --visits <n>         first visits of each run (default 10)
//   --only <part>        draw or startup; such a report is not archived
//   --headed             draw in a window. By default there is none: it is the
//                        main thread's time that is compared
//   --affinity <mask>    pin everything to a set of logical processors
//   --quick              a short run, to see that everything works
//   --pages <dir>, --site <dir>  other builds than apps/web/dist-spikes and dist
//   --results <dir>      where to archive (default bench/results)
//   --no-archive         only write the report to target/bench-render/

import { spawnSync } from "node:child_process";
import { cpSync, existsSync, rmSync } from "node:fs";
import { delimiter, join, resolve } from "node:path";
import { parseArgs } from "node:util";
import { chromium } from "@playwright/test";
import { compareSides, matchingRate, refreshRates, refusals } from "../apps/web/src/bench/report.ts";
import { root, serve } from "../spikes/grid/serve.mjs";
import { commit, git, machine } from "../spikes/machine.mjs";
import { pin } from "../spikes/pin.mjs";
import { archive, builtEarlier, PUBLISHED, reportOf, writeReport } from "./archive.mjs";

/** The window of the measurement page, which leaves room for a thousand cells. */
const VIEWPORT = { width: 1676, height: 1036 };
/** What the current build may take of the baseline's time. */
const LIMITS = { draw: 1.15, startup: 1.05 };
const CASES = [
  ["new text every frame", { content: "unique", rowsPerFrame: 50 }],
  ["scrolling by 2 rows a frame", { content: "unique", rowsPerFrame: 2 }],
  ["repeating values, new screen every frame", { content: "repeating", rowsPerFrame: 50 }],
];
const PACE = "scrolling at 5 screens a second";
const TURNS = ["base", "current", "current", "base"];
/** In drawing, there is a third side: the baseline with its texts drawn the way the current build draws them. */
const DRAW_TURNS = ["base", "plain", "current", "current", "plain", "base"];

const { values: options } = parseArgs({
  options: {
    baseline: { type: "string", default: "5b46fff" },
    rounds: { type: "string" },
    visits: { type: "string" },
    only: { type: "string", default: "" },
    headed: { type: "boolean", default: false },
    affinity: { type: "string", default: "" },
    quick: { type: "boolean", default: false },
    pages: { type: "string", default: join(root, "apps/web/dist-spikes") },
    site: { type: "string", default: join(root, "apps/web/dist") },
    results: { type: "string", default: PUBLISHED },
    "no-archive": { type: "boolean", default: false },
  },
});
const { quick, only, affinity, headed } = options;
const rounds = Number(options.rounds ?? (quick ? 1 : 3));
const visits = Number(options.visits ?? (quick ? 2 : 10));
if (!(rounds > 0 && visits > 0) || !["", "draw", "startup"].includes(only)) {
  console.error("--rounds and --visits take positive numbers, --only draw or startup");
  process.exit(2);
}
const baseline = git(["rev-parse", "--short", `${options.baseline}^{commit}`]);
if (!baseline) {
  console.error(`no commit ${options.baseline}`);
  process.exit(2);
}

// --- the baseline's build ----------------------------------------------------

function run(command, args, cwd) {
  console.log(`$ ${command} ${args.join(" ")}`);
  // A script on Windows, which only the command interpreter can start.
  const isScript = process.platform === "win32" && command === "pnpm";
  const { status, error } = spawnSync(isScript ? "cmd" : command, isScript ? ["/C", command, ...args] : args, {
    cwd,
    stdio: "inherit",
    env: { ...process.env, PATH: `${join(root, ".tools/bin")}${delimiter}${process.env.PATH}` },
  });
  if (error || status !== 0) {
    throw new Error(`${command} ${args[0]} failed (${error ?? status})`);
  }
}

/** The directory with the baseline's `dist` and `dist-spikes`, built if it is not there. */
function buildBaseline() {
  const built = join(root, "target/ab", `${baseline}-build`);
  if (existsSync(join(built, "dist-spikes/bench.html"))) {
    return built;
  }
  const worktree = join(root, "target/ab", baseline);
  run("git", ["worktree", "add", "--detach", worktree, baseline], root);
  try {
    run("pnpm", ["install", "--offline", "--frozen-lockfile"], worktree);
    run("cargo", ["run", "--locked", "--package", "xtask", "--", "build"], worktree);
    for (const name of ["dist", "dist-spikes"]) {
      cpSync(join(worktree, "apps/web", name), join(built, name), { recursive: true });
    }
  } finally {
    // Not left to git: on Windows, it takes the links between packages for directories.
    for (const name of ["node_modules", "apps/web/node_modules", "packages/grid/node_modules", "packages/protocol/node_modules", "target"]) {
      rmSync(join(worktree, name), { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    }
    run("git", ["worktree", "remove", "--force", worktree], root);
  }
  return built;
}

const built = buildBaseline();
const builds = {
  base: { pages: join(built, "dist-spikes"), site: join(built, "dist") },
  current: { pages: resolve(options.pages), site: resolve(options.site) },
};
// The baseline is pinned to nothing while it is built.
if (affinity) {
  pin(affinity);
}
const seconds = {
  draw: only === "startup" ? 0 : rounds * DRAW_TURNS.length * (quick ? 6 : 23),
  startup: only === "draw" ? 0 : rounds * TURNS.length * visits * 2.5,
};
console.log(`about ${Math.ceil((seconds.draw + seconds.startup) / 60)} min`);

const facts = machine();
const displays = facts.system?.displays ?? [];
const problems = [];
const ms = (value) => value.toFixed(2).padStart(8);
const percent = (value) => `${(value * 100).toFixed(1)}%`;

/** The median of every run of both sides by a label, compared. */
function compare(runs, limit) {
  return Object.keys(runs.base[0]).map((label) => {
    const sides = compareSides(
      runs.base.map((one) => one[label]),
      runs.current.map((one) => one[label]),
    );
    console.log(
      `  ${label.padEnd(46)} ${ms(sides.baseMedian)} → ${ms(sides.currentMedian)} ms, ${sides.ratio.toFixed(3)} times (${sides.ratio <= limit ? "within" : "EXCEEDS"} ${limit}); the baseline against itself ${sides.sameRatio.toFixed(3)}, within ${percent(sides.baseSpread)}`,
    );
    return { label, limit, ...sides };
  });
}

// --- drawing a thousand cells ------------------------------------------------

/** On a canvas with alpha, texts have no coloured edges. */
function drawPlainTexts() {
  const { getContext } = HTMLCanvasElement.prototype;
  HTMLCanvasElement.prototype.getContext = function (kind, options) {
    if (kind !== "2d") {
      return getContext.call(this, kind, options);
    }
    const context = getContext.call(this, kind, { ...options, alpha: true });
    return context && Object.assign(context, { fontKerning: "none" });
  };
}

let environment = null;
let browserVersion = "";

async function measureDrawing() {
  const base = await serve(0, { site: builds.base.pages });
  const servers = { base, plain: base, current: await serve(0, { site: builds.current.pages }) };
  const browser = await chromium.launch({ headless: !headed, channel: "chrome" });
  browserVersion = browser.version();
  const runs = { base: [], plain: [], current: [] };
  try {
    const context = await browser.newContext({ locale: "en-US", viewport: VIEWPORT, deviceScaleFactor: 1 });
    for (let turn = 0; turn < rounds * DRAW_TURNS.length; turn++) {
      const side = DRAW_TURNS[turn % DRAW_TURNS.length];
      const page = await context.newPage();
      page.on("pageerror", (error) => problems.push(`${side}: ${error}`));
      if (side === "plain") {
        await page.addInitScript(drawPlainTexts);
      }
      await page.goto(`${servers[side].address}/bench.html`);
      await page.waitForFunction(() => window.__bench !== undefined);
      environment ??= await page.evaluate(() => window.__bench.environment());
      const call = (method, argument) => page.evaluate(([method, argument]) => window.__bench.s2[method](argument), [method, argument]);
      const pace = await call("pace", { content: "unique", screensPerSecond: 5, seconds: quick ? 2 : 10, warmupSeconds: 1 });
      const work = { [PACE]: pace.draw.p95 };
      for (const [label, parameters] of CASES) {
        const { draw, cells } = await call("draw", { ...parameters, frames: quick ? 120 : 600, warmup: 60 });
        work[label] = draw.p95;
        if (cells.p50 !== 1000 && !(cells.p50 > 1000 && cells.p50 <= 1020)) {
          problems.push(`${side}: ${cells.p50} cells were drawn, not a thousand`);
        }
      }
      runs[side].push(work);
      console.log(`  run ${turn + 1}, ${side.padEnd(7)} p95 ${Object.values(work).map(ms).join("")}`);
      await page.close();
    }
  } finally {
    await browser.close();
    servers.base.close();
    servers.current.close();
  }
  console.log("\nmedians of the p95 of drawing (the main thread's time)");
  return compare(runs, LIMITS.draw).map((sides) => {
    const plain = runs.plain.map((one) => one[sides.label]);
    const texts = compareSides(sides.base, plain);
    const rest = compareSides(plain, sides.current);
    console.log(`  ${sides.label}: grey texts without kerning ${texts.ratio.toFixed(3)} times, everything else ${rest.ratio.toFixed(3)}`);
    return { ...sides, plain, plainMedian: texts.currentMedian, textsRatio: texts.ratio, restRatio: rest.ratio };
  });
}

// --- a first visit -----------------------------------------------------------

function measureStartup() {
  const runs = { base: [], current: [] };
  for (let turn = 0; turn < rounds * TURNS.length; turn++) {
    const side = TURNS[turn % TURNS.length];
    const out = join(root, `target/bench-render/ab-startup-${turn}.json`);
    const args = ["--site", builds[side].site, "--runs", String(visits), ...(affinity ? ["--affinity", affinity] : [])];
    const report = reportOf("spikes/startup/measure.mjs", args, out, ["ignore", "ignore", "inherit"]);
    problems.push(...report.problems.map((problem) => `${side}: ${problem}`));
    const { shell, firstScreen } = report.results;
    runs[side].push({ "shell, p50": shell.p50, "first screen, p50": firstScreen.p50 });
    console.log(`  run ${turn + 1}, ${side.padEnd(7)} shell ${ms(shell.p50)} ms, first screen ${ms(firstScreen.p50)} ms`);
  }
  console.log("\nmedians of the p50 of a first visit");
  return compare(runs, LIMITS.startup);
}

// --- the report --------------------------------------------------------------

if (only !== "startup") {
  console.log(`\ndrawing a thousand cells, ${baseline} and the current build`);
}
const draw = only === "startup" ? [] : await measureDrawing();
if (only !== "draw") {
  console.log(`\na first visit, ${visits} at a time`);
}
const startup = only === "draw" ? [] : measureStartup();

const report = {
  suite: "m6-ab",
  date: new Date().toISOString(),
  ...commit(),
  baseline,
  affinity: affinity || null,
  machine: facts,
  browser: { name: "chrome", version: browserVersion, headless: !headed },
  environment,
  refreshHz: environment && matchingRate(environment.frameInterval.p50, refreshRates(displays)),
  options: { rounds, visits, turns: TURNS, drawTurns: DRAW_TURNS, quick, only },
  draw,
  startup,
  problems,
};
const own = join(root, "target/bench-render/ab.json");
writeReport(own, report);
console.log(`\nreport written to ${own}`);

const results = resolve(options.results);
const reasons = refusals({
  uncommittedChanges: report.uncommittedChanges,
  softwareAdapter: null,
  idleIntervalMs: environment?.frameInterval.p50 ?? Number.NaN,
  displays,
  // Both sides are measured the same way, and what is compared needs no display.
  headless: false,
  quick,
  published: results === PUBLISHED,
  problems: [
    ...problems,
    ...(only ? [`only the ${only} part was run`] : []),
    ...builtEarlier([join(builds.current.pages, "bench.html"), join(builds.current.site, "index.html")]),
  ],
});
if (!options["no-archive"]) {
  archive(results, report, "ab", reasons, () => report);
}
const exceeded = [...draw, ...startup].filter(({ ratio, limit }) => !(ratio <= limit));
process.exitCode = problems.length > 0 || exceeded.length > 0 ? 1 : 0;
