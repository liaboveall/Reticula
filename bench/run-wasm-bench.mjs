// Runs engine WebAssembly benchmarks B1, B2, B3, B4, B5, B6, B7a/B7b (3 formula
// groups), B8 (memory footprints), B11 (CoW OPFS snapshot save + concurrent 25 ms
// probes), and B12 (50M-cell upper-limit load / CoW save / 100k recalc / peak memory)
// inside Cross-Origin-Isolated headless Google Chrome (`09-quality.md`, `13-m1a.md`,
// `14-m1b.md` 8.3).
//
//   node bench/run-wasm-bench.mjs --pkg <pkg-dir> [--browser chrome] [--affinity FFFF] [--out <json-file>] [--only-b4] [--quick]

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
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
  quick: false,
};

const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const arg = argv[i];
  if (arg === "--only-b4") {
    options["only-b4"] = true;
    continue;
  }
  if (arg === "--quick") {
    options.quick = true;
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

const WORKER_JS = `
import init, { WasmBench } from "/pkg/reticula_wasm.js";

const wasm = await init({ module_or_path: "/pkg/reticula_wasm_bg.wasm" });

let activeBench = null;
let peakMem = wasm.memory.buffer.byteLength;

function trackMem() {
  const cur = wasm.memory.buffer.byteLength;
  if (cur > peakMem) peakMem = cur;
  return cur;
}

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

async function openOpfsSync(fileName, create) {
  const root = await navigator.storage.getDirectory();
  const fh = await root.getFileHandle(fileName, { create });
  return fh.createSyncAccessHandle();
}

async function removeOpfsFile(fileName) {
  try {
    const root = await navigator.storage.getDirectory();
    await root.removeEntry(fileName);
  } catch {
    // Ignore if already absent.
  }
}

// Active background CoW snapshot save state (yields via MessageChannel like production worker).
let saveJob = null;
const sliceChannel = new MessageChannel();

function scheduleSaveSlice() {
  sliceChannel.port2.postMessage(0);
}

sliceChannel.port1.onmessage = () => {
  const job = saveJob;
  if (!job || !activeBench) return;
  const sliceStart = performance.now();
  try {
    while (performance.now() - sliceStart < job.sliceBudgetMs) {
      const step = activeBench.step_snapshot_stream(job.maxBlocks);
      trackMem();
      const status = step[0];
      const offsetOrLen = step[1];
      if (status === 1) {
        const chunk = activeBench.take_snapshot_chunk();
        if (chunk && chunk.byteLength > 0) {
          job.sync.write(chunk, { at: offsetOrLen });
        }
      } else if (status === 2) {
        const header = activeBench.take_snapshot_chunk();
        if (header && header.byteLength > 0) {
          job.sync.write(header, { at: 0 });
        }
        job.sync.truncate(offsetOrLen);
        job.sync.flush();
        job.sync.close();
        saveJob = null;
        trackMem();
        self.postMessage({
          kind: "saved",
          reqId: job.reqId,
          totalLen: offsetOrLen,
          peakWasmMemoryBytes: peakMem,
        });
        return;
      }
    }
    scheduleSaveSlice();
  } catch (err) {
    try {
      job.sync.close();
    } catch {
      // Ignore close error on failure.
    }
    saveJob = null;
    self.postMessage({ kind: "error", reqId: job.reqId, message: String(err) });
  }
};

self.onmessage = async (event) => {
  const msg = event.data;
  try {
    if (msg.kind === "probe") {
      const bytes = activeBench ? activeBench.probe_edit_and_page() : 0;
      trackMem();
      self.postMessage({ kind: "probeAck", id: msg.id, bytes, wasmMemoryBytes: trackMem() });
      return;
    }

    if (msg.kind === "runCore") {
      const quick = Boolean(msg.quick);
      const clockResolutionUs = measureClockGranularityUs();
      const wFast = quick ? 1 : 3;
      const nFast = quick ? 3 : 20;
      const wHeavy = quick ? 1 : 2;
      const nHeavy = quick ? 2 : 10;

      // B4: 100k incremental recalc (target <= 50 ms)
      const b4 = WasmBench.new_b4();
      const b4Times = [];
      for (let i = 0; i < wFast + nFast; i++) {
        const t0 = performance.now();
        b4.step_b4();
        const dt = performance.now() - t0;
        if (i >= wFast) b4Times.push(dt);
      }
      b4.free();
      const b4Summary = summarize(wFast, b4Times);

      if (msg.onlyB4) {
        const b3q = WasmBench.new_b3();
        const b3qTimes = [];
        for (let i = 0; i < 1 + 5; i++) {
          const t0 = performance.now();
          b3q.step_b3();
          const dt = performance.now() - t0;
          if (i >= 1) b3qTimes.push(dt);
        }
        b3q.free();
        self.postMessage({
          kind: "result",
          reqId: msg.reqId,
          value: {
            crossOriginIsolated: self.crossOriginIsolated,
            clockResolutionUs,
            b4_incremental_100k: b4Summary,
            b3_full_recalc_1m: summarize(1, b3qTimes),
          },
        });
        return;
      }

      // B2: 1M-row SUM
      const b2 = WasmBench.new_b2();
      const b2Times = [];
      for (let i = 0; i < wFast + nFast; i++) {
        const t0 = performance.now();
        b2.step_b2();
        const dt = performance.now() - t0;
        if (i >= wFast) b2Times.push(dt);
      }
      b2.free();

      // B3: 1M formulas =B2*C2+$Z$1 full recalc
      const b3 = WasmBench.new_b3();
      const b3Times = [];
      for (let i = 0; i < wHeavy + nHeavy; i++) {
        const t0 = performance.now();
        b3.step_b3();
        const dt = performance.now() - t0;
        if (i >= wHeavy) b3Times.push(dt);
      }
      b3.free();

      // B5: 1M-row cumulative chain =C1+B2
      const b5 = WasmBench.new_b5();
      const b5Times = [];
      for (let i = 0; i < wHeavy + nHeavy; i++) {
        const t0 = performance.now();
        b5.step_b5();
        const dt = performance.now() - t0;
        if (i >= wHeavy) b5Times.push(dt);
      }
      b5.free();

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
          const tb = performance.now();
          fixture.step_b7a(group, 100);
          const batchMeanMs = (performance.now() - tb) / 100;
          extra = {
            batch100_mean_ms: Math.round(batchMeanMs * 10000) / 10000,
            batch_note:
              "mean of 100 consecutive top-row insertions (single op approaches 5 us timer quantum)",
          };
        }
        fixture.free();
        return {
          b7a: summarize(warmup, insTimes, extra),
          b7b: summarize(warmup, editTimes),
        };
      }

      const g1 = runB7Group(1, quick ? 2 : 5, quick ? 5 : 30, true);
      const g2 = runB7Group(2, quick ? 1 : 3, quick ? 3 : 20, false);
      const g3 = runB7Group(3, quick ? 1 : 3, quick ? 3 : 15, false);

      self.postMessage({
        kind: "result",
        reqId: msg.reqId,
        value: {
          crossOriginIsolated: self.crossOriginIsolated,
          clockResolutionUs,
          memoryBytes: trackMem(),
          b2_sum_1m: summarize(wFast, b2Times),
          b3_full_recalc_1m: summarize(wHeavy, b3Times),
          b4_incremental_100k: b4Summary,
          b5_chain_1m: summarize(wHeavy, b5Times),
          b7a_insert_top_row_local: g1.b7a,
          b7b_edit_after_insert_row_local: g1.b7b,
          b7a_insert_cross_range: g2.b7a,
          b7b_edit_after_insert_cross_range: g2.b7b,
          b7a_insert_structural_row_fn: g3.b7a,
          b7b_edit_after_insert_structural: g3.b7b,
        },
      });
      return;
    }

    if (msg.kind === "runB6") {
      const warmup = msg.quick ? 0 : 1;
      const samples = msg.quick ? 1 : 3;
      const b6 = WasmBench.new_b6(100000, 100000);
      const times = [];
      for (let i = 0; i < warmup + samples; i++) {
        const t0 = performance.now();
        b6.step_b6();
        const dt = performance.now() - t0;
        if (i >= warmup) times.push(dt);
      }
      b6.free();
      self.postMessage({
        kind: "result",
        reqId: msg.reqId,
        value: summarize(warmup, times, { table_rows: 100000, formulas: 100000 }),
      });
      return;
    }

    if (msg.kind === "runB8Case") {
      const b8 = WasmBench.new_b8(msg.caseId);
      const result = {
        kernel_estimated_bytes: Math.round(b8.estimated_bytes()),
        wasm_memory_bytes: trackMem(),
        cells: msg.cells,
        formulas: b8.formula_count(),
        unique_strings: b8.unique_strings(),
      };
      b8.free();
      self.postMessage({ kind: "result", reqId: msg.reqId, value: result });
      return;
    }

    if (msg.kind === "prepareStorage") {
      const bench = WasmBench.new_storage_fixture(msg.rows, msg.cols, msg.formulaRows);
      const kernelEstimatedBytes = Math.round(bench.estimated_bytes());
      const sync = await openOpfsSync(msg.fileName, true);
      bench.start_snapshot_stream();
      let totalLen = 0;
      while (true) {
        const step = bench.step_snapshot_stream(64);
        const status = step[0];
        const offsetOrLen = step[1];
        if (status === 1) {
          const chunk = bench.take_snapshot_chunk();
          if (chunk && chunk.byteLength > 0) {
            sync.write(chunk, { at: offsetOrLen });
          }
        } else if (status === 2) {
          const header = bench.take_snapshot_chunk();
          if (header && header.byteLength > 0) {
            sync.write(header, { at: 0 });
          }
          sync.truncate(offsetOrLen);
          sync.flush();
          totalLen = offsetOrLen;
          break;
        }
      }
      sync.close();
      bench.free();
      self.postMessage({
        kind: "result",
        reqId: msg.reqId,
        value: { snapshotBytes: totalLen, kernelEstimatedBytes },
      });
      return;
    }

    if (msg.kind === "loadFromOpfs") {
      const sync = await openOpfsSync(msg.fileName, false);
      const size = sync.getSize();
      const buf = new Uint8Array(size);
      sync.read(buf, { at: 0 });
      sync.close();
      if (activeBench) {
        activeBench.free();
        activeBench = null;
      }
      activeBench = WasmBench.decode_v1_and_first_page(buf);
      const mem = trackMem();
      self.postMessage({
        kind: "result",
        reqId: msg.reqId,
        value: {
          snapshotBytes: size,
          kernelEstimatedBytes: Math.round(activeBench.estimated_bytes()),
          wasmMemoryBytes: mem,
          peakWasmMemoryBytes: peakMem,
        },
      });
      return;
    }

    if (msg.kind === "startSaveToOpfs") {
      if (!activeBench) {
        throw new Error("No active workbook loaded for save");
      }
      const sync = await openOpfsSync(msg.fileName, true);
      activeBench.start_snapshot_stream();
      saveJob = {
        reqId: msg.reqId,
        sync,
        sliceBudgetMs: msg.sliceBudgetMs ?? 4.0,
        maxBlocks: msg.maxBlocks ?? 4,
      };
      scheduleSaveSlice();
      return;
    }

    if (msg.kind === "runStorageRecalc") {
      if (!activeBench) {
        throw new Error("No active workbook loaded for recalc");
      }
      const warmup = msg.warmup ?? 2;
      const samples = msg.samples ?? 10;
      const paramCol = msg.paramCol ?? 25;
      const times = [];
      for (let i = 0; i < warmup + samples; i++) {
        const t0 = performance.now();
        const count = activeBench.step_storage_recalc(paramCol);
        const dt = performance.now() - t0;
        trackMem();
        if (i >= warmup) times.push(dt);
        if (msg.expectedCount && count !== msg.expectedCount) {
          throw new Error(\`Expected \${msg.expectedCount} recalc cells, got \${count}\`);
        }
      }
      self.postMessage({
        kind: "result",
        reqId: msg.reqId,
        value: {
          distribution: summarize(warmup, times),
          peakWasmMemoryBytes: peakMem,
        },
      });
      return;
    }

    if (msg.kind === "removeOpfsFiles") {
      for (const name of msg.files) {
        await removeOpfsFile(name);
      }
      self.postMessage({ kind: "result", reqId: msg.reqId, value: true });
      return;
    }

    throw new Error(\`Unknown worker message: \${msg.kind}\`);
  } catch (err) {
    self.postMessage({ kind: "error", reqId: msg.reqId, message: String(err) });
  }
};

self.postMessage({ kind: "ready" });
`;

const PAGE_HTML = `<!doctype html><meta charset="utf-8"><title>wasm-bench</title>
<script type="module">
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

  let nextReqId = 1;

  async function spawnWorker() {
    const worker = new Worker("/bench-worker.js", { type: "module" });
    await new Promise((resolve, reject) => {
      const onMsg = (ev) => {
        if (ev.data?.kind === "ready") {
          worker.removeEventListener("message", onMsg);
          resolve();
        }
      };
      worker.addEventListener("message", onMsg);
      worker.addEventListener("error", (err) => reject(new Error(err.message || "worker error")), {
        once: true,
      });
    });
    return worker;
  }

  function callWorker(worker, payload) {
    const reqId = nextReqId++;
    return new Promise((resolve, reject) => {
      const onMsg = (ev) => {
        const d = ev.data;
        if (d?.reqId !== reqId) return;
        worker.removeEventListener("message", onMsg);
        if (d.kind === "error") {
          reject(new Error(d.message));
        } else {
          resolve(d.kind === "result" ? d.value : d);
        }
      };
      worker.addEventListener("message", onMsg);
      worker.postMessage({ ...payload, reqId });
    });
  }

  async function runSaveWithProbes(worker, fileName, warmup, samples) {
    const saveTimes = [];
    const probeLatencies = [];
    let peakMem = 0;

    for (let i = 0; i < warmup + samples; i++) {
      const recordProbes = i >= warmup;
      let probeId = 1;
      const inFlight = new Map();
      const runProbes = [];

      const onProbeAck = (ev) => {
        const d = ev.data;
        if (d?.kind !== "probeAck") return;
        const sentAt = inFlight.get(d.id);
        if (sentAt !== undefined) {
          inFlight.delete(d.id);
          if (recordProbes) {
            runProbes.push(performance.now() - sentAt);
          }
        }
      };
      worker.addEventListener("message", onProbeAck);

      const timer = setInterval(() => {
        const id = probeId++;
        inFlight.set(id, performance.now());
        worker.postMessage({ kind: "probe", id });
      }, 25);

      const t0 = performance.now();
      const saved = await callWorker(worker, {
        kind: "startSaveToOpfs",
        fileName,
        sliceBudgetMs: 4.0,
        maxBlocks: 4,
      });
      const dt = performance.now() - t0;

      clearInterval(timer);
      // Drain any probes sent just before completion.
      if (inFlight.size > 0) {
        await new Promise((resolve) => {
          const check = () => {
            if (inFlight.size === 0) resolve();
            else setTimeout(check, 5);
          };
          setTimeout(check, 5);
        });
      }
      worker.removeEventListener("message", onProbeAck);

      if (saved.peakWasmMemoryBytes > peakMem) {
        peakMem = saved.peakWasmMemoryBytes;
      }
      if (i >= warmup) {
        saveTimes.push(dt);
        probeLatencies.push(...runProbes);
      }
    }

    return {
      save_ms: summarize(warmup, saveTimes),
      save_probe_ms: summarize(0, probeLatencies, { probe_interval_ms: 25 }),
      peakWasmMemoryBytes: peakMem,
    };
  }

  async function runStorageSuite({
    label,
    rows,
    cols,
    formulaRows,
    fileA,
    fileB,
    loadWarmup,
    loadSamples,
    saveWarmup,
    saveSamples,
    withRecalc,
    recalcWarmup,
    recalcSamples,
  }) {
    window.__log(\`[\${label}] preparing OPFS snapshot...\`);
    const prepWorker = await spawnWorker();
    const prep = await callWorker(prepWorker, {
      kind: "prepareStorage",
      fileName: fileA,
      rows,
      cols,
      formulaRows,
    });
    prepWorker.terminate();

    window.__log(
      \`[\${label}] snapshot ready (\${(prep.snapshotBytes / 1048576).toFixed(1)} MiB), measuring OPFS load...\`,
    );
    const loadTimes = [];
    let lastWorker = null;
    let loadWasmMem = 0;
    let peakMem = 0;
    let kernelEstimatedBytes = prep.kernelEstimatedBytes;

    for (let i = 0; i < loadWarmup + loadSamples; i++) {
      if (lastWorker) {
        lastWorker.terminate();
        lastWorker = null;
      }
      const w = await spawnWorker();
      const t0 = performance.now();
      const loaded = await callWorker(w, { kind: "loadFromOpfs", fileName: fileA });
      const dt = performance.now() - t0;
      loadWasmMem = loaded.wasmMemoryBytes;
      kernelEstimatedBytes = loaded.kernelEstimatedBytes;
      if (loaded.peakWasmMemoryBytes > peakMem) {
        peakMem = loaded.peakWasmMemoryBytes;
      }
      if (i >= loadWarmup) {
        loadTimes.push(dt);
      }
      lastWorker = w;
    }

    window.__log(\`[\${label}] measuring CoW OPFS save + 25 ms probes...\`);
    const saveRes = await runSaveWithProbes(lastWorker, fileB, saveWarmup, saveSamples);
    if (saveRes.peakWasmMemoryBytes > peakMem) {
      peakMem = saveRes.peakWasmMemoryBytes;
    }

    let recalcRes = null;
    if (withRecalc) {
      window.__log(\`[\${label}] measuring 100k recalc...\`);
      recalcRes = await callWorker(lastWorker, {
        kind: "runStorageRecalc",
        warmup: recalcWarmup,
        samples: recalcSamples,
        paramCol: 25,
        expectedCount: formulaRows,
      });
      if (recalcRes.peakWasmMemoryBytes > peakMem) {
        peakMem = recalcRes.peakWasmMemoryBytes;
      }
    }

    await callWorker(lastWorker, { kind: "removeOpfsFiles", files: [fileA, fileB] });
    lastWorker.terminate();

    const out = {
      snapshot_bytes: prep.snapshotBytes,
      kernel_estimated_bytes: kernelEstimatedBytes,
      load_wasm_memory_bytes: loadWasmMem,
      peak_wasm_memory_bytes: peakMem,
      load_ms: summarize(loadWarmup, loadTimes),
      save_ms: saveRes.save_ms,
      save_probe_ms: saveRes.save_probe_ms,
    };
    if (recalcRes) {
      out.recalc_100k_ms = recalcRes.distribution;
    }
    return out;
  }

  window.__runBenchmarks = async ({ onlyB4, quick }) => {
    window.__log("running core engine benchmarks (B2-B5, B7)...");
    const coreWorker = await spawnWorker();
    const core = await callWorker(coreWorker, { kind: "runCore", onlyB4, quick });
    coreWorker.terminate();

    if (onlyB4) {
      return core;
    }

    // B8: isolated fresh Worker per memory group
    window.__log("running B8 memory footprints (4 isolated WASM instances)...");
    const b8Cases = [
      ["numeric_10m", 1, 10000000],
      ["strings_1m_repeated", 2, 1000000],
      ["strings_1m_unique", 3, 1000000],
      ["formulas_1m", 4, 3000001],
    ];
    const b8_memory = {};
    for (const [key, caseId, cells] of b8Cases) {
      const w = await spawnWorker();
      b8_memory[key] = await callWorker(w, { kind: "runB8Case", caseId, cells });
      w.terminate();
    }

    // B1 + B11 (with 1M formulas) & numeric-only control
    const loadWarmup = quick ? 0 : 1;
    const loadSamples = quick ? 2 : 5;
    const saveWarmup = quick ? 0 : 1;
    const saveSamples = quick ? 2 : 3;

    const b1_b11_20m_with_1m_formulas = await runStorageSuite({
      label: "B1+B11 20M+1M formulas",
      rows: 1000000,
      cols: 20,
      formulaRows: 1000000,
      fileA: "bench-b1-f-a.bin",
      fileB: "bench-b1-f-b.bin",
      loadWarmup,
      loadSamples,
      saveWarmup,
      saveSamples,
      withRecalc: false,
    });

    const b1_b11_20m_numeric_only = await runStorageSuite({
      label: "B1+B11 20M numeric only",
      rows: 1000000,
      cols: 20,
      formulaRows: 0,
      fileA: "bench-b1-n-a.bin",
      fileB: "bench-b1-n-b.bin",
      loadWarmup,
      loadSamples,
      saveWarmup,
      saveSamples,
      withRecalc: false,
    });

    // B12: 1M x 50 (50M cells + 100k formulas)
    const b12_50m_limit = await runStorageSuite({
      label: "B12 50M+100k formulas",
      rows: 1000000,
      cols: 50,
      formulaRows: 100000,
      fileA: "bench-b12-a.bin",
      fileB: "bench-b12-b.bin",
      loadWarmup,
      loadSamples,
      saveWarmup,
      saveSamples,
      withRecalc: true,
      recalcWarmup: quick ? 1 : 2,
      recalcSamples: quick ? 3 : 10,
    });

    // B6: 100k exact bitwise VLOOKUP on 100k rows (linear scan baseline)
    window.__log("running B6 (100k exact VLOOKUP on 100k rows)...");
    const b6Worker = await spawnWorker();
    const b6_vlookup_100k_exact = await callWorker(b6Worker, { kind: "runB6", quick });
    b6Worker.terminate();

    return {
      ...core,
      b1_b11_20m_with_1m_formulas,
      b1_b11_20m_numeric_only,
      b6_vlookup_100k_exact,
      b8_memory,
      b12_50m_limit,
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
  if (pathname === "/bench-worker.js") {
    res.writeHead(200, { ...headers, "content-type": "text/javascript" }).end(WORKER_JS);
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

// Use a fresh disk-backed profile so OPFS `SyncAccessHandle` writes to real SSD
// with full disk quota instead of incognito RAM quota.
const profileDir = mkdtempSync(join(os.tmpdir(), "reticula-wasm-bench-"));
let context;
let browserVersion = "unknown";
try {
  const launcher = options.browser === "firefox" ? firefox : chromium;
  context = await launcher.launchPersistentContext(profileDir, {
    ...(options.browser === "chrome" ? { channel: "chrome" } : {}),
    headless: true,
  });
  browserVersion = context.browser()?.version() ?? "unknown";
  const page = context.pages()[0] ?? (await context.newPage());
  page.setDefaultTimeout(300_000);
  await page.exposeFunction("__log", (line) => {
    console.error(`[wasm-bench] ${line}`);
  });
  await page.goto(url);
  await page.waitForFunction(() => window.__runBenchmarks !== undefined);
  const metrics = await page.evaluate(
    (cfg) => window.__runBenchmarks(cfg),
    { onlyB4: options["only-b4"], quick: options.quick },
  );

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
} finally {
  if (context) {
    await context.close();
  }
  server.close();
  try {
    rmSync(profileDir, { recursive: true, force: true });
  } catch {
    // Ignore transient Windows lock on Chromium profile cleanup.
  }
}
