// The experiment page. Everything that touches files or locks happens in its
// worker; this file passes commands to the worker, does the few things only a
// page can do, and puts the results on screen.
//
// A script drives the page through `window.__opfs.call(command, args)`, which
// resolves to `{ ok: true, value }` or `{ ok: false, error }`.

/** Names this tab in the messages tabs send each other. */
const TAB = Math.random().toString(36).slice(2, 10);
/** How long the worker gets to hand a document over before the page stops it. */
const GRACE_MS = 500;
const SAMPLE_DOCUMENT = "sample";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// --- the worker -------------------------------------------------------------

let worker;
let nextId = 1;
const pending = new Map();
/** The document as the worker last described it. */
let documentState = null;

function startWorker() {
  worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module", name: TAB });
  worker.onmessage = ({ data }) => {
    if (data.event === "document") {
      documentState = data.document;
      showDocument();
      return;
    }
    const waiting = pending.get(data.id);
    pending.delete(data.id);
    if (data.error) {
      waiting?.reject(Object.assign(new Error(data.error.message), { name: data.error.name }));
    } else {
      waiting?.resolve(data.value);
    }
  };
}

/** Stops the worker, which frees every lock and file it holds, and starts a new one. */
function replaceWorker(reason) {
  worker.terminate();
  for (const waiting of pending.values()) {
    waiting.reject(new Error(reason));
  }
  pending.clear();
  startWorker();
}

function ask(command, args = {}) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    worker.postMessage({ id, command, args });
  });
}

/** Runs one command in a worker of its own, next to the page's worker. */
function askAnotherWorker(command, args = {}) {
  return new Promise((resolve, reject) => {
    const other = new Worker(new URL("./worker.js", import.meta.url), { type: "module", name: `${TAB}-other` });
    other.onmessage = ({ data }) => {
      other.terminate();
      if (data.error) {
        reject(Object.assign(new Error(data.error.message), { name: data.error.name }));
      } else {
        resolve(data.value);
      }
    };
    other.postMessage({ id: 0, command, args });
  });
}

// --- the page's side of handing a document over ------------------------------
//
// The worker hands the document over by itself when another tab asks. The
// page listens to the same request so that a worker that is stuck does not
// keep the document for ever: stopping the worker frees its lock and file.

let channel = null;
let watchdog = 0;

function watch(id) {
  channel?.close();
  channel = new BroadcastChannel(`reticula-s4:document:${id}`);
  channel.onmessage = ({ data }) => {
    if (data.type !== "yield" || data.from === TAB || documentState?.state !== "open") {
      return;
    }
    clearTimeout(watchdog);
    watchdog = setTimeout(() => {
      if (documentState?.state !== "open") {
        return;
      }
      replaceWorker("the worker was stopped");
      documentState = {
        ...documentState,
        state: "evicted",
        note: "This tab's worker did not hand it over in time and was stopped.",
      };
      showDocument();
    }, GRACE_MS);
  };
}

// --- commands that only a page can carry out ----------------------------------

const pageCommands = {
  async page() {
    return {
      tab: TAB,
      visibility: document.visibilityState,
      crossOriginIsolated: window.crossOriginIsolated,
      persisted: await navigator.storage.persisted(),
      estimate: await navigator.storage.estimate(),
    };
  },

  /** Asks for storage that the browser will not clear by itself. Some browsers ask the user. */
  persist: () =>
    Promise.race([
      navigator.storage.persist().then((granted) => ({ granted })),
      sleep(3000).then(() => ({ granted: null, note: "no answer within 3 s" })),
    ]),

  terminateWorker() {
    replaceWorker("the worker was stopped");
    return true;
  },

  otherWorker: ({ command, args }) => askAnotherWorker(command, args),

  /** Keeps the worker, the page or both from answering for a while. */
  makeBusy({ worker: workerMs = 0, main: mainMs = 0 }) {
    if (workerMs > 0) {
      ask("spin", { ms: workerMs }).catch(() => {});
    }
    if (mainMs > 0) {
      // Late enough for this command's answer to leave first.
      setTimeout(() => {
        for (const end = performance.now() + mainMs; performance.now() < end; ) {
          // busy
        }
      }, 50);
    }
    return true;
  },

  /** The document as this page sees it, which is not the worker's view once the worker was stopped. */
  document: () => documentState,
};

async function call(command, args = {}) {
  try {
    if (command === "openDocument") {
      watch(args.id);
    }
    const run = pageCommands[command];
    return { ok: true, value: await (run ? run(args) : ask(command, args)) };
  } catch (error) {
    return { ok: false, error: { name: error?.name ?? "Error", message: error?.message ?? String(error) } };
  }
}

// --- on screen --------------------------------------------------------------

const element = (id) => document.getElementById(id);

const STATES = {
  opening: "Opening…",
  open: "Open in this tab.",
  blocked: "Open in another tab.",
  "taking over": "Moving the document here…",
  yielded: "Moved to another tab.",
  evicted: "Taken by another tab.",
  stuck: "Waiting for the other tab.",
};

function showDocument() {
  const state = documentState?.state;
  element("document-state").textContent = state ? (STATES[state] ?? state) : "Not open.";
  element("document-note").textContent =
    state === "open" ? `${documentState.records} records.` : (documentState?.note ?? "");
  element("open").hidden = state !== undefined;
  element("take-over").hidden = !["blocked", "yielded", "evicted", "stuck"].includes(state);
  element("append").hidden = state !== "open";
  element("close").hidden = state === undefined;
}

element("open").onclick = () => call("openDocument", { id: SAMPLE_DOCUMENT });
element("take-over").onclick = async () => {
  if (documentState?.state !== "blocked" && documentState?.state !== "stuck") {
    await call("openDocument", { id: SAMPLE_DOCUMENT });
  }
  if (documentState?.state !== "open") {
    await call("takeOver");
  }
};
element("append").onclick = () => call("appendDocument");
element("close").onclick = async () => {
  await call("closeDocument");
  documentState = null;
  showDocument();
};

const EXPERIMENTS = [
  ["environment", "What this browser has"],
  ["appendAndReadBack", "Appending records and reading them back"],
  ["tornTail", "A last record that was not written in full"],
  ["exclusivity", "A file that is open for writing"],
  ["flushLatency", "The cost of writing and flushing"],
  ["storage", "Quota and persistence"],
];

async function runAll() {
  const results = [];
  element("results").replaceChildren();
  for (const [command, title] of EXPERIMENTS) {
    element("progress").textContent = `${title}…`;
    const answer = await call(command);
    const failures = answer.ok ? (answer.value.failures ?? []) : [`${answer.error.name}: ${answer.error.message}`];
    results.push({ command, title, failures, value: answer.ok ? answer.value : null });

    const row = element("results").insertRow();
    row.insertCell().textContent = title;
    const verdict = row.insertCell();
    verdict.textContent = failures.length > 0 ? "failed" : "done";
    verdict.className = failures.length > 0 ? "failed" : "";
    const details = document.createElement("pre");
    details.textContent = JSON.stringify(answer.ok ? answer.value : answer.error, null, 1);
    row.insertCell().append(details);
  }
  element("progress").textContent = "Done.";
  return results;
}

element("run-all").onclick = () => runAll();

startWorker();
showDocument();
window.__opfs = { tab: TAB, call, runAll };
