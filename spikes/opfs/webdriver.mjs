// Drives the experiment page through a WebDriver server, with plain HTTP
// requests. This is how the page is run in Safari: `safaridriver` ships with
// macOS and speaks this protocol, and Safari is not a browser Playwright
// starts.
//
// A WebDriver session carries out one command at a time, in the window it is
// switched to. Everything the experiments need from a tab is therefore a call
// that returns, never one that stays open while another tab is driven.

import { spawn } from "node:child_process";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const CALL = `
  const [command, args, done] = arguments;
  window.__opfs.call(command, args).then(done, (error) => done({ ok: false, error: { name: "Error", message: String(error) } }));
`;

/** Starts safaridriver on a port and waits until it answers. */
export async function startSafariDriver(port) {
  const driver = spawn("safaridriver", ["--port", String(port)], { stdio: "inherit" });
  const server = `http://localhost:${port}`;
  for (let attempt = 0; attempt < 100; attempt++) {
    try {
      if ((await fetch(`${server}/status`)).ok) {
        return { server, stop: () => driver.kill() };
      }
    } catch {
      // Not listening yet.
    }
    await sleep(100);
  }
  driver.kill();
  throw new Error("safaridriver did not start");
}

export async function connect({ server, browser, address, capabilities = {} }) {
  async function request(method, path, body) {
    const response = await fetch(`${server}${path}`, {
      method,
      headers: { "content-type": "application/json; charset=utf-8" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const { value } = await response.json();
    if (!response.ok) {
      throw new Error(`${method} ${path}: ${value?.error}: ${value?.message}`);
    }
    return value;
  }

  const created = await request("POST", "/session", { capabilities: { alwaysMatch: { browserName: browser, ...capabilities } } });
  const session = `/session/${created.sessionId}`;
  const version = created.capabilities?.browserVersion ?? "";
  await request("POST", `${session}/timeouts`, { script: 180000, pageLoad: 60000 });
  // The session's first window stays open, so that closing a tab never ends the session.
  const first = await request("GET", `${session}/window`);
  let current = first;

  async function switchTo(handle) {
    if (current !== handle) {
      await request("POST", `${session}/window`, { handle });
      current = handle;
    }
  }

  // One command at a time, whichever tab it is for.
  let queue = Promise.resolve();
  const inTurn = (action) => {
    const result = queue.then(action);
    queue = result.catch(() => {});
    return result;
  };

  async function open() {
    const handle = await inTurn(async () => {
      const created = await request("POST", `${session}/window/new`, { type: "tab" });
      await switchTo(created.handle);
      await request("POST", `${session}/url`, { url: `${address}/index.html` });
      for (let attempt = 0; ; attempt++) {
        if (await request("POST", `${session}/execute/sync`, { script: "return window.__opfs !== undefined;", args: [] })) {
          return created.handle;
        }
        if (attempt === 100) {
          throw new Error("the page did not start");
        }
        await sleep(50);
      }
    });
    return {
      call: (command, args = {}) =>
        inTurn(async () => {
          await switchTo(handle);
          const answer = await request("POST", `${session}/execute/async`, { script: CALL, args: [command, args] });
          if (!answer.ok) {
            throw Object.assign(new Error(answer.error.message), { name: answer.error.name });
          }
          return answer.value;
        }),
      close: () =>
        inTurn(async () => {
          await switchTo(handle);
          await request("DELETE", `${session}/window`);
          await switchTo(first);
        }),
    };
  }

  return {
    name: browser,
    description: () => `${browser} ${version}, through WebDriver`,
    setVersion() {},
    // A WebDriver session cannot end a tab's process, start the browser again
    // on the same storage, or change a quota.
    can: { killBrowser: false, killTab: false, limitQuota: false, grantPersistence: false },
    open,
    close: () => request("DELETE", session).catch(() => {}),
  };
}
