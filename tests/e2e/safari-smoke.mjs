// Production Safari smoke suite driven through macOS `safaridriver` (W3C WebDriver).
//
// Serves the built production bundle (`apps/web/dist`) with COOP/COEP headers and
// verifies real Safari behaviour on `/app?hooks`:
//   1. OPFS `FileSystemSyncAccessHandle` (`opfs-sync`) startup & formula recalculation;
//   2. CoW `RTCLSNAP` v1 compaction + `RTCLWAL1` persistence across a full reload;
//   3. Dual-tab single-instance Web Lock (`blocked`) and graceful takeover (`yielded` -> `ready`);
//   4. Hung-worker takeover exercising the 500 ms watchdog & Safari `InvalidStateError` recovery;
//   5. Determinism digest match against `tests/determinism/expected.txt`;
//   6. The renderer the grid gets, and how far what WebGPU draws is from a 2D canvas.
//
// Usage:
//   node tests/e2e/safari-smoke.mjs [--dist apps/web/dist] [--out target/safari-smoke/report.json]

import { spawn } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { dirname, extname, join, resolve } from "node:path";

const options = {
  dist: "apps/web/dist",
  out: "target/safari-smoke/report.json",
  driverPort: 4444,
};

const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const arg = argv[i];
  const key = arg?.replace(/^--/, "");
  const val = argv[++i];
  if (key === "driver-port") {
    options.driverPort = Number(val);
    continue;
  }
  if (!key || !(key in options) || val === undefined) {
    console.error(`unknown or incomplete option: ${arg}`);
    process.exit(2);
  }
  options[key] = val;
}

const distDir = resolve(options.dist);
const expectedDigest = readFileSync(resolve("tests/determinism/expected.txt"), "utf8").trim();

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
};

function startStaticServer() {
  const server = createServer(async (req, res) => {
    const pathname = new URL(req.url ?? "/", "http://localhost").pathname;
    const isAsset = pathname.startsWith("/assets/");
    const rel = isAsset || pathname === "/sw.js" ? pathname.slice(1) : "index.html";
    const full = join(distDir, rel);
    try {
      const body = await readFile(full);
      const ext = extname(full);
      res
        .writeHead(200, {
          "content-type": MIME[ext] ?? "application/octet-stream",
          "cache-control": isAsset ? "public, max-age=31536000, immutable" : "no-cache",
          "cross-origin-opener-policy": "same-origin",
          "cross-origin-embedder-policy": "require-corp",
        })
        .end(body);
    } catch {
      res.writeHead(404).end();
    }
  });
  return new Promise((ready) => {
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      ready({
        origin: `http://localhost:${port}`,
        stop: () => {
          server.closeAllConnections();
          return new Promise((done) => server.close(() => done()));
        },
      });
    });
  });
}

async function startSafariDriver(port) {
  const driver = spawn("safaridriver", ["--port", String(port)], { stdio: "inherit" });
  const baseUrl = `http://localhost:${port}`;
  for (let attempt = 0; attempt < 100; attempt++) {
    try {
      const resp = await fetch(`${baseUrl}/status`);
      if (resp.ok) {
        return { baseUrl, stop: () => driver.kill() };
      }
    } catch {
      // Not listening yet.
    }
    await sleep(100);
  }
  driver.kill();
  throw new Error("safaridriver did not become ready");
}

