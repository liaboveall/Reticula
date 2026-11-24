// Measures a first visit: how long the app takes to start in a browser that
// has never seen it, over a network that is held back.
//
//   node spikes/startup/measure.mjs [options]
//
// It serves the production build (apps/web/dist) and opens it again and
// again, each time in a browser started on an empty profile. The page marks
// its own moments of starting up; this reads them, with the timing of every
// request, prints the medians and writes everything to a data file.
// `cargo xtask spike s9` builds the app first and then runs this.
//
// Options:
//   --browser <name>   chrome (the installed Google Chrome, the default),
//                      chromium or firefox (the builds Playwright downloads)
//   --runs <n>         how many visits (default 20)
//   --network <name>   reference (the default: 20 Mbit/s down, a round trip
//                      of 50 ms, two round trips to set up the connection)
//                      or none
//   --connect <n>      round trips that setting up the connection costs the
//                      visit's first request, instead of the reference's two,
//                      which stand for TCP and TLS 1.3; 0 leaves it out
//   --shaping <where>  server (the default: the server holds its responses
//                      back) or protocol (the browser's own throttling, which
//                      is Chromium's alone and does not reach the request for
//                      the worker's script)
//   --encoding <name>  what the server offers: br (the default, then gzip),
//                      gzip or none
//   --slowdown <n>     throttle the page's main thread by this factor
//                      (Chromium only); the engine's worker is not slowed
//   --affinity <mask>  pin this process, and so the browser, to a set of
//                      logical processors: a hexadecimal mask
//   --headed           show the browser window
//   --locale <tag>     the browser's language (default en-US: the numbers are
//                      comparable only between visits in the same language)
//   --site <dir>       another build to serve instead of apps/web/dist, for
//                      comparing two
//   --quick            five visits, to see that everything works
//   --out <file>       the data file (default: target/spikes/startup/<name>.json,
//                      the name made of the options)

import { mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import os from "node:os";
import { dirname, join, resolve } from "node:path";
import { chromium, firefox } from "@playwright/test";
import { commit } from "../machine.mjs";
import { pin } from "../pin.mjs";
import { root, serve } from "./serve.mjs";

/**
 * The network a first visit is measured on. The upstream rate is only used
 * when the browser does the holding back; a visit's requests are a few
 * hundred bytes.
 */
const REFERENCE = { roundTripMs: 50, downBitsPerSecond: 20_000_000, upBitsPerSecond: 5_000_000, connectRoundTrips: 2 };
/** What the start-up may take, in milliseconds, at the median. */
const TARGETS = { shell: 1500, firstScreen: 2000 };

// --- options ---------------------------------------------------------------

const options = { browser: "chrome", runs: 20, path: "/app", network: "reference", connect: REFERENCE.connectRoundTrips, shaping: "server", encoding: "br", slowdown: 1, affinity: "", headed: false, locale: "en-US", site: "", quick: false, out: "" };
const given = process.argv.slice(2);
for (let index = 0; index < given.length; index++) {
  const name = given[index].replace(/^--/, "");
  if (!(name in options) || !given[index].startsWith("--")) {
    fail(`unknown option ${given[index]}`);
  }
  if (typeof options[name] === "boolean") {
    options[name] = true;
  } else {
    const value = given[++index];
    if (value === undefined) {
      fail(`--${name} needs a value`);
    }
    options[name] = typeof options[name] === "number" ? Number(value) : value;
  }
}
if (!["chrome", "chromium", "firefox"].includes(options.browser)) {
  fail(`unknown browser ${options.browser}`);
}
if (!["reference", "none"].includes(options.network) || !["server", "protocol"].includes(options.shaping) || !["br", "gzip", "none"].includes(options.encoding)) {
  fail("--network takes reference or none, --shaping server or protocol, --encoding br, gzip or none");
}
const chromiumBased = options.browser !== "firefox";
if (!chromiumBased && (options.shaping === "protocol" || options.slowdown > 1)) {
  fail("--shaping protocol and --slowdown work in Chromium only");
}
if (options.quick) {
  options.runs = Math.min(options.runs, 5);
}

function fail(message) {
  console.error(message);
  process.exit(2);
}

// The browser is started by this process and inherits the setting.
if (options.affinity) {
  pin(options.affinity);
}

// --- what is served --------------------------------------------------------

const site = options.site ? resolve(options.site) : join(root, "apps/web/dist");
const html = readFileSync(join(site, "index.html"), "utf8");
/**
 * The scripts the editor's page cannot run without, apart from the worker's:
 * the ones the document names, and the ones its inline script adds for /app.
 */
const editorScripts = new Set([
  ...[...html.matchAll(/<(?:script|link)\b[^>]*?\b(?:src|href)="\/assets\/([^"]+\.js)"/g)].map((match) => match[1]),
  ...JSON.parse(html.match(/"app":(\[[^\]]*\])/)?.[1] ?? "[]"),
].filter((name) => !name.startsWith("worker-")));
if (editorScripts.size === 0) {
  fail(`${site}/index.html names no scripts`);
}
/** The size of each file of the build: as it is, and in both compressed forms. */
function sizes(directory = site, prefix = "") {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      return sizes(path, `${prefix}${entry.name}/`);
    }
    if (/\.(br|gz)$/.test(entry.name)) {
      return [];
    }
    const size = (suffix) => {
      try {
        return statSync(path + suffix).size;
      } catch {
        return null;
      }
    };
    return [{ file: prefix + entry.name, raw: size(""), gzip: size(".gz"), brotli: size(".br") }];
  });
}

