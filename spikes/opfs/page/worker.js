// The worker of the experiment page. It stands in for the engine's worker:
// it is the one that holds the locks and the files, as the engine's will.
//
// The page sends `{ id, command, args }` and gets `{ id, value }` or
// `{ id, error }` back. Changes to the open document are also sent on their
// own, as `{ event: "document", document }`.

import { encode, HEADER, intact, payloadOf, scan, sequenceOf } from "./log.js";

/** Everything the experiments write is under this directory. */
const ROOT = "reticula-s4";
/** The tab this worker belongs to; the page passes it as the worker's name. */
const TAB = self.name;

const now = () => performance.now();
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const describeError = (error) => ({ name: error?.name ?? "Error", message: error?.message ?? String(error) });

/** Lets the messages that are waiting be handled, without the delay of a timer. */
const breathe = (() => {
  const { port1, port2 } = new MessageChannel();
  let wake = () => {};
  port1.onmessage = () => wake();
  return () =>
    new Promise((resolve) => {
      wake = resolve;
      port2.postMessage(0);
    });
})();

function summary(values) {
  const sorted = Float64Array.from(values).sort();
  const at = (fraction) => sorted[Math.min(sorted.length - 1, Math.floor(fraction * sorted.length))] ?? 0;
  return {
    count: sorted.length,
    mean: sorted.reduce((sum, value) => sum + value, 0) / (sorted.length || 1),
    p50: at(0.5),
    p95: at(0.95),
    p99: at(0.99),
    max: sorted[sorted.length - 1] ?? 0,
  };
}

/** The smallest step the clock was seen to take, in milliseconds. */
function timerResolution() {
  let smallest = Number.POSITIVE_INFINITY;
  for (let previous = now(), seen = 0; seen < 50; ) {
    const time = now();
    if (time > previous) {
      smallest = Math.min(smallest, time - previous);
      previous = time;
      seen++;
    }
  }
  return smallest;
}

// --- files ------------------------------------------------------------------

async function directory(path, create = true) {
  let handle = await navigator.storage.getDirectory();
  for (const part of [ROOT, ...path.split("/").filter(Boolean)]) {
    handle = await handle.getDirectoryHandle(part, { create });
  }
  return handle;
}

function split(path) {
  const slash = path.lastIndexOf("/");
  return [path.slice(0, slash), path.slice(slash + 1)];
}

async function fileHandle(path, create = true) {
  const [parent, name] = split(path);
  return (await directory(parent, create)).getFileHandle(name, { create });
}

/** Removes a file or a directory with everything in it; one that is not there is fine. */
async function remove(path) {
  const [parent, name] = split(path);
  try {
    await (await directory(parent)).removeEntry(name, { recursive: true });
  } catch (error) {
    if (error.name !== "NotFoundError") {
      throw error;
    }
  }
}

/**
 * The errors that opening a file fails with while another context has it
 * open: the first in Chromium and Firefox, the second in Safari.
 */
const IN_USE = new Set(["NoModificationAllowedError", "InvalidStateError"]);

/**
 * Opens a file for reading and writing. Only one context can have a file open
 * this way; while another has it, opening fails, and this tries again until
 * `patience` milliseconds have passed.
 */
async function access(path, patience = 0) {
  const handle = await fileHandle(path);
  const started = now();
  for (let attempts = 1; ; attempts++) {
    try {
      return { file: await handle.createSyncAccessHandle(), attempts, waited: now() - started };
    } catch (error) {
      if (!IN_USE.has(error.name) || now() - started >= patience) {
        throw error;
      }
      await sleep(20);
    }
  }
}

function contents(file) {
  const bytes = new Uint8Array(file.getSize());
  let read = 0;
  while (read < bytes.length) {
    const count = file.read(bytes.subarray(read), { at: read });
    if (count === 0) {
      break;
    }
    read += count;
  }
  return bytes.subarray(0, read);
}

/** What an action did: "allowed", or the name of the error it failed with. */
async function outcome(action) {
  try {
    const note = await action();
    return note ? `allowed: ${note}` : "allowed";
  } catch (error) {
    return error?.name ?? String(error);
  }
}

