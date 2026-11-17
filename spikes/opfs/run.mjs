// Runs the experiments on local storage and tabs in a real browser.
//
//   node spikes/opfs/run.mjs [options]
//
// It serves the experiment page (spikes/opfs/page), opens it in as many tabs
// as an experiment needs, prints what happened and writes everything it
// measured, with a description of the machine and the browser, to a data file.
// The process ends with a failure if any experiment failed. `cargo xtask spike
// s4` runs this.
//
// Options:
//   --browser <name>    chrome (the installed Google Chrome, the default),
//                       chromium, firefox or webkit (the builds Playwright
//                       downloads), or safari (through safaridriver, macOS)
//   --webdriver <url>   drive the browser through the WebDriver server at this
//                       address instead; --browser then names the browser to
//                       ask it for
//   --capabilities <json>  with --webdriver: more capabilities to ask for
//   --headed            show the browser window
//   --affinity <mask>   pin this process, and so the browser, to a set of
//                       logical processors: a hexadecimal mask. On a CPU with
//                       performance and efficiency cores, name the former
//   --only <id>         just one experiment: environment, append, torn,
//                       exclusive, latency, document, unresponsive, release,
//                       replicas, crash, storage or quota
//   --quick             fewer rounds, to see that everything works
//   --serve             only serve the page, on port 5276, to open by hand
//   --out <file>        the data file (default: target/spikes/opfs/<browser>.json)

import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import os from "node:os";
import { dirname, join, resolve } from "node:path";
import { run } from "./scenarios.mjs";
import { root, serve } from "./serve.mjs";

const options = { browser: "chrome", webdriver: "", capabilities: "", headed: false, affinity: "", only: "", quick: false, serve: false, out: "" };
const given = process.argv.slice(2);
for (let index = 0; index < given.length; index++) {
  const name = given[index].replace(/^--/, "");
  if (!(name in options) || !given[index].startsWith("--")) {
    fail(`unknown option ${given[index]}`);
  }
  if (typeof options[name] === "boolean") {
    options[name] = true;
  } else {
    const value = given[++index];
    if (value === undefined) {
      fail(`--${name} needs a value`);
    }
    options[name] = value;
  }
}
if (!options.webdriver && !["chrome", "chromium", "firefox", "webkit", "safari"].includes(options.browser)) {
  fail(`unknown browser ${options.browser}`);
}

function fail(message) {
  console.error(message);
  process.exit(2);
}

// A process cannot change which processors it may run on from the inside, so
// the script starts itself again under a command that can. The browser's
// processes inherit the setting.
if (options.affinity && !process.env.RETICULA_PINNED) {
  const environment = { ...process.env, RETICULA_PINNED: options.affinity };
  const command = [process.execPath, ...process.argv.slice(1)];
  const child =
    process.platform === "win32"
      ? spawnSync("cmd.exe", ["/d", "/s", "/c", `"start "" /B /WAIT /AFFINITY ${options.affinity} ${command.map((part) => `"${part}"`).join(" ")}"`], {
          stdio: "inherit",
          env: environment,
          windowsVerbatimArguments: true,
        })
      : spawnSync("taskset", [options.affinity, ...command], { stdio: "inherit", env: environment });
  process.exit(child.status ?? 1);
}

function git(...args) {
  try {
    return execFileSync("git", args, { cwd: root, encoding: "utf8" }).trim();
  } catch {
    return "";
  }
}

if (options.serve) {
  const server = await serve(5276);
  console.log(`The page is at ${server.address}/\nOpen it in the browser to be checked, in one tab or two. Ctrl+C stops this.`);
  await new Promise(() => {});
}

const server = await serve();
const directory = join(root, "target/spikes/opfs");
mkdirSync(directory, { recursive: true });

// Safari is driven through its WebDriver server; so is any browser when a
// server's address is given. Everything else is started by Playwright, which
// is loaded only then: a machine that runs Safari alone needs no packages.
let backend;
let stopDriver = () => {};
if (options.webdriver || options.browser === "safari") {
  const { connect, startSafariDriver } = await import("./webdriver.mjs");
  let address = options.webdriver;
  if (!address) {
    const driver = await startSafariDriver(4723);
    address = driver.server;
    stopDriver = driver.stop;
  }
  backend = await connect({
    server: address,
    browser: options.browser,
    address: server.address,
    capabilities: options.capabilities ? JSON.parse(options.capabilities) : {},
  });
} else {
  const { launch } = await import("./playwright.mjs");
  backend = await launch({ browser: options.browser, headed: options.headed, address: server.address, directory });
}

let results = [];
let stopped = "";
try {
  results = await run(backend, { quick: options.quick, only: options.only });
} catch (error) {
  stopped = String(error?.stack ?? error);
  console.error(stopped);
}

const count = (status) => results.filter((result) => result.status === status).length;
const failed = results.filter((result) => result.status === "failed");
console.log(`\n${backend.description()}: ${count("passed")} passed, ${failed.length} failed, ${count("skipped")} skipped`);
for (const result of failed) {
  console.log(`  ${result.title}:\n    ${result.failures.join("\n    ")}`);
}

const name = `${options.browser}${options.webdriver ? "-webdriver" : ""}${options.only ? `-${options.only}` : ""}${options.quick ? "-quick" : ""}`;
const out = options.out ? resolve(options.out) : join(directory, `${name}.json`);
mkdirSync(dirname(out), { recursive: true });
writeFileSync(
  out,
  `${JSON.stringify(
    {
      spike: "s4",
      date: new Date().toISOString(),
      commit: git("rev-parse", "--short", "HEAD"),
      uncommittedChanges: git("status", "--porcelain") !== "",
      machine: {
        cpu: os.cpus()[0]?.model.trim(),
        logicalProcessors: os.cpus().length,
        memoryGB: Math.round(os.totalmem() / 2 ** 30),
        os: `${os.type()} ${os.release()} ${os.arch()}`,
      },
      browser: backend.description(),
      options,
      results,
      stopped,
    },
    null,
    1,
  )}\n`,
);
console.log(`data written to ${out}`);

await backend.close();
stopDriver();
server.close();
process.exit(failed.length > 0 || stopped ? 1 : 0);
