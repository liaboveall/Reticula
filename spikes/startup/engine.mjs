// Times the engine by itself: how long a build of the module takes to
// generate the sample, in a page, the first time and once the browser has had
// the chance to compile it better.
//
//   node spikes/startup/engine.mjs [options] [<directory of a build>...]
//
// A build is the output of wasm-bindgen: by default the one the app uses
// (packages/protocol/pkg). Each timing is taken in a browser started for it,
// because a browser keeps the code it compiled for a module and hands it to
// the next page that loads the same bytes. `cargo xtask spike s9-builds`
// builds the module at several optimisation levels and runs this on them.
//
// Options:
//   --browser <name>    chrome (the installed Google Chrome, the default),
//                       chromium or firefox (the builds Playwright downloads)
//   --runs <n>          browsers started for each build (default 5)
//   --rows <n>          rows of the sample (default 1000000)
//   --reserve <bytes>   before generating, make the module's memory grow to
//                       this size in one step, by allocating a block that
//                       large and freeing it; the difference is what growing
//                       the memory in small steps costs
//   --js-flags <flags>  Chromium only: flags for its JavaScript engine, such
//                       as --liftoff-only or --no-liftoff, to see what each
//                       of its WebAssembly compilers contributes
//   --affinity <mask>   pin this process, and so the browser, to a set of
//                       logical processors: a hexadecimal mask
//   --out <file>        the data file (default: target/spikes/startup/engine-<browser>.json)

import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { dirname, join, relative, resolve } from "node:path";
import { brotliCompressSync, constants, gzipSync } from "node:zlib";
import { chromium, firefox } from "@playwright/test";
import { pin } from "../pin.mjs";
import { root } from "./serve.mjs";

const options = { browser: "chrome", runs: 5, rows: 1_000_000, reserve: 0, "js-flags": "", affinity: "", out: "" };
const builds = [];
const given = process.argv.slice(2);
for (let index = 0; index < given.length; index++) {
  if (!given[index].startsWith("--")) {
    builds.push(resolve(given[index]));
    continue;
  }
  const name = given[index].slice(2);
  const value = given[++index];
  if (!(name in options) || value === undefined) {
    console.error(`unknown option or missing value: ${given[index - 1]}`);
    process.exit(2);
  }
  options[name] = typeof options[name] === "number" ? Number(value) : value;
}
if (builds.length === 0) {
  builds.push(join(root, "packages/protocol/pkg"));
}
if (options.affinity) {
  pin(options.affinity);
}

// The page: it loads the build it is told to and generates the sample three
// times over, each time in an engine of its own. It can also time growing a
// memory by itself, in the steps an allocator takes and in one step.
const PAGE = `<!doctype html><meta charset="utf-8"><title>engine</title><script type="module">
  const { default: init, Engine } = await import("/build/reticula_wasm.js");
  const started = performance.now();
  const wasm = await init({ module_or_path: "/build/reticula_wasm_bg.wasm" });
  const loaded = performance.now() - started;
  window.__engine = (rows, reserve) => {
    if (reserve > 0) {
      // The allocator keeps what is freed, so the engine's allocations that
      // follow come out of this block and the memory does not grow again.
      wasm.__wbindgen_free(wasm.__wbindgen_malloc(reserve, 1), reserve, 1);
    }
    const generations = [];
    for (let turn = 0; turn < 3; turn++) {
      const engine = new Engine();
      const before = performance.now();
      const ready = engine.handle({ kind: "openSample", seed: 20261117, rows });
      generations.push({ outside: performance.now() - before, inside: ready.sheet.generationMs });
      engine.free();
    }
    return { loaded, generations, memoryBytes: wasm.memory.buffer.byteLength };
  };
  window.__grow = (pages) => {
    const time = (action) => {
      const before = performance.now();
      action();
      return performance.now() - before;
    };
    const stepwise = new WebAssembly.Memory({ initial: 1 });
    const atOnce = new WebAssembly.Memory({ initial: 1 });
    return {
      pages,
      onePageAtATime: time(() => {
        for (let page = 0; page < pages; page++) {
          stepwise.grow(1);
        }
      }),
      inOneStep: time(() => atOnce.grow(pages)),
    };
  };
</script>`;