// --- locks ------------------------------------------------------------------

/** Locks this worker holds: name to the function that lets go. */
const held = new Map();
/** Locks this worker asked for and is still waiting for, or has just got. */
const waits = new Map();
/** Locks that were taken from this worker by force. */
const lost = [];

/**
 * Asks for an exclusive lock and keeps it until `release`. Resolves to
 * whether the lock was granted; with `ifAvailable` it is not when another
 * context holds it. With `patience`, waiting longer than that many
 * milliseconds rejects with an AbortError. With `steal` the lock is taken
 * from whoever holds it.
 */
function acquire(name, { ifAvailable = false, steal = false, patience = Number.POSITIVE_INFINITY } = {}) {
  return new Promise((resolve, reject) => {
    const options = { mode: "exclusive" };
    let timer = 0;
    if (ifAvailable) {
      options.ifAvailable = true;
    } else if (steal) {
      options.steal = true;
    } else if (Number.isFinite(patience)) {
      const controller = new AbortController();
      options.signal = controller.signal;
      timer = setTimeout(() => controller.abort(), patience);
    }
    navigator.locks
      .request(name, options, (lock) => {
        clearTimeout(timer);
        if (!lock) {
          resolve(false);
          return undefined;
        }
        return new Promise((release) => {
          held.set(name, release);
          resolve(true);
        });
      })
      .catch((error) => {
        clearTimeout(timer);
        // The request fails either while it waits, or after it was granted:
        // then another context took the lock by force.
        if (held.delete(name)) {
          lost.push({ name, error: error.name });
          onLost(name);
        } else {
          reject(error);
        }
      });
  });
}

function release(name) {
  const letGo = held.get(name);
  held.delete(name);
  letGo?.();
  return letGo !== undefined;
}

// --- one document, one tab --------------------------------------------------
//
// A local document may be open in one tab at a time. The tab that has it holds
// a lock named after the document, and has the document's log open. A second
// tab finds the lock taken and says so; from there the user can move the
// document to that tab:
//
//   1. The second tab asks the holder to hand the document over, and waits for
//      the lock. The holder's worker closes the log and lets go of the lock.
//   2. The holder's page hears the request too. If its worker has not handed
//      the document over shortly after, it stops the worker, which frees both
//      the lock and the file.
//   3. If the lock still does not come, the second tab takes it by force. That
//      gives it the lock but not the file: the file stays with the old worker
//      until that worker closes it or ends. So it then tries to open the file
//      for a while, and reports being stuck if it cannot.

let doc = null;
const nameOf = (id) => `reticula-s4:document:${id}`;
const logOf = (id) => `documents/${id}/log`;

function describe() {
  return doc && { id: doc.id, state: doc.state, records: doc.records, note: doc.note, timeline: doc.timeline };
}

function announce() {
  self.postMessage({ event: "document", document: describe() });
}

async function openLog(patience) {
  const opened = await access(logOf(doc.id), patience);
  const found = scan(contents(opened.file));
  if (found.tail.bytes > 0) {
    opened.file.truncate(found.end);
  }
  doc.file = opened.file;
  doc.at = found.end;
  doc.records = found.records.length;
  return opened;
}

function closeLog() {
  doc.file?.close();
  doc.file = null;
}

/** The lock is ours; get the file too. */
async function finishOpening(timeline, started, patience) {
  doc.started = started;
  try {
    const opened = await openLog(patience);
    timeline.fileAttempts = (timeline.fileAttempts ?? 0) + opened.attempts;
    timeline.open = now() - started;
    doc.state = "open";
    doc.note = "";
  } catch (error) {
    if (!IN_USE.has(error.name)) {
      throw error;
    }
    timeline.gaveUp = now() - started;
    doc.state = "stuck";
    doc.note = "The other tab still has the document's file open. Close the other tab.";
  }
  doc.timeline = timeline;
  announce();
  return describe();
}

