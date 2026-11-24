// What is archived of the grid by itself: viewport pages between the engine's
// worker and the main thread (S1) and drawing a thousand cells on a 2D canvas
// (S2) in Chrome, and jumping in Firefox.
//
//   node bench/run-grid-bench.mjs [options]
//
// It runs spikes/grid/measure.mjs and makes one report of what that writes, as
// bench/results/<date>-<commit>-render.json. Chrome runs without a window, as
// for every report of this kind so far; Firefox has no GPU then and opens one.
//
// `cargo xtask bench --suite grid` builds the page first and then runs this.
//
// Options:
//   --affinity <mask>  pin the measurements to a set of logical processors
//   --skip-firefox     leave Firefox out, and its window
//   --quick            a short run, to see that everything works
//   --results <dir>    where to archive (default bench/results)
//   --no-archive       only write the report to target/bench-render/

import { join, resolve } from "node:path";
import { parseArgs } from "node:util";
import { refusals, renderReport } from "../apps/web/src/bench/report.ts";
import { root } from "../spikes/grid/serve.mjs";
import { archive, builtEarlier, PUBLISHED, reportOf, writeReport } from "./archive.mjs";

const { values: options } = parseArgs({
  options: {
    affinity: { type: "string", default: "" },
    "skip-firefox": { type: "boolean", default: false },
    quick: { type: "boolean", default: false },
    results: { type: "string", default: PUBLISHED },
    "no-archive": { type: "boolean", default: false },
  },
});
console.log(
  options.quick ? "about a minute" : `about four minutes${options["skip-firefox"] ? "" : ", the last of them with a window on the screen that must stay in view"}`,
);

function measure(name, ...args) {
  const out = join(root, `target/bench-render/${name}.json`);
  const shared = [...(options.affinity ? ["--affinity", options.affinity] : []), ...(options.quick ? ["--quick"] : [])];
  return reportOf("spikes/grid/measure.mjs", [...args, ...shared], out, "inherit");
}

const s1 = measure("s1", "s1");
const s2 = measure("s2", "s2");
const firefox = options["skip-firefox"] ? null : measure("firefox-jump", "s1", "--browser", "firefox", "--headed", "--only", "jump");

const report = renderReport(s1, s2, firefox);
const own = join(root, "target/bench-render/grid.json");
writeReport(own, report);
console.log(`\nreport written to ${own}`);

const results = resolve(options.results);
const reasons = [s1, s2, firefox]
  .filter((part) => part !== null)
  .flatMap((part) =>
    refusals({
      uncommittedChanges: part.uncommittedChanges,
      softwareAdapter: null,
      idleIntervalMs: part.environment.frameInterval.p50,
      displays: part.machine.system?.displays ?? [],
      // What is archived of Chrome is the main thread's share, which needs no display.
      headless: false,
      quick: options.quick,
      published: results === PUBLISHED,
      problems: part.problems,
    }),
  );
reasons.push(...builtEarlier([join(root, "apps/web/dist-spikes/bench.html")]));
if (!options["no-archive"]) {
  archive(results, report, "render", [...new Set(reasons)], () => report);
}
process.exitCode = [s1, s2, firefox].some((part) => part && part.problems.length > 0) ? 1 : 0;
