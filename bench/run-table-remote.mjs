// A data table of ten million rows that is on a server, over a slow network.
//
//   node bench/run-table-remote.mjs [options]
//
// The real server (a debug build, for its development sign-in) is behind a proxy
// that holds every answer back by a round trip and lets the bytes of all answers
// together through at a rate, as spikes/startup/serve.mjs does: the link is the
// same whichever thread of the page asks. A table that the app makes up is put
// on the server without it, and every round opens the workbook in a browser with
// a profile of its own, clicks on the table's tab, and jumps about in it.
//
// Needs a database (RETICULA_TEST_DATABASE_URL, or that of `cargo xtask db up`),
// target/debug/reticula-server and apps/web/dist. Writes a report, and nothing
// to bench/results.
//
// Options:
//   --browser <name>    chrome (the default), chromium or firefox
//   --rows <n>          rows of the table (default 10,000,000)
//   --rounds <n>        profiles (default 5)
//   --jumps <n>         jumps in each (default 40)
//   --idle <s>          between the first screen and the first jump (default 0)
//   --mbit <n>          the rate down (default 20), and --rtt <ms> (default 50)
//   --visitors <n>      instead: so many tabs of people who are not signed in, at
//                       one address, look at the table for --seconds (default 300)
//   --affinity <mask>   pin this process, and so the browser
//   --out <file>        where the report goes (default target/bench-tables/remote.json)

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer, request as ask } from "node:http";
import { connect } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { chromium, firefox } from "@playwright/test";
import { Random, summarize } from "../apps/web/src/bench/stats.ts";
import { root } from "../spikes/grid/serve.mjs";
import { commit, machine } from "../spikes/machine.mjs";
import { pin } from "../spikes/pin.mjs";
import { writeReport } from "./archive.mjs";

const { values: given } = parseArgs({
  options: {
    browser: { type: "string", default: "chrome" },
    rows: { type: "string", default: "10000000" },
    rounds: { type: "string", default: "5" },
    jumps: { type: "string", default: "40" },
    idle: { type: "string", default: "0" },
    mbit: { type: "string", default: "20" },
    rtt: { type: "string", default: "50" },
    visitors: { type: "string" },
    seconds: { type: "string", default: "300" },
    affinity: { type: "string", default: "" },
    out: { type: "string", default: join(root, "target/bench-tables/remote.json") },
  },
});
const [rows, rounds, jumps, idle, mbit, rtt, visitors, seconds] = [given.rows, given.rounds, given.jumps, given.idle, given.mbit, given.rtt, given.visitors ?? "0", given.seconds].map(Number);
if (![rows, rounds, jumps, idle, mbit, rtt, visitors, seconds].every((value) => value >= 0)) {
  throw new Error("numbers are to be numbers");
}
if (given.affinity) {
  pin(given.affinity);
}

// --- the server and the network before it ----------------------------------

const SEGMENT = 1460;
let isSlow = false;
let linkFreeAt = 0;
const answers = new Map();
const bytes = { sent: 0 };

/** Not a millisecond late: see spikes/startup/serve.mjs. */
async function until(time) {
  for (;;) {
    const left = time - performance.now();
    if (left <= 0) {
      return;
    }
    await new Promise((done) => (left > 20 ? setTimeout(done, left - 20) : setImmediate(done)));
  }
}

async function slowly(source, target) {
  const bytesPerMs = (mbit * 1e6) / 8 / 1000;
  for await (const chunk of source) {
    for (let sent = 0; sent < chunk.length; sent += SEGMENT) {
      const segment = chunk.subarray(sent, sent + SEGMENT);
      bytes.sent += segment.length;
      if (isSlow) {
        linkFreeAt = Math.max(performance.now(), linkFreeAt) + segment.length / bytesPerMs;
        await until(linkFreeAt);
      }
      if (!target.write(segment) && !target.destroyed) {
        await new Promise((resolve) => {
          const done = () => {
            target.off("drain", done).off("close", done);
            resolve();
          };
          target.on("drain", done).on("close", done);
        });
      }
      if (target.destroyed) {
        source.destroy();
        return;
      }
    }
  }
  target.end();
}

const freePort = async () => {
  const server = createServer();
  await new Promise((ready) => server.listen(0, "127.0.0.1", ready));
  const { port } = server.address();
  await new Promise((closed) => server.close(closed));
  return port;
};