function handOver() {
  if (doc?.state !== "open") {
    return;
  }
  closeLog();
  release(nameOf(doc.id));
  doc.state = "yielded";
  doc.channel.postMessage({ type: "yielded", from: TAB });
  announce();
}

function onLost(name) {
  if (doc && name === nameOf(doc.id) && doc.state === "open") {
    closeLog();
    doc.state = "evicted";
    doc.note = "This tab did not hand it over in time.";
    announce();
  }
}

function forget() {
  if (!doc) {
    return;
  }
  closeLog();
  release(nameOf(doc.id));
  doc.channel.close();
  doc = null;
}

const documents = {
  async openDocument({ id }) {
    forget();
    doc = { id, state: "opening", records: 0, at: 0, file: null, note: "", timeline: null, channel: new BroadcastChannel(nameOf(id)) };
    doc.channel.onmessage = ({ data }) => {
      if (data.type === "yield" && data.from !== TAB) {
        handOver();
      }
    };
    const started = now();
    if (!(await acquire(nameOf(id), { ifAvailable: true }))) {
      doc.state = "blocked";
      announce();
      return describe();
    }
    return finishOpening({ how: "free", lock: now() - started }, started, 0);
  },

  async takeOver({ patience = 2000, filePatience = 10000 }) {
    if (!doc || doc.state === "open") {
      throw new Error("there is no document here to take over");
    }
    const started = now();
    const name = nameOf(doc.id);
    const timeline = { how: "handed over" };
    doc.state = "taking over";
    announce();
    if (!held.has(name)) {
      doc.channel.postMessage({ type: "yield", from: TAB });
      try {
        await acquire(name, { patience });
      } catch (error) {
        if (error.name !== "AbortError") {
          throw error;
        }
        await acquire(name, { steal: true });
        timeline.how = "taken by force";
      }
    }
    timeline.lock = now() - started;
    return finishOpening(timeline, started, filePatience);
  },

  /** After being stuck: the lock is held, try the file again. */
  async retryOpen({ patience = 5000 }) {
    if (doc?.state !== "stuck") {
      throw new Error("the document is not stuck");
    }
    return finishOpening({ ...doc.timeline }, doc.started, patience);
  },

  appendDocument({ count = 1 }) {
    if (doc?.state !== "open") {
      throw new Error(`the document is ${doc?.state ?? "not open"} in this tab`);
    }
    for (let added = 0; added < count; added++) {
      const record = encode(payloadOf(doc.records + 1, 64));
      doc.at += doc.file.write(record, { at: doc.at });
      doc.file.flush();
      doc.records++;
    }
    announce();
    return describe();
  },

  closeDocument() {
    forget();
    announce();
    return null;
  },

  document: () => describe(),
};

// --- replicas of a shared document ------------------------------------------
//
// Every tab that opens a shared document is a replica of its own, with a
// directory of its own that holds what the replica has not sent yet. A tab
// that closes leaves its directory behind, and the next tab to open the
// document takes it over. Choosing a directory is done by one tab at a time.

let replica = null;
const replicaLock = (id, number) => `reticula-s4:replica:${id}:${number}`;
const replicaLog = (id, number) => `replicas/${id}/${number}/log`;

