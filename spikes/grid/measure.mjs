// Measures the grid in a real browser.
//
//   node spikes/grid/measure.mjs <s1|s2> [options]
//
//   s1  viewport pages between the engine's worker and the main thread
//   s2  drawing text on a 2D canvas
//
// It serves the measurement page built by `vite build --mode spikes`
// (apps/web/dist-spikes), drives it, prints a summary and writes everything it
// measured, with a description of the machine and the browser, to a data file.
// `cargo xtask spike s1` builds the page first and then runs this.
//
// Options:
//   --browser <name>   chrome (the installed Google Chrome, the default),
//                      chromium or firefox (the builds Playwright downloads)
//   --headed           show the browser window. Frame intervals only mean
//                      something this way: a headless browser has no display
//   --dpr <number>     device pixel ratio to emulate (default 1)
//   --slowdown <n>     throttle the page's main thread by this factor
//                      (Chromium only), as a rough stand-in for a slower machine
//   --affinity <mask>  pin this process, and so the browser, to a set of
//                      logical processors: a hexadecimal mask. On a CPU with
//                      performance and efficiency cores, name the former
//   --trace            s2 only, Chromium only: record a trace of the pacing
//                      run that the browser's Performance panel can load
//   --quick            a short run, to see that everything works
//   --out <file>       the data file (default: target/spikes/grid/<name>.json)

import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import os from "node:os";
import { dirname, join, resolve } from "node:path";
import { chromium, firefox } from "@playwright/test";
import { root, serve } from "./serve.mjs";

// --- options ---------------------------------------------------------------

const [bench, ...rest] = process.argv.slice(2);
const options = { browser: "chrome", headed: false, dpr: 1, slowdown: 1, affinity: "", trace: false, quick: false, out: "" };
for (let index = 0; index < rest.length; index++) {
  const name = rest[index].replace(/^--/, "");
  if (!(name in options) || !rest[index].startsWith("--")) {
    fail(`unknown option ${rest[index]}`);
  }
  if (typeof options[name] === "boolean") {
    options[name] = true;
  } else {
    const value = rest[++index];
    if (value === undefined) {
      fail(`--${name} needs a value`);
    }
    options[name] = typeof options[name] === "number" ? Number(value) : value;
  }
}
if (bench !== "s1" && bench !== "s2") {
  fail("usage: node spikes/grid/measure.mjs <s1|s2> [options]");
}
if (!["chrome", "chromium", "firefox"].includes(options.browser)) {
  fail(`unknown browser ${options.browser}`);
}

function fail(message) {
  console.error(message);
  process.exit(2);
}

// --- pinning ---------------------------------------------------------------

// A process cannot change which processors it may run on from the inside, so
// the script starts itself again under a command that can. The browser's
// processes inherit the setting.
if (options.affinity && !process.env.RETICULA_PINNED) {
  const environment = { ...process.env, RETICULA_PINNED: options.affinity };
  const command = [process.execPath, ...process.argv.slice(1)];
  const child =
    process.platform === "win32"
      ? spawnSync(
          "cmd.exe",
          ["/d", "/s", "/c", `"start "" /B /WAIT /AFFINITY ${options.affinity} ${command.map((part) => `"${part}"`).join(" ")}"`],
          { stdio: "inherit", env: environment, windowsVerbatimArguments: true },
        )
      : spawnSync("taskset", [options.affinity, ...command], { stdio: "inherit", env: environment });
  process.exit(child.status ?? 1);
}

/** Facts about the machine that Node cannot see, asked of the system. Windows only. */
function system() {
  if (process.platform !== "win32") {
    return null;
  }
  const script = [
    `$affinity = '{0:X}' -f [int64](Get-Process -Id ${process.pid}).ProcessorAffinity`,
    "$battery = Get-CimInstance Win32_Battery | Select-Object -First 1",
    "$displays = Get-CimInstance Win32_VideoController | Where-Object CurrentRefreshRate | ForEach-Object { '{0} {1}x{2} {3} Hz' -f $_.Name, $_.CurrentHorizontalResolution, $_.CurrentVerticalResolution, $_.CurrentRefreshRate }",
    "[pscustomobject]@{ affinity = $affinity; onMains = if ($battery) { $battery.BatteryStatus -eq 2 } else { $true }; powerScheme = ((powercfg /getactivescheme) -join ' ') -replace '.*?([0-9a-fA-F]{8}(-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}).*', '$1'; displays = @($displays) } | ConvertTo-Json -Compress",
  ].join("; ");
  try {
    return JSON.parse(execFileSync("powershell.exe", ["-NoProfile", "-Command", script], { encoding: "utf8" }));
  } catch {
    return null;
  }
}