const port = await freePort();
const proxy = createServer(async (incoming, outgoing) => {
  const arrived = performance.now();
  const onward = ask({ host: "127.0.0.1", port, method: incoming.method, path: incoming.url, headers: incoming.headers }, async (answer) => {
    const status = answer.statusCode ?? 502;
    answers.set(status, (answers.get(status) ?? 0) + 1);
    if (isSlow) {
      await until(arrived + rtt);
    }
    outgoing.writeHead(status, answer.headers);
    await slowly(answer, outgoing).catch(() => outgoing.destroy());
  });
  onward.on("error", () => outgoing.destroy());
  outgoing.on("close", () => onward.destroy());
  incoming.pipe(onward);
});
proxy.on("upgrade", (incoming, socket, head) => {
  const through = connect(port, "127.0.0.1", () => {
    const lines = incoming.rawHeaders.flatMap((name, at) => (at % 2 === 0 ? [`${name}: ${incoming.rawHeaders[at + 1]}`] : []));
    through.write(`${incoming.method} ${incoming.url} HTTP/1.1\r\n${lines.join("\r\n")}\r\n\r\n`);
    through.write(head);
    socket.pipe(through);
    through.on("data", (data) => {
      // Nothing large comes this way; it comes a round trip later all the same.
      setTimeout(() => socket.writable && socket.write(data), isSlow ? rtt : 0);
    });
  });
  for (const end of [socket, through]) {
    for (const event of ["error", "close"]) {
      end.on(event, () => {
        socket.destroy();
        through.destroy();
      });
    }
  }
});
await new Promise((ready) => proxy.listen(0, "127.0.0.1", ready));
const baseUrl = `http://127.0.0.1:${proxy.address().port}`;

const dataDir = mkdtempSync(join(tmpdir(), "reticula-remote-"));
const server = spawn(join(root, "target/debug", process.platform === "win32" ? "reticula-server.exe" : "reticula-server"), [], {
  env: {
    ...process.env,
    BIND_ADDR: `127.0.0.1:${port}`,
    RETICULA_STATIC_DIR: join(root, "apps/web/dist"),
    RETICULA_DATA_DIR: dataDir,
    RETICULA_DATABASE_URL: process.env.RETICULA_TEST_DATABASE_URL ?? "postgres://reticula:reticula_local_dev@127.0.0.1:5434/reticula",
    RETICULA_PUBLIC_ORIGIN: baseUrl,
    RETICULA_AUTH_MODE: "dev-mock",
    RETICULA_LLM_MODE: "mock",
  },
  stdio: ["ignore", "ignore", "inherit"],
});
for (const deadline = Date.now() + 20_000; ; ) {
  if (await fetch(`${baseUrl}/readyz`).then(({ ok }) => ok, () => false)) {
    break;
  }
  if (Date.now() > deadline) {
    throw new Error("the server did not start");
  }
  await new Promise((later) => setTimeout(later, 100));
}

// --- the browser -----------------------------------------------------------

const type = given.browser === "firefox" ? firefox : chromium;
const launched = await type.launch({ headless: true, ...(given.browser === "firefox" ? {} : { channel: given.browser === "chrome" ? "chrome" : "chromium" }) });
const problems = [];
const ready = (page) => page.waitForFunction(() => window.__reticula?.docStatus().engineStatus === "ready", undefined, { polling: 50, timeout: 120_000 });

async function newContext(account) {
  const context = await launched.newContext({ viewport: { width: 1920, height: 1080 }, locale: "en-US" });
  await context.addInitScript({ path: join(root, "bench/table-page.js") });
  let userId = "";
  if (account) {
    const answer = await context.request.post(`${baseUrl}/api/auth/dev-login`, { headers: { origin: baseUrl }, data: { account } });
    userId = (await answer.json()).id;
  }
  return { context, userId };
}

const api = async (context, method, path, data) => {
  const answer = await context.request.fetch(`${baseUrl}/api${path}`, { method, headers: { origin: baseUrl }, ...(data ? { data } : {}) });
  if (!answer.ok()) {
    throw new Error(`${method} ${path}: ${answer.status()} ${await answer.text()}`);
  }
  const text = await answer.text();
  return text ? JSON.parse(text) : null;
};