let build = builds[0];
const server = createServer(async (request, response) => {
  const path = new URL(request.url, "http://localhost").pathname;
  // Isolated, for a clock fine enough to time with.
  const headers = { "cache-control": "no-store", "cross-origin-opener-policy": "same-origin", "cross-origin-embedder-policy": "require-corp" };
  if (path === "/") {
    response.writeHead(200, { ...headers, "content-type": "text/html" }).end(PAGE);
    return;
  }
  try {
    const name = path.replace(/^\/build\//, "");
    const body = await readFile(join(build, name));
    response.writeHead(200, { ...headers, "content-type": name.endsWith(".wasm") ? "application/wasm" : "text/javascript" }).end(body);
  } catch {
    response.writeHead(404).end();
  }
});
await new Promise((ready) => server.listen(0, "127.0.0.1", ready));
const address = `http://localhost:${server.address().port}/`;

const type = options.browser === "firefox" ? firefox : chromium;
const launch = {
  ...(options.browser === "chrome" ? { channel: "chrome" } : {}),
  ...(options["js-flags"] ? { args: [`--js-flags=${options["js-flags"]}`] } : {}),
};
const median = (values) => [...values].sort((left, right) => left - right)[Math.floor(values.length / 2)];
const ms = (value) => value.toFixed(0).padStart(5);

let version = "";
const results = [];
for (build of builds) {
  const wasm = readFileSync(join(build, "reticula_wasm_bg.wasm"));
  const size = {
    raw: wasm.length,
    gzip: gzipSync(wasm, { level: 9 }).length,
    brotli: brotliCompressSync(wasm, { params: { [constants.BROTLI_PARAM_QUALITY]: 11, [constants.BROTLI_PARAM_SIZE_HINT]: wasm.length } }).length,
  };
  const runs = [];
  for (let run = 0; run < options.runs; run++) {
    const browser = await type.launch(launch);
    version ||= browser.version();
    const page = await browser.newPage({ locale: "en-US" });
    await page.goto(address);
    await page.waitForFunction(() => window.__engine !== undefined);
    runs.push(await page.evaluate(([rows, reserve]) => window.__engine(rows, reserve), [options.rows, options.reserve]));
    await browser.close();
  }
  const turn = (index) => median(runs.map((run) => run.generations[index].inside));
  const result = {
    build: relative(root, build).replaceAll("\\", "/"),
    size,
    loadMs: median(runs.map((run) => run.loaded)),
    generationMs: [turn(0), turn(1), turn(2)],
    memoryBytes: runs[0].memoryBytes,
    runs,
  };
  results.push(result);
  if (results.length === 1) {
    console.log(
      `${options.browser} ${version}${options["js-flags"] ? ` ${options["js-flags"]}` : ""}, ${options.rows} rows${options.reserve ? `, ${options.reserve} bytes reserved first` : ""}, medians of ${options.runs} browsers`,
    );
    console.log(`  ${"build".padEnd(44)} ${"raw".padStart(8)} ${"gzip".padStart(8)} ${"brotli".padStart(8)}   load  generating: first second  third (ms)`);
  }
  console.log(
    `  ${result.build.padEnd(44)} ${String(size.raw).padStart(8)} ${String(size.gzip).padStart(8)} ${String(size.brotli).padStart(8)}  ${ms(result.loadMs)}              ${ms(result.generationMs[0])}  ${ms(result.generationMs[1])}  ${ms(result.generationMs[2])}`,
  );
}

// What growing a memory costs by itself: as many pages as the sample needs.
const pages = Math.ceil((results[0]?.memoryBytes ?? 0) / 65536);
const grows = [];
for (let run = 0; run < options.runs; run++) {
  const browser = await type.launch(launch);
  const page = await browser.newPage({ locale: "en-US" });
  await page.goto(address);
  await page.waitForFunction(() => window.__grow !== undefined);
  grows.push(await page.evaluate((pages) => window.__grow(pages), pages));
  await browser.close();
}
const growth = { pages, onePageAtATimeMs: median(grows.map((run) => run.onePageAtATime)), inOneStepMs: median(grows.map((run) => run.inOneStep)), runs: grows };
console.log(`  growing a memory by ${pages} pages: ${growth.onePageAtATimeMs.toFixed(1)} ms one page at a time, ${growth.inOneStepMs.toFixed(2)} ms in one step`);
server.close();

function git(...args) {
  try {
    return execFileSync("git", args, { cwd: root, encoding: "utf8" }).trim();
  } catch {
    return "";
  }
}
const out = options.out ? resolve(options.out) : join(root, `target/spikes/startup/engine-${options.browser}${options.reserve ? "-reserved" : ""}.json`);
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, `${JSON.stringify({ spike: "s9", date: new Date().toISOString(), commit: git("rev-parse", "--short", "HEAD"), uncommittedChanges: git("status", "--porcelain") !== "", browser: { name: options.browser, version }, options, results, growth }, null, 1)}\n`);
console.log(`data written to ${out}`);