function git(...args) {
  try {
    return execFileSync("git", args, { cwd: root, encoding: "utf8" }).trim();
  } catch {
    return "";
  }
}

// --- the page --------------------------------------------------------------

const server = await serve();

const browser = await (options.browser === "firefox" ? firefox : chromium).launch({
  headless: !options.headed,
  ...(options.browser === "chrome" ? { channel: "chrome" } : {}),
});
// 1676 by 1036 leaves room for 20 columns and 50 rows of the sizes the page uses.
const context = await browser.newContext({ viewport: { width: 1676, height: 1036 }, deviceScaleFactor: options.dpr });
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
if (options.slowdown > 1) {
  if (options.browser === "firefox") {
    fail("--slowdown works in Chromium only");
  }
  const session = await context.newCDPSession(page);
  await session.send("Emulation.setCPUThrottlingRate", { rate: options.slowdown });
}

const call = (group, method, argument) =>
  page.evaluate(([group, method, argument]) => window.__bench[group][method](argument), [group, method, argument]);

// --- the measurements ------------------------------------------------------

const environment = await page.evaluate(() => window.__bench.environment());
const title = `${options.browser} ${browser.version()}${options.headed ? "" : ", headless"}, DPR ${environment.devicePixelRatio}${options.slowdown > 1 ? `, main thread slowed ${options.slowdown}x` : ""}`;
console.log(title);
console.log(
  `clock resolution ${(environment.timerResolution * 1000).toFixed(0)} µs, frames every ${environment.frameInterval.p50.toFixed(2)} ms, ${environment.viewport.width}x${environment.viewport.height}`,
);

const ms = (value) => `${value.toFixed(value < 10 ? 3 : 1)}`;
const row = (label, summary, format = ms) =>
  console.log(`  ${label.padEnd(44)} p50 ${format(summary.p50).padStart(8)}  p95 ${format(summary.p95).padStart(8)}  p99 ${format(summary.p99).padStart(8)}  max ${format(summary.max).padStart(8)}`);
const verdict = (passed) => (passed ? "within the target" : "MISSES THE TARGET");

const results = {};
const quick = options.quick;