/** Makes the table up, and puts it on the server. Gives how long sorting it here takes as well. */
async function putTable(context, docId) {
  const page = await context.newPage();
  await page.goto(`${baseUrl}/app?hooks&doc=source-${randomUUID()}`);
  await ready(page);
  const made = await page.evaluate(
    async ([id, count]) => {
      const { TableClient } = await window.__reticula.tables();
      const local = new TableClient({ docId: `made-${id}`, tabId: crypto.randomUUID() }, {});
      await local.acquire();
      const { entry } = await local.sample("orders", count, 7);
      const file = await local.export(entry.id);
      const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", await file.arrayBuffer()));
      const sha256 = Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join("");
      const url = `/api/workbooks/${id}/tables`;
      const send = async (path, method, headers, body) => {
        for (;;) {
          const answer = await fetch(url + path, { method, headers, body: body ?? null });
          if (answer.ok) {
            return answer.json();
          }
          if (answer.status !== 429) {
            throw new Error(`${method} ${path}: ${answer.status} ${await answer.text()}`);
          }
          await new Promise((later) => setTimeout(later, 1_000 * Number(answer.headers.get("Retry-After") ?? 1)));
        }
      };
      const table = crypto.randomUUID();
      await send("", "POST", { "Content-Type": "application/json" }, JSON.stringify({ id: table, name: "orders", bytes: file.size, sha256 }));
      for (let at = 0; at < file.size; at += 8 << 20) {
        await send(`/${table}/data`, "PATCH", { "Upload-Offset": `${at}` }, file.slice(at, at + (8 << 20)));
      }
      await send(`/${table}/complete`, "POST", {});
      await local.delete(entry.id);
      await local.close();
      return { bytes: file.size };
    },
    [docId, rows],
  );
  await page.close();
  return made;
}

const shown = () => window.__b14.isOpen() && window.__b14.isWhole();

async function round(docId, positions) {
  const { context } = await newContext("alice");
  const page = await context.newPage();
  page.setDefaultTimeout(300_000);
  page.on("pageerror", (error) => problems.push(String(error)));
  isSlow = true;
  await page.goto(`${baseUrl}/app?hooks&collab=${docId}`);
  await ready(page);
  const tab = page.getByRole("tab", { name: "orders", exact: true });
  await tab.waitFor();
  // What the editor fetches of its own accord is not part of it.
  await page.waitForTimeout(3_000);
  const before = bytes.sent;
  const firstMs = await page.evaluate(async () => {
    const tabs = [...document.querySelectorAll('[role="tab"]')];
    const started = performance.now();
    tabs.find((one) => one.textContent?.trim() === "orders")?.click();
    await window.__b14.until("the table", () => window.__b14.isOpen() && window.__b14.isWhole());
    return performance.now() - started;
  });
  const firstBytes = bytes.sent - before;
  await page.waitForTimeout(idle * 1_000);
  const jumpMs = await page.evaluate((at) => window.__b14.jumps(at), positions);
  const jumpBytes = bytes.sent - before - firstBytes;
  isSlow = false;
  return { context, page, firstMs, firstBytes, jumpMs, jumpBytes };
}

const list = (times) => times.map((one) => one.toFixed(0)).join(", ");

/** How long some sorts take, each of a column of its own. */
async function sorts(page) {
  const times = [];
  for (const column of [1, 4, 8]) {
    times.push((await page.evaluate((c) => window.__b14.ask({ filters: [], order: [{ column: c, descending: true }] }), column)).ms);
  }
  return times;
}

/** The same of a table like it that is not on a server. */
async function sortsHere() {
  const { context } = await newContext();
  const page = await context.newPage();
  page.setDefaultTimeout(300_000);
  await page.goto(`${baseUrl}/app?hooks&doc=orders-10m&sample=${rows}`);
  await page.waitForFunction(() => window.__b14.isOpen() && window.__b14.isWhole(), undefined, { polling: 200 });
  await page.evaluate(() => window.__b14.ask({ filters: [], order: [{ column: 0, descending: true }] }));
  const times = await sorts(page);
  await context.close();
  return times;
}