const shaped = options.network === "reference";
const server = await serve({
  site,
  encodings: { br: ["br", "gzip"], gzip: ["gzip"], none: [] }[options.encoding],
  network: shaped && options.shaping === "server" ? { ...REFERENCE, connectRoundTrips: options.connect } : null,
});

// --- one visit -------------------------------------------------------------

const directory = join(root, "target/spikes/startup");
mkdirSync(directory, { recursive: true });
const type = options.browser === "firefox" ? firefox : chromium;
let version = "";

/** What a page or a worker knows about its own start: its marks and its requests. */
const timeline = () => ({
  origin: performance.timeOrigin,
  marks: Object.fromEntries(performance.getEntriesByType("mark").map((mark) => [mark.name, mark.startTime])),
  requests: performance.getEntriesByType("resource").map((entry) => ({
    name: entry.name.split("/").pop(),
    start: entry.startTime,
    end: entry.responseEnd,
    transferSize: entry.transferSize,
    decodedBodySize: entry.decodedBodySize,
  })),
});

async function visit(number) {
  const profile = join(directory, `profile-${options.browser}-${Date.now().toString(36)}-${number}`);
  mkdirSync(profile, { recursive: true });
  const context = await type.launchPersistentContext(profile, {
    headless: !options.headed,
    ...(options.browser === "chrome" ? { channel: "chrome" } : {}),
    locale: options.locale,
    viewport: { width: 1280, height: 800 },
  });
  try {
    version ||= context.browser()?.version() ?? "";
    const page = context.pages()[0] ?? (await context.newPage());
    const problems = [];
    page.on("pageerror", (error) => problems.push(String(error)));
    if (chromiumBased && (options.slowdown > 1 || (shaped && options.shaping === "protocol"))) {
      const session = await context.newCDPSession(page);
      if (options.slowdown > 1) {
        await session.send("Emulation.setCPUThrottlingRate", { rate: options.slowdown });
      }
      if (shaped && options.shaping === "protocol") {
        await session.send("Network.enable");
        await session.send("Network.emulateNetworkConditions", {
          offline: false,
          latency: REFERENCE.roundTripMs,
          downloadThroughput: REFERENCE.downBitsPerSecond / 8,
          uploadThroughput: REFERENCE.upBitsPerSecond / 8,
        });
      }
    }
    server.newVisit();
    const workerStarted = page.waitForEvent("worker");
    await page.goto(`${server.address}${options.path}`, { waitUntil: "commit" });
    await page.waitForFunction(() => performance.getEntriesByName("reticula:first-screen").length > 0, null, { timeout: 60000 });
    const worker = await workerStarted;

    const fromPage = await page.evaluate(
      `(() => {
        const timeline = ${timeline.toString()};
        const [navigation] = performance.getEntriesByType("navigation");
        return {
          ...timeline(),
          navigation: { responseStart: navigation.responseStart, responseEnd: navigation.responseEnd, domInteractive: navigation.domInteractive, transferSize: navigation.transferSize },
          paints: Object.fromEntries(performance.getEntriesByType("paint").map((paint) => [paint.name, paint.startTime])),
          userAgent: navigator.userAgent,
        };
      })()`,
    );
    const fromWorker = await worker.evaluate(`(${timeline.toString()})()`);

    // Everything on the page's clock: milliseconds since the visit began.
    const shift = fromWorker.origin - fromPage.origin;
    const request = (requests, pattern, offset = 0) => {
      const found = requests.find((entry) => pattern.test(entry.name));
      return found ? { start: found.start + offset, end: found.end + offset, transferSize: found.transferSize } : null;
    };
    const wasm = request(fromWorker.requests, /\.wasm$/, shift);
    const scripts = fromPage.requests.filter((entry) => editorScripts.has(entry.name));
    if (scripts.length !== editorScripts.size) {
      problems.push(`of ${editorScripts.size} scripts the page reports ${scripts.length}`);
    }
    const marks = {
      shell: fromPage.marks["reticula:shell"],
      engineReady: fromPage.marks["reticula:engine-ready"],
      firstScreen: fromPage.marks["reticula:first-screen"],
      workerStarted: shift,
      wasmReady: fromWorker.marks["reticula:wasm-ready"] + shift,
      sampleReady: fromWorker.marks["reticula:sample-ready"] + shift,
    };
    return {
      marks,
      document: { firstByte: fromPage.navigation.responseStart, end: fromPage.navigation.responseEnd, transferSize: fromPage.navigation.transferSize },
      // From the first script's request to the arrival of the last one.
      script: {
        start: Math.min(...scripts.map((entry) => entry.start)),
        end: Math.max(...scripts.map((entry) => entry.end)),
        transferSize: scripts.reduce((sum, entry) => sum + entry.transferSize, 0),
      },
      style: request(fromPage.requests, /\.css$/),
      workerScript: request(fromPage.requests, /^worker-.*\.js$/),
      wasm,
      firstContentfulPaint: fromPage.paints["first-contentful-paint"] ?? null,
      // Parts of the way that are the engine's own doing.
      compileAfterDownload: wasm ? marks.wasmReady - wasm.end : null,
      generation: marks.sampleReady - marks.wasmReady,
      readyToFirstScreen: marks.firstScreen - marks.engineReady,
      userAgent: fromPage.userAgent,
      problems,
    };
  } finally {
    await context.close().catch(() => {});
    try {
      rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    } catch {
      // A browser process has not let go of a file yet; the directory is under target/.
    }
  }
}