if (bench === "s1") {
  const roundTrip = (results.roundTrip = await call("s1", "roundTrip", { samples: quick ? 200 : 2000, warmup: 200, seed: 1 }));
  console.log(`\nround trip of one page, ${roundTrip.cellsPerPage} cells, ${roundTrip.total.count} samples (ms)`);
  row("request to decoded", roundTrip.total);
  row("  until the packet arrived", roundTrip.arrival);
  row("  decoding", roundTrip.decode);
  row("packet size (bytes)", roundTrip.bytes, (value) => value.toFixed(0));
  console.log(`  target p95 <= 2 ms: ${verdict(roundTrip.total.p95 <= 2)}`);

  const codec = (results.codec = await call("s1", "codec", { pages: 200, rounds: quick ? 5 : 30, seed: 2 }));
  const micros = (value) => value.toFixed(1);
  console.log(`\none page on its own, ${codec.pages} pages x ${codec.rounds} rounds (microseconds for a page)`);
  row("encoding, in the engine", codec.encodeMicros, micros);
  row("decoding, in the grid", codec.decodeMicros, micros);
  row("  of which: the text, from UTF-8", codec.textFromUtf8Micros, micros);
  row("  the same text, were it UTF-16", codec.textFromUtf16Micros, micros);
  console.log(
    `  text of a page: ${codec.textBytesUtf8.mean.toFixed(0)} bytes as UTF-8, ${codec.textBytesUtf16.mean.toFixed(0)} as UTF-16; whole packet ${codec.packetBytes.mean.toFixed(0)} bytes`,
  );

  results.scroll = [];
  console.log(`\nscrolling at 5 screens a second${quick ? "" : " for 30 s"}, by margin (in multiples of the visible rows)`);
  for (const marginRows of [0, 0.25, 0.5, 1, 2]) {
    const scroll = await call("s1", "scroll", { marginRows, marginCols: 1, screensPerSecond: 5, seconds: quick ? 3 : 30, warmupSeconds: 2 });
    results.scroll.push(scroll);
    console.log(
      `  margin ${String(marginRows).padEnd(4)} ${scroll.frames} frames, ${scroll.framesWithPlaceholders} with placeholders (most ${scroll.mostPlaceholders} cells); ` +
        `${scroll.pagesReceived} pages, ${(scroll.bytesReceived / 1024).toFixed(0)} KB; held at most ${scroll.mostPagesHeld}; ` +
        `frame work p50 ${ms(scroll.work.p50)} p99 ${ms(scroll.work.p99)} max ${ms(scroll.work.max)} ms`,
    );
  }
  const smallest = results.scroll.find((scroll) => scroll.framesWithPlaceholders === 0);
  console.log(`  target no placeholders: ${smallest ? `met from a margin of ${smallest.margin.rows}` : "NOT MET at any margin tried"}`);

  const jump = (results.jump = await call("s1", "jump", { jumps: quick ? 20 : 200, seed: 3, marginRows: 2, marginCols: 1 }));
  console.log(`\njumping to ${jump.jumps} random positions (ms)`);
  row("jump to every cell drawn", jump.filled);
  row("  jump to the first frame there", jump.firstFrame);
  row("frames with placeholders", jump.framesWithPlaceholders, (value) => value.toFixed(0));
  console.log(`  target p95 <= 100 ms: ${verdict(jump.filled.p95 <= 100)}`);
} else {
  const frames = quick ? 120 : 600;
  const cases = [
    ["new text every frame", { content: "unique", rowsPerFrame: 50 }],
    ["scrolling by 2 rows a frame", { content: "unique", rowsPerFrame: 2 }],
    ["repeating values, new screen every frame", { content: "repeating", rowsPerFrame: 50 }],
  ];
  results.draw = [];
  console.log(`\nissuing the draw calls of one frame, ${frames} frames each (ms)`);
  for (const [label, parameters] of cases) {
    const result = await call("s2", "draw", { ...parameters, frames, warmup: 60 });
    results.draw.push({ label, ...result });
    row(label, result.draw);
    console.log(`    ${result.cells.p50} cells a frame, ${result.clipped.p50} of them clipped; frames every ${result.interval.p50.toFixed(2)} ms`);
  }
  const worst = Math.max(...results.draw.map((result) => result.draw.p95));
  console.log(`  target p95 <= 4 ms: ${verdict(worst <= 4)}`);

  results.shortcuts = [];
  console.log("\nwith one shortcut turned off (ms)");
  for (const shortcut of ["cacheWidths", "boundNumbers", "clipOnlyOverwide", "batchState"]) {
    for (const [label, parameters] of [cases[0], cases[2]]) {
      const result = await call("s2", "draw", { ...parameters, frames, warmup: 60, shortcuts: { [shortcut]: false } });
      results.shortcuts.push({ off: shortcut, label, ...result });
      row(`without ${shortcut}: ${parameters.content}`, result.draw);
    }
  }

  const tracePath = options.trace && options.browser !== "firefox" ? join(root, `target/spikes/grid/s2-trace-${options.browser}.json`) : "";
  if (tracePath) {
    mkdirSync(dirname(tracePath), { recursive: true });
    await browser.startTracing(page, { path: tracePath });
  }
  const pace = (results.pace = await call("s2", "pace", { content: "unique", screensPerSecond: 5, seconds: quick ? 3 : 10, warmupSeconds: 1 }));
  if (tracePath) {
    await browser.stopTracing();
    console.log(`\ntrace written to ${tracePath}`);
  }
  console.log(`\nscrolling at 5 screens a second for ${pace.seconds} s: ${pace.frames} frames (ms)`);
  row("time between frames", pace.interval);
  row("draw calls of a frame", pace.draw);
  console.log(`  ${pace.lateFrames} frames came late by more than half an interval; ${pace.framesOver60Hz} took longer than 16.7 ms`);
  if (!options.headed) {
    console.log("  (headless: there is no display, so these intervals say little; run with --headed)");
  }
}

if (problems.length > 0) {
  console.log(`\nthe page reported errors:\n  ${problems.join("\n  ")}`);
}

// --- the data file ---------------------------------------------------------

const out = options.out ? resolve(options.out) : join(root, `target/spikes/grid/${bench}-${options.browser}${options.headed ? "-headed" : ""}-dpr${options.dpr}${options.slowdown > 1 ? `-slow${options.slowdown}` : ""}.json`);
mkdirSync(dirname(out), { recursive: true });
writeFileSync(
  out,
  `${JSON.stringify(
    {
      bench,
      date: new Date().toISOString(),
      commit: git("rev-parse", "--short", "HEAD"),
      uncommittedChanges: git("status", "--porcelain") !== "",
      machine: {
        cpu: os.cpus()[0]?.model.trim(),
        logicalProcessors: os.cpus().length,
        memoryGB: Math.round(os.totalmem() / 2 ** 30),
        os: `${os.type()} ${os.release()}`,
        system: system(),
      },
      browser: { name: options.browser, version: browser.version(), headless: !options.headed },
      options,
      environment,
      results,
      problems,
    },
    null,
    1,
  )}\n`,
);
console.log(`\ndata written to ${out}`);

await browser.close();
server.close();
process.exit(problems.length > 0 ? 1 : 0);
