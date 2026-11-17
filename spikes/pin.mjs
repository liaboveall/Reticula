// Pinning a measurement to a set of logical processors.
//
// A script that takes measurements calls `pin` itself. One that has no option
// for it can be run through this file instead:
//
//   node spikes/pin.mjs <mask> <script> [arguments of the script]

import { execFileSync } from "node:child_process";
import { resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

/**
 * Restricts this process, and every process it starts from now on, to the
 * logical processors in `mask`: a hexadecimal number with one bit for each.
 * On a CPU with performance and efficiency cores, timings can only be
 * compared when they all come from one kind.
 *
 * A script cannot do this to its own process, so a tool of the system is
 * asked to. The process stays the same one, and so does its exit code.
 */
export function pin(mask) {
  if (!/^[0-9a-f]+$/i.test(mask)) {
    throw new Error(`not a hexadecimal mask: ${mask}`);
  }
  if (process.platform === "win32") {
    execFileSync("powershell.exe", ["-NoProfile", "-Command", `(Get-Process -Id ${process.pid}).ProcessorAffinity = 0x${mask}`], { stdio: ["ignore", "ignore", "inherit"] });
  } else {
    // Every thread of the process, not just the one that asks.
    execFileSync("taskset", ["--all-tasks", "--pid", mask, String(process.pid)], { stdio: ["ignore", "ignore", "inherit"] });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [mask, script, ...rest] = process.argv.slice(2);
  if (!mask || !script) {
    console.error("usage: node spikes/pin.mjs <mask> <script> [arguments of the script]");
    process.exit(2);
  }
  pin(mask);
  // The script sees the arguments it would have seen had it been run directly.
  process.argv.splice(1, process.argv.length - 1, resolve(script), ...rest);
  await import(pathToFileURL(resolve(script)).href);
}
