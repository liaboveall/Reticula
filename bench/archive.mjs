// Writing a report, and archiving it if nothing is wrong with it.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { staleBuilds } from "../apps/web/src/bench/report.ts";
import { root } from "../spikes/grid/serve.mjs";
import { commitTime } from "../spikes/machine.mjs";

/** The results that the pages quote. */
export const PUBLISHED = join(root, "bench/results");

/** Of a number in a report, the digits that mean something. */
const rounded = (_, value) => (typeof value === "number" && !Number.isInteger(value) ? Number(value.toPrecision(6)) : value);

export function writeReport(path, report) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${JSON.stringify(report, rounded, 1)}\n`);
}

/** What is wrong with the files of builds, by which one tells when they were made. */
export function builtEarlier(files) {
  const builds = files.map((file) => {
    const made = statSync(file, { throwIfNoEntry: false });
    return { name: relative(root, dirname(file)).replaceAll("\\", "/"), madeMs: made ? made.mtimeMs : null };
  });
  return staleBuilds(builds, commitTime());
}

/** Runs a script that writes a report, and returns the report: what it said and not what an earlier one did. */
export function reportOf(script, args, out, stdio) {
  rmSync(out, { force: true });
  // An exit code other than 0 is about problems, which the report has as well.
  const { error, status } = spawnSync(process.execPath, [join(root, script), ...args, "--out", out], { stdio });
  if (error || !existsSync(out)) {
    throw error ?? new Error(`${script} wrote no report (exit code ${status})`);
  }
  return JSON.parse(readFileSync(out, "utf8"));
}

/**
 * Archives what `merge` makes of the report that is there under the name, if
 * any, unless there are reasons not to. Says which of it happened.
 */
export function archive(directory, { date, commit }, kind, reasons, merge) {
  if (reasons.length > 0) {
    console.log(`not archived:\n  ${reasons.join("\n  ")}`);
    return false;
  }
  const path = join(directory, `${date.slice(0, 10)}-${commit}-${kind}.json`);
  writeReport(path, merge(existsSync(path) ? JSON.parse(readFileSync(path, "utf8")) : null));
  console.log(`archived in ${path}`);
  return true;
}
