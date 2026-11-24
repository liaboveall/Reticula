// Measures drawing about ten thousand visible cells (S11).
//
//   node spikes/grid/zoomed.mjs [--browser chrome|chromium|firefox] [--headed]
//                               [--dpr n] [--affinity mask] [--quick] [--out file]
//
// It drives the page built by `vite build --mode spikes`. `cargo xtask spike s11`
// builds it first and then runs this.

import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import os from "node:os";
import { dirname, join, resolve } from "node:path";
import { chromium, firefox } from "@playwright/test";
import { pin } from "../pin.mjs";
import { root, serve } from "./serve.mjs";

const options = { browser: "chrome", headed: false, dpr: 1.5, affinity: "", quick: false, out: "" };
const rest = process.argv.slice(2);
for (let index = 0; index < rest.length; index++) {
  const name = rest[index].replace(/^--/, "");
  if (!(name in options)) {
    console.error(`unknown option ${rest[index]}`);
    process.exit(2);
  }
  if (typeof options[name] === "boolean") {
    options[name] = true;
  } else {
    const value = rest[++index];
    options[name] = typeof options[name] === "number" ? Number(value) : value;
  }
}
if (options.affinity) {
  pin(options.affinity);
}

const git = (...args) => execFileSync("git", args, { cwd: root, encoding: "utf8" }).trim();
const server = await serve();
const browser = await (options.browser === "firefox" ? firefox : chromium).launch({
  headless: !options.headed,
  ...(options.browser === "chrome" ? { channel: "chrome" } : {}),
  // The headless shell has no GPU; the full browser's headless mode has.
  ...(options.browser === "chromium" ? { channel: "chromium" } : {}),
});
const context = await browser.newContext({ locale: "en-US", viewport: { width: 1676, height: 1036 }, deviceScaleFactor: options.dpr });
const page = await context.newPage();
const problems = [];
page.on("pageerror", (error) => problems.push(String(error)));
page.on("console", (message) => {
  if (message.type() === "error") {
    problems.push(message.text());
  }
});
await page.goto(`${server.address}/bench.html`);
await page.waitForFunction(() => window.__bench !== undefined);
const call = (method, argument) => page.evaluate(([method, argument]) => window.__bench.s11[method](argument), [method, argument]);

const environment = await page.evaluate(() => window.__bench.environment());
console.log(`${options.browser} ${browser.version()}${options.headed ? "" : ", headless"}, DPR ${environment.devicePixelRatio}, frames every ${environment.frameInterval.p50.toFixed(2)} ms`);

const results = { quality: [], fidelity: [], scroll: [] };
const percent = (value) => `${(value * 100).toFixed(2)}%`;
for (const scale of [0.375, 0.5, 1, 1.25, 1.5, 2]) {
  const quality = await call("quality", { scale });
  results.quality.push(quality);
  console.log(`\nscale ${scale} (${quality.font.split(" ")[0]}), LCD pixels: opaque canvas ${quality.lcdPixelsOpaqueCanvas}, with alpha ${quality.lcdPixelsAlphaCanvas}`);
  for (const [name, value] of Object.entries(quality)) {
    if (value && typeof value === "object") {
      console.log(`  ${name.padEnd(20)} >16 ${percent(value.over16).padStart(8)}  >48 ${percent(value.over48).padStart(8)}  >96 ${percent(value.over96).padStart(8)}  mean ${value.mean.toFixed(2).padStart(6)}  max ${value.max}`);
    }
  }
}
for (const [color, background] of [["#dc2626", "#ffffff"], ["#e3e6ea", "#161616"], ["#60a5fa", "#161616"]]) {
  const quality = await call("quality", { scale: 1.5, color, background });
  results.quality.push(quality);
  console.log(`\nscale 1.5, ${color} on ${background}`);
  for (const name of ["stringOwnColor", "stringBlackMask", "stringWhiteMask", "glyph4OwnColor", "glyph4Fractional", "glyph4BlackMask", "glyph4WhiteMask"]) {
    const value = quality[name];
    console.log(`  ${name.padEnd(20)} >16 ${percent(value.over16).padStart(8)}  >48 ${percent(value.over48).padStart(8)}  >96 ${percent(value.over96).padStart(8)}  mean ${value.mean.toFixed(2).padStart(6)}  max ${value.max}`);
  }
}

const hasGpu = await page.evaluate(async () => Boolean(await navigator.gpu?.requestAdapter()));
if (hasGpu) {
  for (const zoom of [0.25, 0.5, 1, 1.5, 2]) {
    const fidelity = await call("fidelity", { zoom });
    results.fidelity.push(fidelity);
    console.log(`zoom ${zoom}: the texture against a 2D canvas, ${fidelity.texts} texts, ${fidelity.darkPixels} dark pixels, differences ${JSON.stringify(fidelity.difference)}`);
  }
}
const subjects = hasGpu ? ["canvas", "webgpu", "string", "glyph"] : ["canvas"];
for (const zoom of [1, 0.5, 0.25]) {
  for (const screensPerSecond of [5, 0]) {
    for (const subject of subjects) {
      const scroll = await call("scroll", { subject, zoom, screensPerSecond, seconds: options.quick ? 2 : 8, warmupSeconds: 1 });
      results.scroll.push(scroll);
      console.log(
        `zoom ${zoom} ${screensPerSecond === 0 ? "a new screen every frame" : "5 screens a second"}, ${subject.padEnd(6)} ${scroll.visibleCells} cells: work p50 ${scroll.work.p50.toFixed(2)} p95 ${scroll.work.p95.toFixed(2)} p99 ${scroll.work.p99.toFixed(2)} max ${scroll.work.max.toFixed(1)} ms; ` +
          `frames every p50 ${scroll.interval.p50.toFixed(1)} p95 ${scroll.interval.p95.toFixed(1)} p99 ${scroll.interval.p99.toFixed(1)} ms, ${scroll.framesOver60Hz} of ${scroll.frames} over 16.7; ${scroll.adapter} ${JSON.stringify(scroll.usage ?? scroll.counters.end ?? "")}`,
      );
    }
  }
}
if (problems.length > 0) {
  console.log(`the page reported errors:\n  ${problems.join("\n  ")}`);
}

const out = options.out ? resolve(options.out) : join(root, `target/spikes/grid/s11-${options.browser}${options.headed ? "-headed" : ""}-dpr${options.dpr}${options.quick ? "-quick" : ""}.json`);
mkdirSync(dirname(out), { recursive: true });
writeFileSync(
  out,
  `${JSON.stringify({ spike: "s11", date: new Date().toISOString(), commit: git("rev-parse", "--short", "HEAD"), uncommittedChanges: git("status", "--porcelain") !== "", machine: { cpu: os.cpus()[0]?.model.trim(), logicalProcessors: os.cpus().length, os: `${os.type()} ${os.release()}` }, browser: { name: options.browser, version: browser.version(), headless: !options.headed }, options, environment, results, problems }, null, 1)}\n`,
);
console.log(`data written to ${out}`);
await browser.close();
server.close();