const replicas = {
  async openReplica({ id }) {
    replicas.closeReplica();
    return navigator.locks.request(`reticula-s4:replicas:${id}`, async () => {
      const parent = await directory(`replicas/${id}`);
      const numbers = [];
      for await (const [name, handle] of parent.entries()) {
        if (handle.kind === "directory" && /^\d+$/.test(name)) {
          numbers.push(Number(name));
        }
      }
      numbers.sort((left, right) => left - right);
      let number = 0;
      for (const candidate of numbers) {
        if (await acquire(replicaLock(id, candidate), { ifAvailable: true })) {
          number = candidate;
          break;
        }
      }
      const created = number === 0;
      if (created) {
        // In the product the server hands out the number of a new replica.
        number = (numbers.at(-1) ?? 0) + 1;
        if (!(await acquire(replicaLock(id, number), { ifAvailable: true }))) {
          throw new Error(`the lock of the new replica ${number} is taken`);
        }
      }
      const { file } = await access(replicaLog(id, number), 2000);
      const found = scan(contents(file));
      replica = { id, number, file, at: found.end, records: found.records.length };
      return { number, created, records: replica.records, existing: numbers };
    });
  },

  appendReplica({ count = 1 }) {
    for (let added = 0; added < count; added++) {
      const record = encode(payloadOf(replica.records + 1, 64));
      replica.at += replica.file.write(record, { at: replica.at });
      replica.file.flush();
      replica.records++;
    }
    return { number: replica.number, records: replica.records };
  },

  closeReplica() {
    if (replica) {
      replica.file.close();
      release(replicaLock(replica.id, replica.number));
      replica = null;
    }
    return null;
  },

  /** Deletes a replica's directory without asking for its lock first. */
  removeReplicaDirectory: ({ id, number }) =>
    outcome(async () => (await directory(`replicas/${id}`)).removeEntry(String(number), { recursive: true })),
};

// --- a writer to be killed --------------------------------------------------

let writer = null;
const logFile = (dir, index) => `${dir}/${String(index).padStart(6, "0")}.log`;

const crash = {
  /**
   * Starts appending records and does not stop. A new file is begun every
   * `rotateEvery` records, so that creating files is part of what is killed.
   * With `flushEvery` 0 nothing is ever flushed.
   */
  async startWriter({ dir, size = 200, flushEvery = 1, rotateEvery = 200 }) {
    await remove(dir);
    writer = { dir, written: 0, durable: 0, files: 0, error: null };
    const state = writer;
    (async () => {
      let file = null;
      let at = 0;
      for (let sequence = 1; ; sequence++) {
        if ((sequence - 1) % rotateEvery === 0) {
          if (file) {
            if (flushEvery) {
              file.flush();
              state.durable = sequence - 1;
            }
            file.close();
          }
          ({ file } = await access(logFile(dir, state.files)));
          state.files++;
          at = 0;
        }
        at += file.write(encode(payloadOf(sequence, size)), { at });
        state.written = sequence;
        if (flushEvery && sequence % flushEvery === 0) {
          file.flush();
          state.durable = sequence;
        }
        if (sequence % 4 === 0) {
          await breathe();
        }
      }
    })().catch((error) => {
      state.error = describeError(error);
    });
    return true;
  },

  /** How far the writer is. What it answers here, it must still have after being killed. */
  writer: () => writer && { written: writer.written, durable: writer.durable, files: writer.files, error: writer.error },

  /** Reads back what a writer left behind. */
  async verify({ dir, rotateEvery = 200, patience = 5000 }) {
    const parent = await directory(dir);
    const names = [];
    for await (const name of parent.keys()) {
      names.push(name);
    }
    names.sort();
    const problems = [];
    const tails = [];
    let last = 0;
    let attempts = 0;
    let waited = 0;
    for (const [index, name] of names.entries()) {
      if (name !== logFile("", index).slice(1)) {
        problems.push(`unexpected file ${name} in place ${index}`);
      }
      const opened = await access(`${dir}/${name}`, patience);
      attempts += opened.attempts;
      waited += opened.waited;
      const bytes = contents(opened.file);
      opened.file.close();
      const found = scan(bytes);
      for (const record of found.records) {
        const payload = bytes.subarray(record.at, record.at + record.length);
        const sequence = sequenceOf(payload);
        if (sequence !== last + 1) {
          problems.push(`${name}: record ${sequence} follows record ${last}`);
        } else if (!intact(payload)) {
          problems.push(`${name}: record ${sequence} has a valid checksum but the wrong bytes`);
        } else if (Math.floor((sequence - 1) / rotateEvery) !== index) {
          problems.push(`${name}: record ${sequence} is in the wrong file`);
        }
        last = sequence;
      }
      if (found.tail.bytes > 0) {
        tails.push({ file: name, bytes: found.tail.bytes, reason: found.tail.reason });
        if (index !== names.length - 1) {
          problems.push(`${name}: ${found.tail.bytes} bytes of damage in a file that is not the last`);
        }
      }
    }
    return { files: names.length, last, tails, problems: problems.slice(0, 10), fileAttempts: attempts, fileWaited: waited };
  },
};

