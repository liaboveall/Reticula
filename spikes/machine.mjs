// What a report says about the machine it was measured on and the commit.

import { execFileSync } from "node:child_process";
import os from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** The repository root. */
const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

/** Facts about the machine that Node cannot see, asked of the system. Windows only. */
export function system() {
  if (process.platform !== "win32") {
    return null;
  }
  const script = [
    `$affinity = '{0:X}' -f [int64](Get-Process -Id ${process.pid}).ProcessorAffinity`,
    "$battery = Get-CimInstance Win32_Battery | Select-Object -First 1",
    "$displays = Get-CimInstance Win32_VideoController | Where-Object CurrentRefreshRate | ForEach-Object { '{0} {1}x{2} {3} Hz' -f $_.Name, $_.CurrentHorizontalResolution, $_.CurrentVerticalResolution, $_.CurrentRefreshRate }",
    "[pscustomobject]@{ affinity = $affinity; onMains = if ($battery) { $battery.BatteryStatus -eq 2 } else { $true }; powerScheme = ((powercfg /getactivescheme) -join ' ') -replace '.*?([0-9a-fA-F]{8}(-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}).*', '$1'; displays = @($displays) } | ConvertTo-Json -Compress",
  ].join("; ");
  try {
    return JSON.parse(execFileSync("powershell.exe", ["-NoProfile", "-Command", script], { encoding: "utf8" }));
  } catch {
    return null;
  }
}

export function machine() {
  return {
    cpu: os.cpus()[0]?.model.trim(),
    logicalProcessors: os.cpus().length,
    memoryGB: Math.round(os.totalmem() / 2 ** 30),
    os: `${os.type()} ${os.release()}`,
    system: system(),
  };
}

/** What git says, run in the repository or in another directory. */
export function git(args, cwd = root) {
  try {
    return execFileSync("git", args, { cwd, encoding: "utf8" }).trim();
  } catch {
    return "";
  }
}

/**
 * What is not the commit's. Results do not count: nothing is built from them,
 * and the first one that is archived of a commit must not stand in the way of
 * the others.
 */
const STATUS = ["status", "--porcelain", "--", ".", ":(exclude)bench/results"];

/** The commit, and whether the files are not the commit's. */
export function commit(cwd = root) {
  const name = git(["rev-parse", "--short", "HEAD"], cwd);
  return { commit: name, uncommittedChanges: name === "" || git(STATUS, cwd) !== "" };
}

/** When the commit was made, in milliseconds. */
export function commitTime(cwd = root) {
  return Number(git(["log", "-1", "--format=%ct"], cwd)) * 1000;
}
