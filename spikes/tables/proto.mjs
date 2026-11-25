// Measures the column store prototype, on the table duckdb.mjs is measured on.
//
//   node spikes/tables/proto.mjs <wasm> <csv> [--browser chrome|firefox] [--affinity FFFF] [--out file]

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
const options = { browser: "chrome", affinity: "", out: "", slice: "4194304" };
for (let i = 0; i < args.length; i++) {
  if (args[i].startsWith("--")) options[args[i].slice(2)] = args[++i];
  else positional.push(resolve(args[i]));
}
const [wasm, csv] = positional;
if (options.affinity) pin(options.affinity);
const server = await serve({ "/": here, "/proto.wasm": () => wasm });
const profile = join(here, "../../target/spikes/tables/profile-" + Date.now().toString(36));
mkdirSync(profile, { recursive: true });
const type = options.browser === "firefox" ? firefox : chromium;
const context = await type.launchPersistentContext(profile, { headless: true, ...(options.browser === "chrome" ? { channel: "chrome" } : {}), locale: "en-US" });
const page = context.pages()[0] ?? (await context.newPage());
page.on("console", (m) => console.log("CONSOLE", m.text().slice(0, 300)));
page.setDefaultTimeout(0);
await page.goto(server.address + "/proto.html");
await page.waitForFunction(() => window.run);
await page.locator("#file").setInputFiles(csv);
const run = (name, argument = {}) => page.evaluate(([n, a]) => window.run(n, a), [name, argument]);
const stats = ({ times, reads }) => { const s = [...times].sort((a, b) => a - b); const at = (f) => s[Math.min(s.length - 1, Math.floor(f * s.length))]; return { n: s.length, p50: at(0.5), p95: at(0.95), max: s.at(-1), reads }; };
const processes = () => { try { return Math.max(...execFileSync("powershell.exe", ["-NoProfile", "-Command", `Get-CimInstance Win32_Process -Filter "Name='chrome.exe' OR Name='firefox.exe'" | Where-Object { $_.CommandLine -like '*--type=renderer*' -or $_.CommandLine -like '*tab*' } | ForEach-Object { $_.PrivatePageCount }`], { encoding: "utf8" }).trim().split(/\s+/).map(Number)); } catch { return null; } };
const result = { spike: "s8-prototype", date: new Date().toISOString(), ...commit(), machine: machine(), browser: { name: options.browser, version: context.browser()?.version() ?? "" }, csv, steps: {} };
const step = async (name, body) => {
  let value;
  try { value = await body(); } catch (e) { value = { error: String(e).slice(0, 300) }; }
  const memory = await run("memory").catch(() => null);
  const renderer = processes();
  result.steps[name] = { value, memory, renderer };
  console.log(name.padEnd(40), JSON.stringify(value).slice(0, 220), `mem ${((memory ?? 0) / 2 ** 20).toFixed(0)} / ${((renderer ?? 0) / 2 ** 20).toFixed(0)} MiB`);
  return value;
};
// id, ordered_at, day, store, region, product, customer, quantity, unit_price, discount, amount, paid, note
const KINDS = [0, 2, 3, 0, 5, 5, 5, 0, 1, 1, 1, 4, 5];
const [REGION, CUSTOMER, AMOUNT, NOTE] = [4, 6, 10, 12];
await step("boot", () => run("boot"));
const { rows } = await step("csv to columns", () => run("import", { kinds: KINDS, slice: Number(options.slice) }));
let seed = 7;
const random = (limit) => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed % limit; };
const starts = (n, limit = rows) => Array.from({ length: n }, () => random(limit - 64));
await step("dictionary of customer", () => run("dictionary", { column: CUSTOMER }));
await step("screen in order, cold", async () => stats(await run("screens", { starts: starts(20), count: 64, ordered: false })));
await step("aggregates of 5 columns", () => run("aggregate", { columns: [3, 7, 8, 9, 10] }));
await step("filter region+amount", () => run("filter", { column: REGION, value: "East", number: AMOUNT, limit: 100000 }));
await step("... its row numbers", () => run("project", { sorted: false }));
await run("clear");
await step("screen of the filtered, cold", async () => stats(await run("screens", { starts: starts(20, 600000), count: 64, ordered: true })));
await step("filter note contains", () => run("contains", { column: NOTE, value: "gift" }));
await step("filter customer contains", () => run("contains", { column: CUSTOMER, value: "777" }));
await step("sort by customer", () => run("sort", { column: CUSTOMER, descending: false, fresh: true }));
await step("sort by amount, then region", async () => { const a = await run("sort", { column: AMOUNT, descending: true, fresh: true }); const b = await run("sort", { column: REGION, descending: false, fresh: false }); return { ms: a.ms + b.ms }; });
await step("sort by amount", () => run("sort", { column: AMOUNT, descending: true, fresh: true }));
await run("clear");
await step("screen of the sorted, cold", async () => stats(await run("screens", { starts: starts(20), count: 64, ordered: true })));
await step("screen of the sorted, warm", async () => stats(await run("screens", { starts: starts(20), count: 64, ordered: true })));
await step("... filtered, in order", () => run("project", { sorted: true }));
await context.close();
server.close();
try { rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch {}
if (options.out) writeFileSync(options.out, JSON.stringify(result, null, 1) + "\n");
process.exit(0);