async function wdRequest(baseUrl, method, path, body) {
  const resp = await fetch(`${baseUrl}${path}`, {
    method,
    headers: { "content-type": "application/json; charset=utf-8" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const json = await resp.json();
  if (!resp.ok) {
    throw new Error(`${method} ${path} failed: ${json?.value?.error}: ${json?.value?.message}`);
  }
  return json.value;
}

/**
 * Has the grid drawn in several states, with the renderer it has and then on a
 * 2D canvas, and compares. Runs in the page, which must be drawn with WebGPU.
 */
async function compareRenderers() {
  const hooks = window.__reticula;
  const nextFrame = () => new Promise((resolve) => requestAnimationFrame(resolve));
  const waitFor = async (label, test) => {
    for (let frame = 0; !test(); frame++) {
      if (frame > 600) {
        throw new Error(`timed out waiting for ${label}`);
      }
      await nextFrame();
    }
  };
  const zoomTo = async (percent) => {
    document.querySelector('[data-testid="zoom-value"]').click();
    await waitFor("the zoom steps", () =>
      document.querySelector(`[data-testid="zoom-${percent}"]`),
    );
    document.querySelector(`[data-testid="zoom-${percent}"]`).click();
    await waitFor("the zoom", () => hooks.stats().zoom === percent / 100);
  };
  const states = [
    { label: "top", scroll: { top: 0, left: 0 }, zoom: 100 },
    { label: "scrolled", scroll: { top: 247, left: 131 }, zoom: 100 },
    { label: "zoomed out", scroll: { top: 60, left: 0 }, zoom: 50 },
    { label: "zoomed in", scroll: { top: 33, left: 71 }, zoom: 150 },
  ];
  const pictures = async () => {
    const taken = [];
    for (const { scroll, zoom } of states) {
      await zoomTo(zoom);
      hooks.scrollTo(scroll);
      await nextFrame();
      await waitFor("the pages", () => hooks.stats().missingCells === 0);
      hooks.selectRange({ row: 2, col: 1 }, { row: 5, col: 2 });
      // After the grid's own callback, which selecting has requested.
      await nextFrame();
      const canvas = [...document.querySelectorAll(".reticula-grid canvas")].at(-1);
      const copy = document.createElement("canvas");
      copy.width = canvas.width;
      copy.height = canvas.height;
      const context = copy.getContext("2d");
      context.drawImage(canvas, 0, 0);
      taken.push(context.getImageData(0, 0, copy.width, copy.height).data);
    }
    return taken;
  };
  const drawn = await pictures();
  hooks.setRenderer("canvas2d");
  await waitFor("a 2D canvas", () => hooks.stats().renderer.kind === "canvas2d");
  await nextFrame();
  const wanted = await pictures();
  return states.map(({ label }, index) => {
    const a = drawn[index];
    const b = wanted[index];
    const result = {
      label,
      pixels: b.length / 4,
      drawn: 0,
      differing: 0,
      over16: 0,
      over96: 0,
      most: 0,
    };
    for (let at = 0; at < b.length; at += 4) {
      let apart = 0;
      let drawn = false;
      for (let channel = at; channel < at + 4; channel++) {
        apart = Math.max(apart, Math.abs(a[channel] - b[channel]));
        drawn ||= b[channel] !== b[channel % 4];
      }
      result.drawn += drawn ? 1 : 0;
      result.differing += apart > 0 ? 1 : 0;
      result.over16 += apart > 16 ? 1 : 0;
      result.over96 += apart > 96 ? 1 : 0;
      result.most = Math.max(result.most, apart);
    }
    return result;
  });
}

function assert(condition, message) {
  if (!condition) {
    throw new Error(`Assertion failed: ${message}`);
  }
}

async function waitUntil(label, fn, timeoutMs = 20_000, intervalMs = 100) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      const val = await fn();
      if (val) {
        return val;
      }
    } catch (err) {
      lastError = err;
    }
    await sleep(intervalMs);
  }
  const suffix = lastError ? ` (last error: ${lastError.message})` : "";
  throw new Error(`Timed out waiting for ${label} after ${timeoutMs} ms${suffix}`);
}

const server = await startStaticServer();
const driver = await startSafariDriver(options.driverPort);
let sessionId = null;