// --- experiments within one tab ---------------------------------------------

/**
 * Goes through the file operations everything else is built on, one at a
 * time, and says which ones this browser carries out. Where an experiment
 * fails, this shows what it failed on.
 */
async function fileOperations() {
  const steps = {};
  let stopped = false;
  const step = async (name, action) => {
    if (stopped) {
      return;
    }
    try {
      await action();
      steps[name] = "ok";
    } catch (error) {
      steps[name] = `${error.name}: ${error.message}`;
      stopped = true;
    }
  };
  const name = `${ROOT}-probe-${TAB}`;
  let root;
  let parent;
  let handle;
  let file;
  await step("getDirectory", async () => {
    root = await navigator.storage.getDirectory();
  });
  await step("getDirectoryHandle", async () => {
    parent = await root.getDirectoryHandle(name, { create: true });
  });
  await step("getFileHandle", async () => {
    handle = await parent.getFileHandle("file", { create: true });
  });
  await step("createSyncAccessHandle", async () => {
    file = await handle.createSyncAccessHandle();
  });
  await step("write", () => {
    if (file.write(new Uint8Array([1, 2, 3, 4]), { at: 0 }) !== 4) {
      throw new Error("a write of 4 bytes did not write 4");
    }
  });
  await step("flush", () => file.flush());
  await step("getSize", () => {
    if (file.getSize() !== 4) {
      throw new Error(`the size is ${file.getSize()}, not 4`);
    }
  });
  await step("read", () => {
    const bytes = new Uint8Array(4);
    if (file.read(bytes, { at: 0 }) !== 4 || bytes[3] !== 4) {
      throw new Error("what was written did not read back");
    }
  });
  await step("truncate", () => file.truncate(2));
  await step("close", () => file.close());
  await step("entries", async () => {
    const names = [];
    for await (const [entry] of parent.entries()) {
      names.push(entry);
    }
    if (names.join() !== "file") {
      throw new Error(`the directory lists ${names.join() || "nothing"}`);
    }
  });
  await step("removeEntry", () => parent.removeEntry("file"));
  await step("removeEntry, a missing file", async () => {
    try {
      await parent.removeEntry("file");
      throw new Error("removing a file that is not there succeeded");
    } catch (error) {
      if (error.name !== "NotFoundError") {
        throw error;
      }
    }
  });
  await step("removeEntry, a directory", () => root.removeEntry(name, { recursive: true }));
  return steps;
}

