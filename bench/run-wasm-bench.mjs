// Runs engine WebAssembly benchmarks B2, B3, B4, B5, B7a (3 formula groups) and
// B7b (3 formula groups) inside Cross-Origin-Isolated headless Google Chrome
// (`09-quality.md` & `13-m1a.md` 7.4).
//
//   node bench/run-wasm-bench.mjs --pkg <pkg-dir> [--browser chrome] [--affinity FFFF] [--out <json-file>] [--only-b4]

import { mkdirSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import os from "node:os";
import { dirname, join, resolve } from "node:path";
import { chromium, firefox } from "@playwright/test";
import { pin } from "../spikes/pin.mjs";

const options = {
  pkg: "target/bench-wasm/pkg",
  browser: "chrome",
  affinity: "",
  out: "",
  "only-b4": false,
};

const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const arg = argv[i];
  if (arg === "--only-b4") {
    options["only-b4"] = true;
    continue;
  }
  const key = arg.replace(/^--/, "");
  const val = argv[++i];
  if (!(key in options) || val === undefined) {
    console.error(`unknown or incomplete option: ${arg}`);
    process.exit(2);
  }
  options[key] = val;
}

if (options.affinity) {
  pin(options.affinity);
}

const pkgDir = resolve(options.pkg);

const PAGE_HTML = `<!doctype html><meta charset="utf-8"><title>wasm-bench</title>
<script type="module">
  const { default: init, WasmBench } = await import("/pkg/reticula_wasm.js");
  const wasm = await init({ module_or_path: "/pkg/reticula_wasm_bg.wasm" });

  function summarize(warmup, raw, extra = {}) {
    const sorted = [...raw].sort((a, b) => a - b);
    const n = sorted.length || 1;
    const pct = (p) => {
      if (sorted.length === 0) return 0;
      const idx = Math.min(n - 1, Math.ceil((n - 1) * p));
      return Math.round(sorted[idx] * 1000) / 1000;
    };
    const mean = sorted.reduce((acc, x) => acc + x, 0) / n;
    return {
      warmup,
      samples: sorted.length,
      p50_ms: pct(0.5),
      p95_ms: pct(0.95),
      p99_ms: pct(0.99),
      max_ms: Math.round((sorted[n - 1] ?? 0) * 1000) / 1000,
      mean_ms: Math.round(mean * 1000) / 1000,
      ...extra,
    };
  }

  function measureClockGranularityUs() {
    let minDeltaMs = Infinity;
    for (let i = 0; i < 2000; i++) {
      const t0 = performance.now();
      let t1 = t0;
      let guard = 0;
      while (t1 === t0 && guard < 100000) {
        t1 = performance.now();
        guard++;
      }
      if (t1 > t0 && t1 - t0 < minDeltaMs) {
        minDeltaMs = t1 - t0;
      }
    }
    return Number.isFinite(minDeltaMs) ? Math.round(minDeltaMs * 1000 * 100) / 100 : null;
  }

  window.__runBenchmarks = (onlyB4) => {
    const clockResolutionUs = measureClockGranularityUs();

    // B4: 100k incremental recalc (target <= 50 ms)
    const b4 = WasmBench.new_b4();
    const b4Times = [];
    for (let i = 0; i < 3 + 20; i++) {
      const t0 = performance.now();
      b4.step_b4();
      const dt = performance.now() - t0;
      if (i >= 3) b4Times.push(dt);
    }
    b4.free();
    const b4Summary = summarize(3, b4Times);

    if (onlyB4) {
      // Also run B3 quick for s9-builds comparison
      const b3q = WasmBench.new_b3();
      const b3qTimes = [];
      for (let i = 0; i < 1 + 5; i++) {
        const t0 = performance.now();
        b3q.step_b3();
        const dt = performance.now() - t0;
        if (i >= 1) b3qTimes.push(dt);
      }
      b3q.free();
      return {
        crossOriginIsolated: window.crossOriginIsolated,
        clockResolutionUs,
        b4_incremental_100k: b4Summary,
        b3_full_recalc_1m: summarize(1, b3qTimes),
      };
    }

    // B2: 1M-row SUM
    const b2 = WasmBench.new_b2();
    const b2Times = [];
    for (let i = 0; i < 3 + 20; i++) {
      const t0 = performance.now();
      b2.step_b2();
      const dt = performance.now() - t0;
      if (i >= 3) b2Times.push(dt);
    }
    b2.free();

    // B3: 1M formulas =B2*C2+$Z$1 full recalc
    const b3 = WasmBench.new_b3();
    const b3Times = [];
    for (let i = 0; i < 2 + 10; i++) {
      const t0 = performance.now();
      b3.step_b3();
      const dt = performance.now() - t0;
      if (i >= 2) b3Times.push(dt);
    }
    b3.free();

    // B5: 1M-row cumulative chain =C1+B2 (explicit stack, zero recursion)
    const b5 = WasmBench.new_b5();
    const b5Times = [];
    for (let i = 0; i < 2 + 10; i++) {
      const t0 = performance.now();
      b5.step_b5();
      const dt = performance.now() - t0;
      if (i >= 2) b5Times.push(dt);
    }
    b5.free();

    // B7a & B7b across all 3 required formula groups:
    function runB7Group(group, warmup, samples, withBatchMean) {
      const fixture = WasmBench.new_b7(group);
      const insTimes = [];
      const editTimes = [];
      for (let i = 0; i < warmup + samples; i++) {
        const t0 = performance.now();
        fixture.step_b7a(group, 1);
        const dtIns = performance.now() - t0;

        const t1 = performance.now();
        fixture.step_b7b();
        const dtEdit = performance.now() - t1;

        if (i >= warmup) {
          insTimes.push(dtIns);
          editTimes.push(dtEdit);
        }
      }
      let extra = {};
      if (withBatchMean) {
        // Single top-row insertion on row-local formulas is near the 5 us clock
        // quantum, so also time a 100-insert batch and report per-op mean (09-quality.md).
        const tb = performance.now();
        fixture.step_b7a(group, 100);
        const batchMeanMs = (performance.now() - tb) / 100;
        extra = {
          batch100_mean_ms: Math.round(batchMeanMs * 10000) / 10000,
          batch_note: "mean of 100 consecutive top-row insertions (single op approaches 5 us timer quantum)",
        };
      }
      fixture.free();
      return {
        b7a: summarize(warmup, insTimes, extra),
        b7b: summarize(warmup, editTimes),
      };
    }

    const g1 = runB7Group(1, 5, 30, true);
    const g2 = runB7Group(2, 3, 20, false);
    const g3 = runB7Group(3, 3, 15, false);

    return {
      crossOriginIsolated: window.crossOriginIsolated,
      clockResolutionUs,
      memoryBytes: wasm.memory.buffer.byteLength,
      b2_sum_1m: summarize(3, b2Times),
      b3_full_recalc_1m: summarize(2, b3Times),
      b4_incremental_100k: b4Summary,
      b5_chain_1m: summarize(2, b5Times),
      b7a_insert_top_row_local: g1.b7a,
      b7b_edit_after_insert_row_local: g1.b7b,
      b7a_insert_cross_range: g2.b7a,
      b7b_edit_after_insert_cross_range: g2.b7b,
      b7a_insert_structural_row_fn: g3.b7a,
      b7b_edit_after_insert_structural: g3.b7b,
    };
  };
</script>`;