try {
  const created = await wdRequest(driver.baseUrl, "POST", "/session", {
    capabilities: { alwaysMatch: { browserName: "safari" } },
  });
  sessionId = created.sessionId;
  const safariVersion = created.capabilities?.browserVersion ?? "unknown";
  const sess = `/session/${sessionId}`;
  await wdRequest(driver.baseUrl, "POST", `${sess}/timeouts`, {
    script: 60_000,
    pageLoad: 60_000,
  });

  const keepAliveWindow = await wdRequest(driver.baseUrl, "GET", `${sess}/window`);
  let activeWindow = keepAliveWindow;

  async function switchTo(handle) {
    if (activeWindow !== handle) {
      await wdRequest(driver.baseUrl, "POST", `${sess}/window`, { handle });
      activeWindow = handle;
    }
  }

  async function execSync(handle, script, args = []) {
    await switchTo(handle);
    return wdRequest(driver.baseUrl, "POST", `${sess}/execute/sync`, { script, args });
  }

  async function execAsync(handle, script, args = []) {
    await switchTo(handle);
    return wdRequest(driver.baseUrl, "POST", `${sess}/execute/async`, { script, args });
  }

  async function openTab(url) {
    const win = await wdRequest(driver.baseUrl, "POST", `${sess}/window/new`, { type: "tab" });
    await switchTo(win.handle);
    await wdRequest(driver.baseUrl, "POST", `${sess}/url`, { url });
    return win.handle;
  }

  async function describeTab(handle) {
    try {
      return await execSync(
        handle,
        `
        const h = window.__reticula;
        return {
          href: location.href,
          docStatus: h ? h.docStatus() : null,
          stats: h ? { pagesHeld: h.stats().pagesHeld, missingCells: h.stats().missingCells } : null,
          error: document.querySelector('[data-testid="engine-error"]')?.textContent ?? null,
          overlay: document.querySelector('[data-testid="lock-overlay-message"]')?.textContent ?? null,
        };
        `,
      );
    } catch (err) {
      return { inspectError: err instanceof Error ? err.message : String(err) };
    }
  }

  async function waitReady(handle, timeoutMs = 20_000) {
    try {
      return await waitUntil(
        "engine ready & settled",
        () =>
          execSync(
            handle,
            `
            const h = window.__reticula;
            if (!h) return null;
            const st = h.docStatus();
            const gs = h.stats();
            if (st.engineStatus === "ready" && gs.pagesHeld > 0 && gs.missingCells === 0) {
              return st;
            }
            return null;
            `,
          ),
        timeoutMs,
      );
    } catch (err) {
      const diag = await describeTab(handle);
      throw new Error(
        `${err instanceof Error ? err.message : String(err)} — tab state: ${JSON.stringify(diag)}`,
      );
    }
  }

  async function waitStatus(handle, expected, timeoutMs = 15_000) {
    try {
      return await waitUntil(
        `engineStatus === "${expected}"`,
        () =>
          execSync(
            handle,
            `
            const st = window.__reticula?.docStatus();
            return st && st.engineStatus === arguments[0] ? st : null;
            `,
            [expected],
          ),
        timeoutMs,
      );
    } catch (err) {
      const diag = await describeTab(handle);
      throw new Error(
        `${err instanceof Error ? err.message : String(err)} — tab state: ${JSON.stringify(diag)}`,
      );
    }
  }

  async function editCell(handle, row, col, address, raw, expectedText) {
    const beforeSeq = Number(
      await execSync(handle, `return window.__reticula?.docStatus().walSeq ?? "0";`),
    );
    await execSync(handle, `window.__reticula.select(arguments[0], arguments[1]);`, [row, col]);
    await waitUntil(`name-box === ${address} & editable formula-input`, () =>
      execSync(
        handle,
        `
        const nb = document.querySelector('[data-testid="name-box"]')?.textContent?.trim();
        const fi = document.querySelector('[data-testid="formula-input"]');
        return nb === arguments[0] && fi?.getAttribute("contenteditable") === "true";
        `,
        [address],
      ),
    );
    await switchTo(handle);
    const elem = await wdRequest(driver.baseUrl, "POST", `${sess}/element`, {
      using: "css selector",
      value: '[data-testid="formula-input"]',
    });
    const elemId = Object.values(elem)[0];
    await wdRequest(driver.baseUrl, "POST", `${sess}/element/${elemId}/click`, {});
    await wdRequest(driver.baseUrl, "POST", `${sess}/element/${elemId}/value`, {
      text: `${raw}\uE007`,
    });
    await waitUntil(`walSeq > ${beforeSeq} & textAt(${row},${col}) === ${expectedText}`, () =>
      execSync(
        handle,
        `
        const h = window.__reticula;
        if (!h) return false;
        const seq = Number(h.docStatus().walSeq ?? "0");
        return seq > arguments[0] && h.textAt(arguments[1], arguments[2]) === arguments[3];
        `,
        [beforeSeq, row, col, expectedText],
      ),
    );
  }

  async function clickTestId(handle, testId) {
    await switchTo(handle);
    const elem = await wdRequest(driver.baseUrl, "POST", `${sess}/element`, {
      using: "css selector",
      value: `[data-testid="${testId}"]`,
    });
    const elemId = Object.values(elem)[0];
    await wdRequest(driver.baseUrl, "POST", `${sess}/element/${elemId}/click`, {});
  }

  const docId = `safari-smoke-${Date.now()}`;
  const docUrl = `${server.origin}/app?hooks&doc=${docId}`;
  const startedAt = Date.now();

  // 1. Open Tab 1 on production `/app` and verify OPFS SyncAccessHandle backend.
  console.log(`[safari-smoke] Safari ${safariVersion} — opening ${docUrl}`);
  const tab1 = await openTab(docUrl);
  const st1 = await waitReady(tab1);
  assert(
    st1.storageBackend === "opfs-sync",
    `expected opfs-sync backend on Safari, got ${st1.storageBackend}`,
  );
  console.log(`[safari-smoke] step 1 ok: backend=${st1.storageBackend}`);

  // 2. Edit values & formula, compact CoW RTCLSNAP v1 snapshot, edit WAL on top, reload & verify.
  await editCell(tab1, 0, 0, "A1", "15", "15");
  await editCell(tab1, 1, 0, "A2", "27", "27");
  await editCell(tab1, 2, 0, "A3", "=SUM(A1:A2)", "42");

  const snapResult = await execAsync(
    tab1,
    `
    const done = arguments[arguments.length - 1];
    window.__reticula.compactSnapshot().then(
      (res) => done({ ok: true, res }),
      (err) => done({ ok: false, err: String(err) }),
    );
    `,
  );
  assert(snapResult.ok, `compactSnapshot failed: ${snapResult.err}`);
  assert(snapResult.res.slot === "b", `expected slot "b", got ${snapResult.res.slot}`);
  assert(snapResult.res.byteLen > 0, "expected non-empty snapshot");

  await editCell(tab1, 3, 0, "A4", "=A3*2", "84");
  await sleep(100);

  await switchTo(tab1);
  await wdRequest(driver.baseUrl, "POST", `${sess}/refresh`, {});
  const afterReload = await waitReady(tab1);
  assert(
    afterReload.activeSlot === "b",
    `expected activeSlot "b" after reload, got ${afterReload.activeSlot}`,
  );
  const persisted = await execSync(
    tab1,
    `return [0, 1, 2, 3].map((r) => window.__reticula.textAt(r, 0));`,
  );
  assert(
    JSON.stringify(persisted) === JSON.stringify(["15", "27", "42", "84"]),
    `unexpected persisted cells after reload: ${JSON.stringify(persisted)}`,
  );
  console.log(
    `[safari-smoke] step 2 ok: slot=${afterReload.activeSlot} bytes=${snapResult.res.byteLen}`,
  );

  // 3. Open Tab 2 on the same docId -> blocked by Web Lock -> graceful takeover!
  const tab2 = await openTab(docUrl);
  assert(tab2 !== tab1, "expected distinct WebDriver tab handles for tab1 and tab2");
  await waitStatus(tab2, "blocked");
  const overlayText = await execSync(
    tab2,
    `return document.querySelector('[data-testid="lock-overlay"]')?.textContent ?? "";`,
  );
  assert(
    overlayText.includes("open in another tab"),
    `expected lock-overlay prompt, got: ${overlayText}`,
  );

  await clickTestId(tab2, "take-over-btn");
  await waitReady(tab2);
  await waitStatus(tab1, "yielded");
  const tab2A4 = await execSync(tab2, `return window.__reticula.textAt(3, 0);`);
  assert(tab2A4 === "84", `expected Tab 2 A4 === "84" after takeover, got ${tab2A4}`);
  console.log("[safari-smoke] step 3 ok: dual-tab lock & graceful takeover verified");

  // 4. Hung-worker takeover (500 ms watchdog + Safari InvalidStateError / steal path).
  await editCell(tab2, 0, 1, "B1", "BeforeHang", "BeforeHang");
  await sleep(100);
  await execSync(tab2, `window.__reticula.spinWorker(2500);`);

  await clickTestId(tab1, "take-over-btn");
  const tab1Recovered = await waitReady(tab1, 20_000);
  assert(
    tab1Recovered.storageBackend === "opfs-sync",
    `expected opfs-sync after hung takeover, got ${tab1Recovered.storageBackend}`,
  );
  const recoveredCells = await execSync(
    tab1,
    `return [window.__reticula.textAt(3, 0), window.__reticula.textAt(0, 1)];`,
  );
  assert(
    JSON.stringify(recoveredCells) === JSON.stringify(["84", "BeforeHang"]),
    `unexpected cells after hung takeover: ${JSON.stringify(recoveredCells)}`,
  );
  console.log("[safari-smoke] step 4 ok: hung-worker takeover verified");

  // 5. Determinism self-check on Safari WebKit WASM.
  const digestRes = await execAsync(
    tab1,
    `
    const done = arguments[arguments.length - 1];
    window.__reticula.selfCheck().then(
      (digest) => done({ ok: true, digest }),
      (err) => done({ ok: false, err: String(err) }),
    );
    `,
  );
  assert(digestRes.ok, `selfCheck failed: ${digestRes.err}`);
  assert(
    digestRes.digest === expectedDigest,
    `determinism digest mismatch: got ${digestRes.digest}, expected ${expectedDigest}`,
  );

  // 6. The renderer: by itself, and when WebGPU is asked for.
  const automatic = await execSync(tab1, `return window.__reticula.stats().renderer;`);
  assert(
    automatic.kind === "canvas2d",
    `expected Canvas 2D by default, got ${JSON.stringify(automatic)}`,
  );
  const gpuTab = await openTab(`${server.origin}/app?hooks&renderer=webgpu&doc=${docId}-webgpu`);
  await waitReady(gpuTab);
  await editCell(gpuTab, 0, 0, "A1", "Overhead 永东国酱 fjord", "Overhead 永东国酱 fjord");
  await editCell(gpuTab, 2, 1, "B3", "0.25", "0.25");
  const asked = await waitUntil("the renderer to be decided on", () =>
    execSync(
      gpuTab,
      `
      const { renderer } = window.__reticula.stats();
      return renderer.reason === "starting" ? null : renderer;
      `,
    ),
  );
  let compared = null;
  if (asked.kind === "webgpu") {
    const comparison = await execAsync(
      gpuTab,
      `
      const done = arguments[arguments.length - 1];
      (${compareRenderers})().then(
        (states) => done({ ok: true, states }),
        (err) => done({ ok: false, err: String(err) }),
      );
      `,
    );
    assert(comparison.ok, `comparing the renderers failed: ${comparison.err}`);
    compared = comparison.states;
    for (const state of compared) {
      // Whether text is close enough for WebGPU to be the default is for whoever
      // reads the report to say; here it only has to be the same sheet.
      assert(
        state.drawn > 0 && state.over96 <= state.drawn * 0.02,
        `WebGPU draws something else: ${JSON.stringify(state)}`,
      );
    }
  }
  console.log(`[safari-smoke] step 6 ok: ${asked.kind} (${asked.reason ?? asked.adapter})`);

  const report = {
    browser: "safari",
    version: safariVersion,
    docId,
    elapsedMs: Date.now() - startedAt,
    storageBackend: tab1Recovered.storageBackend,
    snapshotSlot: snapResult.res.slot,
    snapshotBytes: snapResult.res.byteLen,
    determinismDigest: digestRes.digest,
    checks: {
      opfsSyncStartup: true,
      formulaEditAndCompactionReload: true,
      dualTabLockAndGracefulTakeover: true,
      hungWorkerTakeover: true,
      determinismDigestMatch: true,
    },
    renderer: {
      automatic: { kind: automatic.kind, reason: automatic.reason },
      asked: { kind: asked.kind, reason: asked.reason, adapter: asked.adapter },
      counters: asked.counters,
      compared,
    },
  };

  const outPath = resolve(options.out);
  mkdirSync(dirname(outPath), { recursive: true });
  writeFileSync(outPath, `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report, null, 2));
} finally {
  if (sessionId) {
    await wdRequest(driver.baseUrl, "DELETE", `/session/${sessionId}`).catch(() => {});
  }
  driver.stop();
  await server.stop();
}
