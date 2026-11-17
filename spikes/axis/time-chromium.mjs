// Times the workloads of spike_axis::cost, compiled to WebAssembly, inside
// Chromium.
//
//   node spikes/axis/time-chromium.mjs path/to/spike_axis.wasm [chrome] [slowdown]
//
// With "chrome" as the second argument it uses the installed Google Chrome
// instead of the Chromium build that Playwright downloads ("" keeps the
// default). A slowdown such as 4 makes the browser throttle its CPU by that
// factor, as a rough stand-in for a slower machine.
import { readFileSync } from "node:fs";
import { chromium } from "@playwright/test";

const wasm = readFileSync(process.argv[2]).toString("base64");
const browser = await chromium.launch(process.argv[3] ? { channel: process.argv[3] } : {});
const page = await browser.newPage();
const slowdown = Number(process.argv[4] ?? 1);
if (slowdown > 1) {
  const session = await page.context().newCDPSession(page);
  await session.send("Emulation.setCPUThrottlingRate", { rate: slowdown });
}

const lines = await page.evaluate(async (encoded) => {
  const bytes = Uint8Array.from(atob(encoded), (character) => character.charCodeAt(0));
  const { instance } = await WebAssembly.instantiate(bytes, {});
  const sheet = instance.exports;

  const COUNT = 1_000_000;
  // Milliseconds for one call of `run`, best of five. Short workloads are
  // repeated so that each sample is long compared with the timer's resolution.
  const best = (run, repeat = 1) => {
    let fastest = Number.POSITIVE_INFINITY;
    for (let sample = 0; sample < 5; sample++) {
      const start = performance.now();
      for (let index = 0; index < repeat; index++) run(index);
      fastest = Math.min(fastest, (performance.now() - start) / repeat);
    }
    return fastest;
  };
  const ns = (milliseconds, count) => `${((milliseconds * 1e6) / count).toFixed(2).padStart(8)} ns`;
  const ms = (milliseconds) => `${milliseconds.toFixed(2).padStart(8)} ms`;

  // The timer's resolution: the smallest step it was seen to take.
  let resolution = Number.POSITIVE_INFINITY;
  for (let previous = performance.now(), seen = 0; seen < 50; ) {
    const now = performance.now();
    if (now > previous) {
      resolution = Math.min(resolution, now - previous);
      previous = now;
      seen++;
    }
  }

  const out = [`timer resolution ${resolution.toFixed(3)} ms`, "", "baseline, addressed by position"];
  sheet.axis_setup(0);
  out.push(`  read at a random position        ${ns(best(() => sheet.axis_read_random_positional(COUNT)), COUNT)}`);
  out.push(`  sum of one column, 1M rows       ${ms(best(() => sheet.axis_sum_column_positional(), 10))}`);
  sheet.axis_fill_scaled(100_000);
  out.push(`  100k formulas =A*E1, recalculate ${ms(best((index) => sheet.axis_recalculate_positional(index + 2), 20))}`);
  sheet.axis_fill_product();
  out.push(`  1M formulas =B*C+E1, recalculate ${ms(best((index) => sheet.axis_recalculate_positional(index + 2), 3))}`);
  out.push(`  insert one row (data moves)      ${ms(best(() => sheet.axis_insert_rows_positional(1), 5))}`);

  const shapes = ["as created", "after 1,000 inserts", "fully shuffled"];
  for (let shape = 0; shape < shapes.length; shape++) {
    const runs = sheet.axis_setup(shape);
    const sharing = sheet.axis_rows_sharing_a_template();
    out.push("", `by identity, ${shapes[shape]}: ${runs} runs, axis ${(sheet.axis_bytes() / 1024).toFixed(1)} KB, ${sharing} rows can share a template`);
    out.push(`  read at a random position        ${ns(best(() => sheet.axis_read_random(COUNT)), COUNT)}`);
    out.push(`  read at consecutive positions    ${ns(best(() => sheet.axis_read_consecutive(COUNT)), COUNT)}`);
    out.push(`  read through an identity         ${ns(best(() => sheet.axis_read_by_identity()), COUNT)}`);
    out.push(`  write at a random position       ${ns(best(() => sheet.axis_write_random(COUNT)), COUNT)}`);
    out.push(`  which of two rows comes first    ${ns(best(() => sheet.axis_compare_positions()), COUNT / 2)}`);
    out.push(`  sum of one column, 1M rows       ${ms(best(() => sheet.axis_sum_column(), 10))}`);
    sheet.axis_fill_scaled(100_000);
    out.push(`  100k formulas =A*E1, recalculate ${ms(best((index) => sheet.axis_recalculate(index + 2), 20))}`);
    sheet.axis_fill_product();
    out.push(`  1M formulas =B*C+E1, recalculate ${ms(best((index) => sheet.axis_recalculate(index + 2), 3))}`);
    out.push(`  insert one row (axis rebuilt)    ${ms(best(() => sheet.axis_insert_rows(1), shape === 2 ? 2 : 20))}`);
  }
  return out;
}, wasm);

console.log(`${browser.browserType().name()} ${browser.version()}${slowdown > 1 ? `, CPU slowed ${slowdown}x` : ""}`);
console.log(lines.join("\n"));
await browser.close();