// --- the visits ------------------------------------------------------------

function summary(values) {
  const sorted = values.filter((value) => Number.isFinite(value)).sort((left, right) => left - right);
  const at = (fraction) => sorted[Math.min(sorted.length - 1, Math.floor(fraction * sorted.length))] ?? Number.NaN;
  return { count: sorted.length, min: sorted[0] ?? Number.NaN, p50: at(0.5), p95: at(0.95), max: sorted[sorted.length - 1] ?? Number.NaN };
}

const network = !shaped
  ? "no network held back"
  : options.shaping === "server"
    ? `${REFERENCE.downBitsPerSecond / 1e6} Mbit/s down, round trip ${REFERENCE.roundTripMs} ms, ${options.connect} round trips to connect, held back by the server`
    : `${REFERENCE.downBitsPerSecond / 1e6} Mbit/s down, round trip ${REFERENCE.roundTripMs} ms, nothing for connecting, held back by the browser`;
const visits = [];
for (let number = 1; number <= options.runs; number++) {
  const result = await visit(number);
  visits.push(result);
  if (number === 1) {
    console.log(`${options.browser} ${version}${options.headed ? "" : ", headless"}${options.slowdown > 1 ? `, main thread slowed ${options.slowdown}x` : ""}; ${network}; ${options.encoding === "none" ? "uncompressed" : options.encoding}`);
  }
  console.log(`  visit ${String(number).padStart(2)}: shell ${result.marks.shell.toFixed(0).padStart(5)} ms, first screen ${result.marks.firstScreen.toFixed(0).padStart(5)} ms`);
}
const requests = server.log.slice(-Math.round(server.log.length / options.runs));
server.close();