const experiments = {
  async environment() {
    const features = {
      locks: typeof navigator.locks?.request === "function",
      locksQuery: typeof navigator.locks?.query === "function",
      originPrivateFileSystem: typeof navigator.storage?.getDirectory === "function",
      syncAccessHandle: typeof FileSystemFileHandle === "function" && "createSyncAccessHandle" in FileSystemFileHandle.prototype,
      broadcastChannel: typeof BroadcastChannel === "function",
      estimate: typeof navigator.storage?.estimate === "function",
    };
    const fileOperationsTried = await fileOperations();
    return {
      userAgent: navigator.userAgent,
      brands: navigator.userAgentData?.brands?.map((brand) => `${brand.brand} ${brand.version}`) ?? null,
      crossOriginIsolated: self.crossOriginIsolated,
      timerResolution: timerResolution(),
      hardwareConcurrency: navigator.hardwareConcurrency,
      features,
      fileOperations: fileOperationsTried,
      failures: [
        ...Object.entries(features).filter(([, present]) => !present).map(([name]) => `${name} is missing`),
        ...Object.entries(fileOperationsTried).filter(([, result]) => result !== "ok").map(([name, result]) => `${name} failed with ${result}`),
      ],
    };
  },

  /** Appends records of several sizes, flushes, and reads them back through a new handle. */
  async appendAndReadBack({ count = 2000 }) {
    const sizes = [4, 5, 11, 100, 1000, 4096, 65536];
    const path = "append/log";
    await remove("append");
    const { file } = await access(path);
    let at = 0;
    const started = now();
    for (let sequence = 1; sequence <= count; sequence++) {
      const record = encode(payloadOf(sequence, sizes[sequence % sizes.length]));
      const wrote = file.write(record, { at });
      if (wrote !== record.length) {
        throw new Error(`a write of ${record.length} bytes wrote ${wrote}`);
      }
      at += wrote;
    }
    const written = now();
    file.flush();
    const flushed = now();
    file.close();

    const failures = [];
    const again = await access(path);
    const bytes = contents(again.file);
    again.file.close();
    const found = scan(bytes);
    if (bytes.length !== at) {
      failures.push(`wrote ${at} bytes, the file has ${bytes.length}`);
    }
    if (found.records.length !== count || found.tail.bytes !== 0) {
      failures.push(`wrote ${count} records, found ${found.records.length} and a tail of ${found.tail.bytes} bytes`);
    }
    for (const [index, record] of found.records.entries()) {
      const payload = bytes.subarray(record.at, record.at + record.length);
      if (sequenceOf(payload) !== index + 1 || !intact(payload) || payload.length !== sizes[(index + 1) % sizes.length]) {
        failures.push(`record ${index + 1} is not what was written`);
        break;
      }
    }
    // The same file read the way a page without a worker would read it.
    const blob = new Uint8Array(await (await (await fileHandle(path)).getFile()).arrayBuffer());
    if (blob.length !== bytes.length || blob.some((byte, index) => byte !== bytes[index])) {
      failures.push("the file read through getFile() differs");
    }
    await remove("append");
    return { records: count, bytes: at, writeMs: written - started, flushMs: flushed - written, failures };
  },

  /**
   * Damages the last record of a log in every way a write that did not finish
   * can, and checks each time that the reader keeps exactly the records
   * before it.
   */
  async tornTail() {
    const total = 12;
    await remove("torn");
    const { file } = await access("torn/log");
    let lastAt = 0;
    for (let sequence = 1; sequence < total; sequence++) {
      lastAt += file.write(encode(payloadOf(sequence, [4, 40, 400][sequence % 3])), { at: lastAt });
    }
    const last = encode(payloadOf(total, 300));
    const restore = () => file.write(last, { at: lastAt });
    restore();
    file.flush();

    const failures = [];
    let cases = 0;
    const check = (label, kept, reason) => {
      cases++;
      const found = scan(contents(file));
      const right =
        found.records.length === total - 1 &&
        found.end === lastAt &&
        found.tail.bytes === file.getSize() - lastAt &&
        (reason === undefined || found.tail.reason === reason);
      if (!right) {
        failures.push(`${label}, ${kept} bytes kept: ${found.records.length} records, tail ${found.tail.bytes} bytes (${found.tail.reason})`);
      }
    };

    // The file ends inside the record.
    for (let kept = 0; kept < last.length; kept++) {
      file.truncate(lastAt + kept);
      check("cut short", kept, kept === 0 ? null : kept < HEADER ? "incomplete header" : "incomplete record");
      restore();
    }
    // The file has its full length, but the end of the record never arrived
    // and reads as zeros.
    const zeros = new Uint8Array(last.length);
    for (let kept = 0; kept < last.length; kept++) {
      if (last.subarray(kept).every((byte) => byte === 0)) {
        continue;
      }
      file.write(zeros.subarray(kept), { at: lastAt + kept });
      check("zeros", kept);
      restore();
    }
    // The length is nonsense.
    file.write(new Uint8Array([0xff, 0xff, 0xff, 0xff]), { at: lastAt });
    check("length", 0, "length over the limit");
    restore();

    const whole = scan(contents(file));
    if (whole.records.length !== total || whole.tail.bytes !== 0) {
      failures.push(`after restoring: ${whole.records.length} records, tail ${whole.tail.bytes} bytes`);
    }
    file.close();
    await remove("torn");
    return { cases, failed: failures.length, failures: failures.slice(0, 5) };
  },

  /** What writing a record and flushing it costs, by the size of the record. */
  async flushLatency({ samples = 200, sizes = [64, 1024, 16384, 262144, 1048576] }) {
    const results = [];
    for (const size of sizes) {
      await remove("latency");
      const { file } = await access("latency/log");
      const record = encode(payloadOf(1, size - HEADER));
      const large = size >= 262144;
      const count = large ? Math.min(samples, 30) : samples;
      const warmup = large ? 5 : 20;
      const write = [];
      const flush = [];
      let at = 0;
      for (let index = 0; index < count + warmup; index++) {
        const started = now();
        at += file.write(record, { at });
        const written = now();
        file.flush();
        const flushed = now();
        if (index >= warmup) {
          write.push(written - started);
          flush.push(flushed - written);
        }
      }
      // The same records without a flush after each, then one flush for all.
      const started = now();
      for (let index = 0; index < count; index++) {
        at += file.write(record, { at });
      }
      const written = now();
      file.flush();
      const flushed = now();
      file.close();
      results.push({
        size,
        samples: count,
        write: summary(write),
        flush: summary(flush),
        both: summary(write.map((value, index) => value + flush[index])),
        unflushedWriteMean: (written - started) / count,
        oneFlushForAll: flushed - written,
      });
    }
    await remove("latency");
    return { timerResolution: timerResolution(), sizes: results, failures: [] };
  },

  /** What else can be done with a file while it is open for writing. */
  async exclusivity() {
    await remove("exclusive");
    const path = "exclusive/file";
    const handle = await fileHandle(path);
    const first = await handle.createSyncAccessHandle();
    first.write(new Uint8Array([1, 2, 3]), { at: 0 });
    first.flush();
    const result = {
      secondHandle: await outcome(async () => (await handle.createSyncAccessHandle()).close()),
      secondHandleLookedUpAgain: await outcome(async () => (await (await fileHandle(path)).createSyncAccessHandle()).close()),
      getFile: await outcome(async () => `${(await (await handle.getFile()).arrayBuffer()).byteLength} bytes`),
      createWritable: await outcome(async () => (await handle.createWritable()).abort()),
    };
    first.close();
    result.afterClosing = await outcome(async () => (await handle.createSyncAccessHandle()).close());

    // Deleting a file, or the directory it is in, while the file is open.
    const second = await handle.createSyncAccessHandle();
    result.removeFile = await outcome(async () => (await directory("exclusive")).removeEntry("file"));
    result.removeDirectory = await outcome(async () => (await directory("")).removeEntry("exclusive", { recursive: true }));
    result.writeAfterThat = await outcome(() => {
      second.write(new Uint8Array([4]), { at: 3 });
      second.flush();
      return `${second.getSize()} bytes`;
    });
    second.close();

    // Other ways of opening, where the browser has them.
    await remove("exclusive");
    const fresh = await fileHandle(path);
    for (const mode of ["read-only", "readwrite-unsafe"]) {
      result[mode] = await outcome(async () => {
        const one = await fresh.createSyncAccessHandle({ mode });
        try {
          (await fresh.createSyncAccessHandle({ mode })).close();
          return "two at once";
        } catch (error) {
          return `one at a time (${error.name})`;
        } finally {
          one.close();
        }
      });
    }
    await remove("exclusive");
    const failures = [];
    if (result.secondHandle.startsWith("allowed") || result.secondHandleLookedUpAgain.startsWith("allowed")) {
      failures.push("a file that is open for writing could be opened a second time");
    }
    if (!result.afterClosing.startsWith("allowed")) {
      failures.push(`after closing, opening again failed: ${result.afterClosing}`);
    }
    return { ...result, failures };
  },

  async storage() {
    return { persisted: await navigator.storage.persisted(), estimate: await navigator.storage.estimate() };
  },

  /**
   * Writes until the browser refuses, to see where and how it refuses. Meant
   * to be run with the quota turned down.
   */
  async fill({ chunk = 1 << 20, limit = 1 << 28 }) {
    await remove("fill");
    const { file } = await access("fill/log");
    const block = encode(payloadOf(1, chunk - HEADER));
    const result = { before: await navigator.storage.estimate(), written: 0, error: null };
    let at = 0;
    try {
      while (at < limit) {
        const wrote = file.write(block, { at });
        at += wrote;
        if (wrote !== block.length) {
          result.shortWrite = { asked: block.length, wrote };
          break;
        }
      }
    } catch (error) {
      result.error = { during: "write", ...describeError(error) };
    }
    result.written = at;
    result.sizeAfterRefusal = file.getSize();
    result.partialBlockLeftBehind = result.sizeAfterRefusal % block.length;
    result.flush = await outcome(() => file.flush());
    result.atRefusal = await navigator.storage.estimate();
    // Is a small record still accepted once the last block is given back?
    result.afterGivingBackOneBlock = await outcome(() => {
      file.truncate(Math.max(0, result.sizeAfterRefusal - result.partialBlockLeftBehind - block.length));
      file.write(encode(payloadOf(2, 64)), { at: file.getSize() });
      file.flush();
    });
    file.truncate(0);
    file.flush();
    file.close();
    await remove("fill");
    result.afterRemoving = await navigator.storage.estimate();
    return result;
  },
};