const report = { date: new Date().toISOString(), ...commit(), machine: machine(), affinity: given.affinity || null, browser: { name: given.browser, version: launched.version(), headless: true }, network: { mbit, rttMs: rtt }, rows, idleSeconds: idle, jumps };
const owner = await newContext("alice");
const { id: docId } = await api(owner.context, "POST", "/workbooks", { title: "Remote" });
try {
  const { bytes: fileBytes } = await putTable(owner.context, docId);
  report.fileBytes = fileBytes;
  console.log(`${rows.toLocaleString("en-US")} rows, ${(fileBytes / 1e6).toFixed(1)} MB; ${given.browser} ${launched.version()}`);
  const random = new Random(21);
  if (visitors > 0) {
    const { url } = await api(owner.context, "POST", `/workbooks/${docId}/share-links`, { role: "viewer" });
    await api(owner.context, "PATCH", `/workbooks/${docId}`, { visibility: "link_view" });
    const token = /[^/=]+$/.exec(url)?.[0];
    const { context } = await newContext();
    // People do not all come in the same instant.
    const pages = await Promise.all(
      Array.from({ length: visitors }, async () => {
        const page = await context.newPage();
        page.setDefaultTimeout(300_000);
        await page.goto(`${baseUrl}/app?hooks&collab=${docId}&share=${token}`);
        await page.getByRole("tab", { name: "orders", exact: true }).click();
        await page.waitForFunction(shown, undefined, { polling: 200 });
        return page;
      }),
    );
    console.log(`  ${visitors} tabs have the table open`);
    const seen = async () => {
      const end = Date.now() + seconds * 1_000;
      const counts = await Promise.all(
        pages.map(async (page) => {
          let count = 0;
          while (Date.now() < end) {
            await page.evaluate((at) => window.__reticula.table().scrollTo({ top: Math.floor(at * window.__reticula.table().stats().scrollLimits.top) }), random.unit());
            await page.waitForFunction(shown, undefined, { polling: 100 });
            await page.waitForTimeout(1_500);
            count++;
          }
          return count;
        }),
      );
      return counts.reduce((sum, count) => sum + count, 0);
    };
    const from = new Map(answers);
    report.visitors = { tabs: visitors, seconds, screens: await seen(), answers: Object.fromEntries([...answers].map(([status, count]) => [status, count - (from.get(status) ?? 0)])), all: Object.fromEntries(answers) };
    console.log(JSON.stringify(report.visitors));
    await context.close();
  } else {
    const made = [];
    for (let at = 0; at < rounds; at++) {
      const { context, page, ...one } = await round(docId, Array.from({ length: jumps }, () => random.unit()));
      made.push(one);
      console.log(`  first screen ${one.firstMs.toFixed(0)} ms (${(one.firstBytes / 1e6).toFixed(2)} MB), jumps p50 ${summarize(one.jumpMs).p50.toFixed(0)} p95 ${summarize(one.jumpMs).p95.toFixed(0)} ms (${(one.jumpBytes / 1e6).toFixed(2)} MB)`);
      if (at === rounds - 1) {
        // All of it, without the slow network, and then as if it were here.
        report.firstSort = await page.evaluate(() => window.__b14.ask({ filters: [], order: [{ column: 0, descending: true }] }));
        report.sortMs = { remote: await sorts(page), here: await sortsHere() };
        console.log(`  a sort, with the rest of the table coming: ${report.firstSort.ms.toFixed(0)} ms; afterwards ${list(report.sortMs.remote)} ms, and ${list(report.sortMs.here)} of a table that is kept here`);
      }
      await context.close();
    }
    report.rounds = made;
    report.firstMs = summarize(made.map((one) => one.firstMs));
    report.jumpMs = summarize(made.flatMap((one) => one.jumpMs));
    // Nothing of the dictionaries is there before the first jump.
    report.laterJumpMs = summarize(made.flatMap((one) => one.jumpMs.slice(1)));
    console.log(`first screen p50 ${report.firstMs.p50.toFixed(0)}, worst ${report.firstMs.max?.toFixed(0)} ms; jumps p50 ${report.jumpMs.p50.toFixed(0)}, p95 ${report.jumpMs.p95.toFixed(0)} ms`);
  }
} finally {
  isSlow = false;
  await api(owner.context, "DELETE", `/workbooks/${docId}`).catch((error) => problems.push(String(error)));
  report.problems = problems;
  writeReport(given.out, report);
  await launched.close();
  server.kill("SIGKILL");
  proxy.closeAllConnections();
  proxy.close();
  await new Promise((later) => setTimeout(later, 500));
  rmSync(dataDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 300 });
}
console.log(`report written to ${given.out}`);
if (problems.length > 0) {
  console.log(problems.join("\n"));
  process.exitCode = 1;
}
