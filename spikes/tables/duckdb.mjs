// Measures DuckDB-WASM on one table: importing a CSV, reading screens of
// rows, sorting, filtering, aggregating, and the memory of it.
//
//   node spikes/tables/duckdb.mjs <node_modules> <extensions> <csv> [--browser chrome|firefox] [--affinity FFFF] [--steps a,b] [--out file]

import { execFileSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium, firefox } from "@playwright/test";
import { commit, machine } from "../machine.mjs";
import { pin } from "../pin.mjs";
import { serve } from "./serve.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const positional = [];
const options = { browser: "chrome", affinity: "", steps: "", out: "", profile: "" };
for (let i = 0; i < args.length; i++) {
  if (args[i].startsWith("--")) options[args[i].slice(2)] = args[++i];
  else positional.push(args[i]);
}
const [modules, extensions, csv] = positional.map((p) => resolve(p));
if (options.affinity) pin(options.affinity);
const server = await serve({ "/": here, "/nm/": modules, "/ext/": (path) => join(extensions, path.split("/").pop().replace(".duckdb_extension.wasm", ".eh.wasm")) });
const profile = resolve(options.profile || join(here, "../../target/spikes/tables/profile-" + Date.now().toString(36)));
mkdirSync(profile, { recursive: true });
const type = options.browser === "firefox" ? firefox : chromium;
const context = await type.launchPersistentContext(profile, { headless: true, ...(options.browser === "chrome" ? { channel: "chrome", args: ["--enable-blink-features=ForceEagerMeasureMemory"] } : {}), locale: "en-US", viewport: { width: 1280, height: 800 } });
const page = context.pages()[0] ?? (await context.newPage());
page.on("pageerror", (e) => console.log("PAGE ERROR", String(e)));
page.on("console", (m) => console.log("CONSOLE", m.text().slice(0, 300)));
page.setDefaultTimeout(0);
await page.goto(server.address + "/duckdb.html");
await page.waitForFunction(() => window.s8);
const result = { spike: "s8-duckdb", date: new Date().toISOString(), ...commit(), machine: machine(), browser: { name: options.browser, version: context.browser()?.version() ?? "" }, csv, steps: {} };
const call = (name, ...rest) => page.evaluate(([n, a]) => window.s8[n](...a), [name, rest]);
/** The largest private memory of the browser's processes, as the system counts it: the renderer's, with its workers. */
function processMemory() {
  try {
    const text = execFileSync("powershell.exe", ["-NoProfile", "-Command", `Get-CimInstance Win32_Process -Filter "Name='chrome.exe' OR Name='firefox.exe'" | Where-Object { $_.CommandLine -like '*--type=renderer*' } | ForEach-Object { $_.PrivatePageCount }`], { encoding: "utf8" });
    return Math.max(...text.trim().split(/\s+/).map(Number));
  } catch {
    return null;
  }
}
const stats = (v) => { const s = [...v].sort((a, b) => a - b); const at = (f) => s[Math.min(s.length - 1, Math.floor(f * s.length))]; return { n: s.length, p50: at(0.5), p95: at(0.95), max: s.at(-1) }; };
const step = async (name, body) => {
  const t = Date.now();
  let value;
  try { value = await body(); } catch (e) { value = { error: String(e).slice(0, 500) }; }
  let memory = null;
  try { memory = (await call("memory")).bytes; } catch {}
  const renderer = processMemory();
  result.steps[name] = { value, memory, renderer, wall: Date.now() - t };
  console.log(name.padEnd(34), JSON.stringify(value).slice(0, 200), `mem ${((memory ?? 0) / 2 ** 20).toFixed(0)} / ${((renderer ?? 0) / 2 ** 20).toFixed(0)} MiB`);
  return value;
};
const wanted = options.steps ? new Set(options.steps.split(",")) : null;
const on = (name) => !wanted || wanted.has(name);