// --- pieces the multi-tab experiments are put together from -------------------

/** Files kept open by `hold`. */
const open = new Map();

const pieces = {
  async lock({ name, how }) {
    if (how === "wait") {
      waits.set(name, { since: now(), acquired: null });
      acquire(name).then(() => {
        waits.get(name).acquired = now();
      });
      return "waiting";
    }
    return acquire(name, how === "steal" ? { steal: true } : { ifAvailable: true });
  },

  lockState({ name }) {
    const wait = waits.get(name);
    return { held: held.has(name), waited: wait?.acquired ? wait.acquired - wait.since : null, lost: lost.filter((entry) => entry.name === name) };
  },

  unlock: ({ name }) => release(name),

  /** Who holds and who waits for the locks of these experiments, as the browser tells it. */
  async locks() {
    const { held: holders, pending } = await navigator.locks.query();
    const ours = (entries) => entries.filter((entry) => entry.name.startsWith("reticula-s4:")).map((entry) => ({ name: entry.name, client: entry.clientId }));
    return { held: ours(holders), pending: ours(pending) };
  },

  async hold({ path }) {
    const opened = await access(path, 2000);
    open.set(path, opened.file);
    return { attempts: opened.attempts };
  },

  drop({ path }) {
    open.get(path)?.close();
    return open.delete(path);
  },

  /** Lets go of a lock and a file in one step. */
  letGo({ name, path }) {
    pieces.drop({ path });
    return release(name);
  },

  /** Tries once to open a file that another context may have open. */
  attempt: ({ path }) => outcome(async () => (await (await fileHandle(path)).createSyncAccessHandle()).close()),

  /** Opens a file that another context has just let go of, and says how long that took. */
  async openAfter({ path, patience = 5000 }) {
    const opened = await access(path, patience);
    opened.file.close();
    return { attempts: opened.attempts, waited: opened.waited };
  },

  /** Keeps this worker from answering anything for a while. */
  spin({ ms }) {
    for (const end = now() + ms; now() < end; ) {
      // busy
    }
    return true;
  },

  async clear() {
    forget();
    replicas.closeReplica();
    for (const file of open.values()) {
      file.close();
    }
    open.clear();
    const root = await navigator.storage.getDirectory();
    await outcome(() => root.removeEntry(ROOT, { recursive: true }));
    return true;
  },
};

const commands = { ...experiments, ...documents, ...replicas, ...crash, ...pieces };

self.onmessage = async ({ data: { id, command, args } }) => {
  try {
    const run = commands[command];
    if (!run) {
      throw new Error(`unknown command ${command}`);
    }
    self.postMessage({ id, value: await run(args ?? {}) });
  } catch (error) {
    self.postMessage({ id, error: describeError(error) });
  }
};