const server = createServer(async (req, res) => {
  const pathname = new URL(req.url ?? "/", "http://localhost").pathname;
  const headers = {
    "cache-control": "no-store",
    "cross-origin-opener-policy": "same-origin",
    "cross-origin-embedder-policy": "require-corp",
  };
  if (pathname === "/") {
    res.writeHead(200, { ...headers, "content-type": "text/html" }).end(PAGE_HTML);
    return;
  }
  try {
    const file = pathname.replace(/^\/pkg\//, "");
    const body = await readFile(join(pkgDir, file));
    res
      .writeHead(200, {
        ...headers,
        "content-type": file.endsWith(".wasm") ? "application/wasm" : "text/javascript",
      })
      .end(body);
  } catch {
    res.writeHead(404).end();
  }
});

await new Promise((ready) => server.listen(0, "127.0.0.1", ready));
const url = `http://localhost:${server.address().port}/`;

const launcher = options.browser === "firefox" ? firefox : chromium;
const browser = await launcher.launch(options.browser === "chrome" ? { channel: "chrome" } : {});
const browserVersion = browser.version();
const page = await browser.newPage();
await page.goto(url);
await page.waitForFunction(() => window.__runBenchmarks !== undefined);
const metrics = await page.evaluate((onlyB4) => window.__runBenchmarks(onlyB4), options["only-b4"]);
await browser.close();
server.close();

const payload = {
  browser: { name: options.browser, version: browserVersion },
  affinity: options.affinity || "unpinned",
  machine: {
    cpus: os.cpus()[0]?.model ?? "unknown",
    logicalProcessors: os.cpus().length,
    totalMemGiB: Math.round((os.totalmem() / 1024 ** 3) * 10) / 10,
    platform: `${os.platform()} ${os.release()}`,
  },
  ...metrics,
};

if (options.out) {
  const outPath = resolve(options.out);
  mkdirSync(dirname(outPath), { recursive: true });
  writeFileSync(outPath, `${JSON.stringify(payload, null, 2)}\n`);
}
console.log(JSON.stringify(payload, null, 2));