const pick = (path) => summary(visits.map((result) => path(result)));
const results = {
  shell: pick((result) => result.marks.shell),
  firstScreen: pick((result) => result.marks.firstScreen),
  steps: {
    "document, first byte": pick((result) => result.document.firstByte),
    "scripts, all loaded": pick((result) => result.script?.end),
    "style sheet, loaded": pick((result) => result.style?.end),
    "first contentful paint": pick((result) => result.firstContentfulPaint),
    "shell: the grid's first frame": pick((result) => result.marks.shell),
    "worker's script, loaded": pick((result) => result.workerScript?.end),
    "worker running": pick((result) => result.marks.workerStarted),
    "wasm, downloaded": pick((result) => result.wasm?.end),
    "wasm, compiled and instantiated": pick((result) => result.marks.wasmReady),
    "sample generated": pick((result) => result.marks.sampleReady),
    "page told that the engine is ready": pick((result) => result.marks.engineReady),
    "first screen drawn": pick((result) => result.marks.firstScreen),
  },
  parts: {
    "wasm: compiling after the download ended": pick((result) => result.compileAfterDownload),
    "generating the sample": pick((result) => result.generation),
    "engine ready to first screen": pick((result) => result.readyToFirstScreen),
  },
};

const ms = (value) => (Number.isFinite(value) ? value.toFixed(0) : "-").padStart(6);
const row = (label, stats) => console.log(`  ${label.padEnd(42)} p50 ${ms(stats.p50)}  p95 ${ms(stats.p95)}  min ${ms(stats.min)}  max ${ms(stats.max)}`);
console.log(`\nmilliseconds from the start of the visit, ${visits.length} visits`);
for (const [label, stats] of Object.entries(results.steps)) {
  row(label, stats);
}
console.log("\nparts of it (ms)");
for (const [label, stats] of Object.entries(results.parts)) {
  row(label, stats);
}
console.log("\nwhat one visit downloads");
for (const entry of requests) {
  console.log(`  ${entry.path.padEnd(44)} ${String(entry.bytes).padStart(8)} bytes  ${entry.encoding}`);
}
console.log(`  ${"in all".padEnd(44)} ${String(requests.reduce((sum, entry) => sum + entry.bytes, 0)).padStart(8)} bytes`);
const verdict = (value, target) => `${value.toFixed(0)} ms, ${value <= target ? "within" : "MISSES"} the target of ${target} ms`;
console.log(`\nshell, median: ${verdict(results.shell.p50, TARGETS.shell)}`);
console.log(`first screen, median: ${verdict(results.firstScreen.p50, TARGETS.firstScreen)}`);
const problems = visits.flatMap((result) => result.problems);
if (problems.length > 0) {
  console.log(`\nthe page reported errors:\n  ${problems.join("\n  ")}`);
}

// --- the data file ---------------------------------------------------------

const name = [
  "s9",
  options.browser,
  shaped ? `${options.shaping}${options.shaping === "server" ? `-connect${options.connect}` : ""}` : "unshaped",
  options.encoding,
  ...(options.slowdown > 1 ? [`slow${options.slowdown}`] : []),
  ...(options.headed ? ["headed"] : []),
  ...(options.locale === "en-US" ? [] : [options.locale]),
  ...(options.quick ? ["quick"] : []),
].join("-");
const out = options.out ? resolve(options.out) : join(directory, `${name}.json`);
mkdirSync(dirname(out), { recursive: true });
writeFileSync(
  out,
  `${JSON.stringify(
    {
      spike: "s9",
      suite: "m6-startup",
      date: new Date().toISOString(),
      ...commit(),
      machine: {
        cpu: os.cpus()[0]?.model.trim(),
        logicalProcessors: os.cpus().length,
        memoryGB: Math.round(os.totalmem() / 2 ** 30),
        os: `${os.type()} ${os.release()} ${os.arch()}`,
      },
      browser: { name: options.browser, version, headless: !options.headed, userAgent: visits[0]?.userAgent },
      options,
      network: shaped ? { ...REFERENCE, connectRoundTrips: options.shaping === "server" ? options.connect : 0, heldBackBy: options.shaping } : null,
      targets: TARGETS,
      sizes: sizes(),
      requests,
      results,
      visits,
      problems,
    },
    null,
    1,
  )}\n`,
);
console.log(`\ndata written to ${out}`);
process.exit(problems.length > 0 ? 1 : 0);
