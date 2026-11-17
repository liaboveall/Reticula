// Pinning a measurement to a set of logical processors.

import { execFileSync } from "node:child_process";

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
