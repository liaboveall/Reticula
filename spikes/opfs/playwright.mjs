// Drives the experiment page in a browser that Playwright starts: Chromium,
// the installed Chrome, Firefox or WebKit.
//
// The browser runs on a profile directory of its own, so that it can be
// killed and started again on what it left on disk.

import { execFileSync } from "node:child_process";
import { mkdirSync, rmSync } from "node:fs";
import { basename, join } from "node:path";
import { chromium, firefox, webkit } from "@playwright/test";

const types = { chrome: chromium, chromium, firefox, webkit };
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Every process on the machine: its id, its parent's and its command line. */
function processes() {
  if (process.platform === "win32") {
    const script = "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,CommandLine | ConvertTo-Json -Compress";
    const listed = execFileSync("powershell.exe", ["-NoProfile", "-Command", script], { encoding: "utf8", maxBuffer: 1 << 28 });
    return JSON.parse(listed).map((entry) => ({ pid: entry.ProcessId, parent: entry.ParentProcessId, command: entry.CommandLine ?? "" }));
  }
  return execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8", maxBuffer: 1 << 28 })
    .split("\n")
    .map((line) => line.trim().match(/^(\d+)\s+(\d+)\s+(.*)$/))
    .filter(Boolean)
    .map(([, pid, parent, command]) => ({ pid: Number(pid), parent: Number(parent), command }));
}

/** The browser started on `profile` and everything it started, parents first. */
function family(profile) {
  const all = processes();
  const mark = basename(profile);
  const found = all.filter((entry) => entry.command.includes(mark));
  const pids = new Set(found.map((entry) => entry.pid));
  for (let grew = true; grew; ) {
    grew = false;
    for (const entry of all) {
      if (pids.has(entry.parent) && !pids.has(entry.pid)) {
        pids.add(entry.pid);
        found.push(entry);
        grew = true;
      }
    }
  }
  return found.sort((left, right) => Number(pids.has(left.parent)) - Number(pids.has(right.parent)));
}

/**
 * Starts a browser. `quota`, in bytes, turns down how much the page may
 * store; only Chromium has a way to do that.
 */
export async function launch({ browser, headed, address, directory, quota }) {
  const type = types[browser];
  const chromiumBased = type === chromium;
  const profile = join(directory, `profile-${browser}-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`);
  mkdirSync(profile, { recursive: true });

  const options = { headless: !headed };
  if (browser === "chrome") {
    options.channel = "chrome";
  }
  if (type === firefox) {
    options.firefoxUserPrefs = {
      // Starting again after being killed must not end in the dialog that
      // offers to start without add-ons.
      "toolkit.startup.max_resumed_crashes": -1,
    };
  }

  let context = await type.launchPersistentContext(profile, options);
  let version = context.browser()?.version() ?? "";
  const tabs = new Set();

  async function open() {
    const page = await context.newPage();
    await page.goto(`${address}/index.html`);
    await page.waitForFunction(() => window.__opfs !== undefined);
    if (chromiumBased && quota) {
      const session = await context.newCDPSession(page);
      await session.send("Storage.overrideQuotaForOrigin", { origin: address, quotaSize: quota });
    }
    let crashed = false;
    page.on("crash", () => {
      crashed = true;
    });
    const tab = {
      async call(command, args = {}) {
        const answer = await page.evaluate(([command, args]) => window.__opfs.call(command, args), [command, args]);
        if (!answer.ok) {
          throw Object.assign(new Error(answer.error.message), { name: answer.error.name });
        }
        return answer.value;
      },
      async close() {
        tabs.delete(tab);
        await page.close().catch(() => {});
      },
      /**
       * Lets the page have persistent storage without asking anyone. The
       * permission lasts as long as this tab: it goes when the session that
       * granted it does.
       */
      async grantPersistence() {
        const session = await context.newCDPSession(page);
        await session.send("Browser.grantPermissions", { origin: address, permissions: ["durableStorage"] });
      },
      /** Ends the process the tab runs in, without warning. */
      async kill() {
        tabs.delete(tab);
        if (chromiumBased) {
          const session = await context.newCDPSession(page);
          session.send("Page.crash").catch(() => {});
        } else {
          page.goto("about:crashcontent").catch(() => {});
        }
        for (const end = Date.now() + 5000; !crashed && Date.now() < end; ) {
          await sleep(5);
        }
        if (!crashed) {
          throw new Error("the tab's process could not be ended");
        }
      },
    };
    tabs.add(tab);
    return tab;
  }

  return {
    name: browser,
    description: () => `${browser} ${version}${headed ? "" : ", headless"}`,
    setVersion(found) {
      version ||= found;
    },
    can: {
      killBrowser: true,
      // WebKit has no command for it.
      killTab: type !== webkit,
      // Firefox has a preference for the size of all the storage it manages
      // (dom.quotaManager.temporaryStorage.fixedLimit), but the build
      // Playwright downloads went on writing far past it.
      limitQuota: chromiumBased,
      grantPersistence: chromiumBased,
    },
    open,

    /**
     * Finds the browser's processes now, which takes a moment, and returns a
     * function that ends them all at once and starts the browser again on
     * the same profile.
     */
    prepareKill() {
      const victims = family(profile);
      return async () => {
        const closed = new Promise((resolve) => context.once("close", resolve));
        for (const victim of victims) {
          try {
            process.kill(victim.pid, "SIGKILL");
          } catch {
            // It ended with its parent.
          }
        }
        await Promise.race([closed, sleep(10000)]);
        tabs.clear();
        // The profile's own lock can outlive the processes for a moment.
        for (let attempt = 1; ; attempt++) {
          try {
            context = await type.launchPersistentContext(profile, options);
            break;
          } catch (error) {
            if (attempt === 5) {
              throw error;
            }
            await sleep(500);
          }
        }
        return victims.length;
      };
    },

    /** A second browser like this one, on a profile of its own, with the quota turned down. */
    limited: (bytes) => launch({ browser, headed, address, directory, quota: bytes }),

    async close() {
      await context.close().catch(() => {});
      try {
        rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
      } catch {
        // Files a browser process has not let go of yet; the directory is under target/.
      }
    },
  };
}