await step("boot", () => call("boot", {}));
await page.locator("#file").setInputFiles(csv);
await step("register csv", () => call("registerFile", "in.csv"));
const P = "'opfs://t.parquet'";
if (on("import")) {
  await call("drop", "t.parquet");
  await call("registerOpfs", "t.parquet");
  await step("csv to parquet (snappy)", () => call("sql", `COPY (SELECT * FROM read_csv('in.csv')) TO ${P} (FORMAT parquet, COMPRESSION snappy)`));
  await step("parquet size", () => call("sizeOf", "t.parquet"));
} else {
  await call("registerOpfs", "t.parquet");
}
await step("columns", async () => (await call("sql", `SELECT string_agg(column_name||':'||column_type, ', ') c FROM (DESCRIBE SELECT * FROM ${P})`)).first);
const rows = (await call("sql", `SELECT count(*)::INT n FROM ${P}`)).first.n;
result.rows = rows;
let seed = 7;
const random = (limit) => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed % limit; };
const starts = (n) => Array.from({ length: n }, () => random(rows - 64));
if (on("view")) {
  await step("screen, parquet, OFFSET", async () => stats(await call("many", starts(10).map((s) => `SELECT * FROM ${P} LIMIT 64 OFFSET ${s}`))));
  await step("screen, parquet, file_row_number", async () => stats(await call("many", starts(20).map((s) => `SELECT * FROM read_parquet(${P}, file_row_number=true) WHERE file_row_number BETWEEN ${s} AND ${s + 63}`))));
}
if (on("agg")) {
  await step("count sum avg min max of 5 columns", () => call("sql", `SELECT ${["store", "quantity", "unit_price", "discount", "amount"].map((c) => `count(${c}), sum(${c}), avg(${c}), min(${c}), max(${c})`).join(", ")} FROM ${P}`));
}
if (on("filter")) {
  await step("filter region+amount, count", () => call("sql", `SELECT count(*) FROM ${P} WHERE region='East' AND amount>1000`));
  await step("filter note contains, count", () => call("sql", `SELECT count(*) FROM ${P} WHERE contains(note,'gift')`));
  await step("filter customer contains, count", () => call("sql", `SELECT count(*) FROM ${P} WHERE contains(customer,'777')`));
  await step("filter, row numbers fetched", () => call("column", `SELECT file_row_number::INT FROM read_parquet(${P}, file_row_number=true) WHERE region='East' AND amount>1000`));
  await step("filter, distinct values of product", () => call("sql", `SELECT product, count(*) FROM ${P} GROUP BY product ORDER BY 2 DESC`));
}
if (on("sort")) {
  await step("top 64 by amount", () => call("sql", `SELECT * FROM ${P} ORDER BY amount DESC LIMIT 64`));
  await step("order by amount, row numbers", () => call("column", `SELECT file_row_number::INT FROM read_parquet(${P}, file_row_number=true) ORDER BY amount DESC`));
  await step("order by customer, row numbers", () => call("column", `SELECT file_row_number::INT FROM read_parquet(${P}, file_row_number=true) ORDER BY customer`));
  await step("order by region, amount, row numbers", () => call("column", `SELECT file_row_number::INT FROM read_parquet(${P}, file_row_number=true) ORDER BY region, amount DESC`));
  await step("64 scattered rows, parquet", async () => stats(await call("many", Array.from({ length: 5 }, () => `SELECT * FROM read_parquet(${P}, file_row_number=true) WHERE file_row_number IN (${Array.from({ length: 64 }, () => random(rows)).join(",")})`))));
}
if (on("sorted")) {
  await call("drop", "s.parquet");
  await call("registerOpfs", "s.parquet");
  await step("sorted copy to parquet", () => call("sql", `COPY (SELECT * FROM ${P} ORDER BY amount DESC) TO 'opfs://s.parquet' (FORMAT parquet, COMPRESSION snappy)`));
}
if (on("sortedtable")) {
  // The whole table again, in the order asked for: what a screen costs then, and what having it costs.
  await step("sorted copy as an in-memory table", () => call("sql", `CREATE TABLE st AS SELECT * FROM ${P} ORDER BY amount DESC`));
  await step("screen of it, rowid range", async () => stats(await call("many", starts(20).map((s) => `SELECT * FROM st WHERE rowid BETWEEN ${s} AND ${s + 63}`))));
  await step("another order: drop, sort by region, amount", async () => { await call("sql", "DROP TABLE st"); return call("sql", `CREATE TABLE st AS SELECT * FROM ${P} ORDER BY region, amount DESC`); });
  await step("screen of that", async () => stats(await call("many", starts(20).map((s) => `SELECT * FROM st WHERE rowid BETWEEN ${s} AND ${s + 63}`))));
  await step("filtered rows as a table", async () => { await call("sql", "DROP TABLE st"); return call("sql", `CREATE TABLE st AS SELECT * FROM ${P} WHERE region='East' AND amount>1000`); });
}
if (on("order")) {
  // Only the row numbers, kept in order, and the rows joined to them when they are looked at.
  await step("order as a table of row numbers", () => call("sql", `CREATE TABLE o AS SELECT file_row_number r FROM read_parquet(${P}, file_row_number=true) ORDER BY amount DESC`));
  await step("screen joined through it", async () => stats(await call("many", starts(5).map((s) => `SELECT p.* FROM read_parquet(${P}, file_row_number=true) p JOIN o ON p.file_row_number=o.r WHERE o.rowid BETWEEN ${s} AND ${s + 63}`))));
}
if (on("table")) {
  await step("load into an in-memory table", () => call("sql", `CREATE TABLE t AS SELECT * FROM ${P}`));
  await step("screen, table, rowid range", async () => stats(await call("many", starts(20).map((s) => `SELECT * FROM t WHERE rowid BETWEEN ${s} AND ${s + 63}`))));
  await step("64 scattered rows, table", async () => stats(await call("many", Array.from({ length: 10 }, () => `SELECT * FROM t WHERE rowid IN (${Array.from({ length: 64 }, () => random(rows)).join(",")})`))));
  await step("order by amount, rowids, table", () => call("column", "SELECT rowid::INT FROM t ORDER BY amount DESC"));
  await step("agg, table", () => call("sql", `SELECT ${["store", "quantity", "unit_price", "discount", "amount"].map((c) => `count(${c}), sum(${c}), avg(${c}), min(${c}), max(${c})`).join(", ")} FROM t`));
}
result.requests = server.log.filter((r) => r.status !== 404).map((r) => ({ path: r.path, bytes: r.bytes }));
result.missing = server.log.filter((r) => r.status === 404).map((r) => r.path);
await context.close();
server.close();
try { rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch {}
if (options.out) writeFileSync(options.out, JSON.stringify(result, null, 1) + "\n");
process.exit(0);
