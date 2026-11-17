// The experiments, written against whatever drives the browser: each opens
// tabs on the experiment page and tells them what to do.

import { performance } from "node:perf_hooks";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const round = (value) => Math.round(value * 100) / 100;
const megabytes = (bytes) => `${(bytes / 2 ** 20).toFixed(1)} MiB`;

function summary(values) {
  const sorted = [...values].sort((left, right) => left - right);
  const at = (fraction) => sorted[Math.min(sorted.length - 1, Math.floor(fraction * sorted.length))] ?? 0;
  return { count: sorted.length, p50: round(at(0.5)), max: round(sorted[sorted.length - 1] ?? 0) };
}

/**
 * Runs the experiments the browser can take part in. Resolves to one result
 * for each: whether it passed, what failed, and what it measured.
 */
export async function run(backend, { quick = false, only = "", say = console.log } = {}) {
  const results = [];
  let serial = 0;
  /** A name no earlier run has used, for locks, files and documents. */
  const fresh = (prefix) => `${prefix}-${Date.now().toString(36)}-${++serial}`;

  /**
   * One experiment. `needs` names what the browser's driver must be able to
   * do. `body` gets a function that opens a tab and one that records a
   * failure; what it returns is kept as the experiment's measurements.
   */
  async function experiment(id, title, needs, body) {
    if (only && only !== id) {
      return;
    }
    const missing = needs.filter((need) => !backend.can[need]);
    if (missing.length > 0) {
      results.push({ id, title, status: "skipped", reason: `needs ${missing.join(", ")}, which this browser driven this way cannot do` });
      say(`\n-- ${title}: skipped (needs ${missing.join(", ")})`);
      return;
    }
    say(`\n-- ${title}`);
    const failures = [];
    const tabs = [];
    const open = async () => {
      const tab = await backend.open();
      tabs.push(tab);
      return tab;
    };
    const expect = (holds, message) => {
      if (!holds) {
        failures.push(message);
        say(`   FAILED: ${message}`);
      }
      return holds;
    };
    const started = performance.now();
    let measured = null;
    try {
      measured = await body({ open, expect });
    } catch (error) {
      expect(false, `stopped by ${error.name}: ${error.message}`);
    }
    for (const tab of tabs) {
      await tab.close().catch(() => {});
    }
    const status = failures.length > 0 ? "failed" : "passed";
    results.push({ id, title, status, failures, seconds: round((performance.now() - started) / 1000), measured });
    say(`   ${status}`);
  }

  /** An experiment that is a single command of the page's worker. */
  const single = (id, title, command, args, report) =>
    experiment(id, title, [], async ({ open, expect }) => {
      const tab = await open();
      const result = await tab.call(command, args);
      for (const failure of result.failures ?? []) {
        expect(false, failure);
      }
      report?.(result);
      return result;
    });

  // --- within one tab -------------------------------------------------------

  await experiment("environment", "What this browser has", [], async ({ open, expect }) => {
    const tab = await open();
    const worker = await tab.call("environment");
    const page = await tab.call("page");
    for (const failure of worker.failures) {
      expect(false, failure);
    }
    backend.setVersion(worker.userAgent.match(/(?:Firefox|Version|Chrome)\/([\d.]+)/)?.[1] ?? "");
    say(`   ${worker.userAgent}`);
    say(`   isolated: ${worker.crossOriginIsolated}; clock resolution ${(worker.timerResolution * 1000).toFixed(0)} µs; quota ${(page.estimate.quota / 2 ** 30).toFixed(1)} GiB; persisted: ${page.persisted}`);
    return { worker, page };
  });

  await single("append", "Appending records and reading them back", "appendAndReadBack", { count: quick ? 300 : 2000 }, (result) =>
    say(`   ${result.records} records, ${megabytes(result.bytes)}: written in ${result.writeMs.toFixed(1)} ms, flushed in ${result.flushMs.toFixed(1)} ms`),
  );

  await single("torn", "A last record that was not written in full", "tornTail", {}, (result) =>
    say(`   ${result.cases} ways of damaging the last record, ${result.failed} read wrongly`),
  );

  await experiment("exclusive", "A file that is open for writing", [], async ({ open, expect }) => {
    const tab = await open();
    const result = await tab.call("exclusivity");
    for (const failure of result.failures) {
      expect(false, failure);
    }
    // The same from a second worker of the tab, and from a second tab.
    const path = `${fresh("held")}/file`;
    await tab.call("hold", { path });
    result.fromAnotherWorker = await tab.call("otherWorker", { command: "attempt", args: { path } });
    const other = await open();
    result.fromAnotherTab = await other.call("attempt", { path });
    await tab.call("drop", { path });
    result.fromAnotherTabAfterClosing = await other.call("attempt", { path });
    expect(!result.fromAnotherWorker.startsWith("allowed"), "another worker of the tab could open the file too");
    expect(!result.fromAnotherTab.startsWith("allowed"), "another tab could open the file too");
    expect(result.fromAnotherTabAfterClosing.startsWith("allowed"), `another tab could not open the file once it was closed: ${result.fromAnotherTabAfterClosing}`);
    for (const [what, outcome] of Object.entries(result)) {
      if (what !== "failures") {
        say(`   ${what.padEnd(28)} ${outcome}`);
      }
    }
    return result;
  });

  await single("latency", "The cost of writing and flushing", "flushLatency", { samples: quick ? 50 : 200 }, (result) => {
    say(`   clock resolution ${(result.timerResolution * 1000).toFixed(0)} µs; milliseconds for one record`);
    for (const size of result.sizes) {
      say(
        `   ${String(size.size).padStart(8)} bytes: write p50 ${size.write.p50.toFixed(3)} p95 ${size.write.p95.toFixed(3)}; flush p50 ${size.flush.p50.toFixed(3)} p95 ${size.flush.p95.toFixed(3)} max ${size.flush.max.toFixed(3)}; ` +
          `unflushed write ${size.unflushedWriteMean.toFixed(3)}, then one flush ${size.oneFlushForAll.toFixed(3)}`,
      );
    }
  });

  // --- a local document: one tab at a time ----------------------------------

  await experiment("document", "A local document is open in one tab at a time", [], async ({ open, expect }) => {
    const id = fresh("document");
    const first = await open();
    const second = await open();

    let here = await first.call("openDocument", { id });
    expect(here.state === "open", `the first tab found the document ${here.state}`);
    await first.call("appendDocument", { count: 5 });

    let there = await second.call("openDocument", { id });
    expect(there.state === "blocked", `the second tab found the document ${there.state}, not blocked`);
    const queried = await second.call("locks");

    there = await second.call("takeOver");
    expect(there.state === "open" && there.timeline.how === "handed over", `taking over ended ${there.state}, ${there.timeline?.how}`);
    expect(there.records === 5, `the second tab found ${there.records} of the 5 records`);
    here = await first.call("document");
    expect(here.state === "yielded", `the first tab is left ${here.state}`);
    const refused = await first.call("appendDocument").then(() => false, () => true);
    expect(refused, "the first tab could still write after handing the document over");
    await second.call("appendDocument", { count: 3 });

    // Back and forth, to time it.
    const moves = [there.timeline.open];
    let holder = second;
    let asker = first;
    for (let move = 0; move < (quick ? 4 : 20); move++) {
      const blocked = await asker.call("openDocument", { id });
      expect(blocked.state === "blocked", `move ${move}: found ${blocked.state}`);
      const taken = await asker.call("takeOver");
      if (!expect(taken.state === "open" && taken.records === 8, `move ${move}: ended ${taken.state} with ${taken.records} records`)) {
        break;
      }
      moves.push(taken.timeline.open);
      [holder, asker] = [asker, holder];
    }
    const handOverMs = summary(moves);
    say(`   asking for the document to having it open: p50 ${handOverMs.p50} ms, max ${handOverMs.max} ms over ${handOverMs.count} moves`);
    return { locksAsSeenByTheSecondTab: queried, handOverMs };
  });

  await experiment("unresponsive", "Taking a document from a tab that does not answer", [], async ({ open, expect }) => {
    const id = fresh("document");
    const first = await open();
    const second = await open();
    const measured = {};

    // The holder stops answering a second after it is told to, and the other
    // tab starts taking the document over two and a half seconds after that,
    // by itself. Nothing is asked of either tab in between: in some browsers
    // the tabs of a site share one thread, and a page that is stuck keeps the
    // other from answering too. Timers of a tab in the background can also
    // be a second late.
    const BUSY_AFTER = 1000;
    const ASK_AFTER = 3500;
    const takeFrom = async (holder, asker, busy, how) => {
      const found = await asker.call("openDocument", { id });
      expect(found.state === "blocked", `the asking tab found the document ${found.state}, not blocked`);
      await holder.call("makeBusy", { ...busy, after: BUSY_AFTER });
      await asker.call("takeOverLater", { delay: ASK_AFTER, ...how });
      for (const deadline = performance.now() + 60000; performance.now() < deadline; ) {
        const outcome = await asker.call("takenOver");
        if (outcome?.error) {
          throw Object.assign(new Error(outcome.error.message), { name: outcome.error.name });
        }
        if (outcome) {
          return outcome.result;
        }
        await sleep(100);
      }
      throw new Error("taking over did not come to an end within a minute");
    };

    // The holder's worker is stuck; its page is not, and stops the worker.
    await first.call("openDocument", { id });
    await first.call("appendDocument", { count: 2 });
    let taken = await takeFrom(first, second, { worker: 12000 }, { patience: 6000 });
    expect(taken.state === "open" && taken.records === 2, `worker stuck: ended ${taken.state} with ${taken.records} records`);
    expect(taken.timeline.how === "handed over", `worker stuck: the lock was ${taken.timeline.how}`);
    let left = await first.call("document");
    expect(left.state === "evicted", `worker stuck: the old tab is left ${left.state}`);
    measured.workerStuck = taken.timeline;
    say(`   worker stuck, page alive: ${taken.timeline.how} after ${round(taken.timeline.lock)} ms, open after ${round(taken.timeline.open)} ms (the page stops its worker)`);

    // The holder's page is stuck for eight seconds; its worker is not.
    // Whether the worker can then hand the document over by itself differs
    // between browsers, so that is recorded, not required.
    taken = await takeFrom(second, first, { main: 8000 }, { patience: 12000 });
    expect(taken.state === "open" && taken.timeline.how === "handed over", `page stuck: ended ${taken.state}, ${taken.timeline.how}`);
    const whileStuck = taken.timeline.open < 3000;
    measured.pageStuck = { ...taken.timeline, workerAnsweredWhileItsPageWasStuck: whileStuck };
    say(
      `   page stuck, worker alive: ${taken.timeline.how} after ${round(taken.timeline.lock)} ms, open after ${round(taken.timeline.open)} ms: ` +
        (whileStuck ? "the worker handed over while its page was stuck" : "the worker handed over only once its page answered again"),
    );
    // Answered once the page is itself again.
    await second.call("page");

    // Both are stuck: only force is left, and force gets the lock, not the file.
    taken = await takeFrom(first, second, { main: 8000, worker: 8000 }, { patience: 1500, filePatience: 15000 });
    expect(taken.timeline.how === "taken by force", `both stuck: the lock was ${taken.timeline.how}`);
    expect(taken.state === "open", `both stuck: ended ${taken.state}`);
    expect(
      taken.timeline.fileAttempts > 1 && taken.timeline.open - taken.timeline.lock > 500,
      `both stuck: the file opened ${round((taken.timeline.open ?? 0) - taken.timeline.lock)} ms after the lock was taken, at attempt ${taken.timeline.fileAttempts}, as if the old worker had not had it`,
    );
    measured.bothStuck = taken.timeline;
    say(`   both stuck: ${taken.timeline.how} after ${round(taken.timeline.lock)} ms, but the file opened only after ${round(taken.timeline.open ?? 0)} ms (${taken.timeline.fileAttempts} attempts)`);
    await first.call("page");
    await sleep(300);
    left = await first.call("document");
    expect(left.state === "evicted" || left.state === "yielded", `both stuck: the old tab is left ${left.state}`);
    measured.oldTabAfterForce = left.state;
    return measured;
  });

  // --- how soon a lock and a file come free ---------------------------------

  await experiment("release", "How soon a lock and a file come free", [], async ({ open, expect }) => {
    const ways = [
      ["let go", [], (holder, held) => holder.call("letGo", held)],
      ["worker stopped", [], (holder) => holder.call("terminateWorker")],
      ["tab closed", [], (holder) => holder.close()],
      ["tab's process killed", ["killTab"], (holder) => holder.kill()],
    ];
    const measured = {};
    for (const [way, needs, act] of ways) {
      if (needs.some((need) => !backend.can[need])) {
        say(`   ${way.padEnd(22)} not possible here`);
        measured[way] = "not possible here";
        continue;
      }
      const lockMs = [];
      const fileMs = [];
      const attempts = [];
      let waiterDied = 0;
      let note = "";
      for (let turn = 0; turn < (quick ? 3 : way.includes("killed") ? 5 : 10); turn++) {
        const held = { name: `reticula-s4:${fresh("release")}`, path: `${fresh("release")}/file` };
        const holder = await open();
        let waiter = await open();
        await holder.call("lock", { name: held.name, how: "available" });
        await holder.call("hold", { path: held.path });
        await waiter.call("lock", { name: held.name, how: "wait" });
        await sleep(50);

        const started = performance.now();
        try {
          await act(holder, held);
        } catch (error) {
          note = error.message;
          await holder.close();
          await waiter.close();
          break;
        }
        let got = false;
        while (!got && performance.now() - started < 10000) {
          try {
            got = (await waiter.call("lockState", { name: held.name })).held;
          } catch {
            // The waiting tab ran in the process that was killed. See whether
            // a new tab gets the lock instead.
            waiterDied++;
            waiter = await open();
            while (!got && performance.now() - started < 10000) {
              got = await waiter.call("lock", { name: held.name, how: "available" });
            }
          }
        }
        if (!expect(got, `${way}: the lock did not come free within 10 s`)) {
          break;
        }
        lockMs.push(performance.now() - started);
        const opened = await waiter.call("openAfter", { path: held.path, patience: 10000 });
        fileMs.push(opened.waited);
        attempts.push(opened.attempts);
        await waiter.call("unlock", { name: held.name });
        await holder.close().catch(() => {});
        await waiter.close();
      }
      measured[way] = note || { rounds: lockMs.length, lockMs: summary(lockMs), fileMs: summary(fileMs), fileAttempts: summary(attempts), waiterDied };
      say(
        note
          ? `   ${way.padEnd(22)} ${note}`
          : `   ${way.padEnd(22)} lock after p50 ${summary(lockMs).p50} ms, max ${summary(lockMs).max} ms; then the file after ${summary(fileMs).max} ms at most, ${summary(attempts).max} attempts at most${waiterDied ? `; the waiting tab died too in ${waiterDied} rounds, a new tab asked instead` : ""}`,
      );
    }
    return measured;
  });

  // --- replicas of a shared document ----------------------------------------

  await experiment("replicas", "A tab takes over the replica a closed tab left", [], async ({ open, expect }) => {
    const id = fresh("shared");
    const first = await open();
    const one = await first.call("openReplica", { id });
    expect(one.created && one.number === 1, `the first tab got replica ${one.number}, created: ${one.created}`);
    await first.call("appendReplica", { count: 3 });

    const second = await open();
    const two = await second.call("openReplica", { id });
    expect(two.created && two.number === 2, `with replica 1 in use, the second tab got replica ${two.number}, created: ${two.created}`);

    // Deleting a replica's directory from a tab that does not hold its lock.
    const removal = await second.call("removeReplicaDirectory", { id, number: 1 });
    const survived = await first.call("appendReplica", { count: 1 }).then((state) => state.records === 4, () => false);
    say(`   deleting the directory of a replica that is in use: ${removal}; its tab could ${survived ? "still" : "no longer"} write`);

    await first.close();
    const third = await open();
    const three = await third.call("openReplica", { id });
    if (removal.startsWith("allowed")) {
      // The directory was deleted from under the first tab, so there is nothing to take over.
      expect(three.number !== 2, `the third tab got replica ${three.number}, which is in use`);
    } else {
      expect(!three.created && three.number === 1 && three.records === 4, `the third tab got replica ${three.number}, created: ${three.created}, with ${three.records} records`);
    }
    say(`   after the first tab was closed, a new tab got replica ${three.number} with ${three.records} records, created: ${three.created}`);

    // Several tabs at once: two directories are free, the others must be new.
    await second.close();
    await third.close();
    const tabs = [await open(), await open(), await open(), await open()];
    const got = await Promise.all(tabs.map((tab) => tab.call("openReplica", { id })));
    const numbers = got.map((replica) => replica.number).sort();
    expect(new Set(numbers).size === 4, `four tabs at once got replicas ${numbers.join(", ")}`);
    const reused = got.filter((replica) => !replica.created).length;
    say(`   four tabs at once got replicas ${numbers.join(", ")}; ${reused} took over a directory that was free`);
    expect(reused === 2, `${reused} of the two free directories were taken over`);
    return { removalWhileInUse: removal, writerSurvivedRemoval: survived, takenOver: three, fourAtOnce: got };
  });

  // --- being killed ---------------------------------------------------------

  await experiment("crash", "What is left after being killed while writing", ["killBrowser"], async ({ open, expect }) => {
    const rounds = [];
    const plan = [
      ["browser", 1],
      ["tab", 1],
      ["browser", 0],
    ].flatMap((entry) => Array.from({ length: quick ? 1 : 3 }, () => entry));
    for (const [victim, flushEvery] of plan) {
      if (victim === "tab" && !backend.can.killTab) {
        continue;
      }
      const dir = fresh("crash");
      const tab = await open();
      await tab.call("startWriter", { dir, flushEvery });
      await sleep(300 + Math.random() * 1200);
      const kill = victim === "browser" ? backend.prepareKill() : () => tab.kill();
      const promised = await tab.call("writer");
      try {
        await kill();
      } catch (error) {
        rounds.push({ victim, flushEvery, note: error.message });
        say(`   ${victim} killed: ${error.message}`);
        continue;
      }
      const survivor = await open();
      const found = await survivor.call("verify", { dir });
      const reported = flushEvery ? promised.durable : promised.written;
      for (const problem of found.problems) {
        expect(false, `${victim} killed: ${problem}`);
      }
      if (flushEvery) {
        expect(found.last >= reported, `${victim} killed: ${reported} records were reported as flushed, ${found.last} are there`);
      }
      rounds.push({ victim, flushEvery, reported, found: found.last, files: found.files, tails: found.tails, fileAttempts: found.fileAttempts, fileWaited: round(found.fileWaited) });
      say(
        `   ${victim} killed, ${flushEvery ? "every record flushed" : "nothing flushed"}: ${reported} reported, ${found.last} found in ${found.files} files` +
          `${found.tails.length > 0 ? `, a tail of ${found.tails[0].bytes} bytes discarded (${found.tails[0].reason})` : ""}` +
          `${found.fileAttempts > found.files ? `; opening the files took ${found.fileAttempts} attempts, ${round(found.fileWaited)} ms` : ""}`,
      );
      await survivor.close();
    }
    expect(rounds.some((entry) => entry.found > 0), "no round wrote anything");
    return rounds;
  });

  // --- quota and persistence ------------------------------------------------

  await experiment("storage", "Quota and persistence", [], async ({ open, expect }) => {
    const tab = await open();
    const before = await tab.call("page");
    const asked = await tab.call("persist");
    const after = await tab.call("page");
    const measured = { quota: before.estimate.quota, usage: before.estimate.usage, persistedBefore: before.persisted, asked, persistedAfter: after.persisted };
    say(`   quota ${(before.estimate.quota / 2 ** 30).toFixed(2)} GiB; persisted before asking: ${before.persisted}; asking gave ${asked.granted === null ? asked.note : asked.granted}; persisted after: ${after.persisted}`);
    if (backend.can.grantPersistence) {
      const permitted = await open();
      await permitted.grantPersistence();
      measured.askedWithPermission = await permitted.call("persist");
      measured.persistedWithPermission = (await permitted.call("page")).persisted;
      say(`   with the permission granted: asking gave ${measured.askedWithPermission.granted}; persisted: ${measured.persistedWithPermission}`);
    }
    expect(typeof before.estimate.quota === "number", "the browser gave no quota estimate");
    return measured;
  });

  await experiment("quota", "Writing until the quota is used up", ["limitQuota"], async ({ expect }) => {
    const measured = [];
    for (const limit of [16, 48, 100].map((mebibytes) => mebibytes * 2 ** 20)) {
      const small = await backend.limited(limit);
      try {
        const tab = await small.open();
        const estimate = (await tab.call("page")).estimate;
        const result = await tab.call("fill", { chunk: 2 ** 18, limit: 4 * limit });
        const stoppedBy = result.error
          ? `${result.error.name} during ${result.error.during}`
          : result.shortWrite
            ? `a write of ${result.shortWrite.asked} bytes that wrote ${result.shortWrite.wrote}`
            : "the experiment's own limit";
        say(`   quota ${megabytes(limit)} (the page is told ${megabytes(estimate.quota)}): wrote ${megabytes(result.written)} before ${stoppedBy}`);
        say(`      the file was left ${megabytes(result.sizeAfterRefusal)} long, ${result.partialBlockLeftBehind} bytes of it a block not written in full; flushing then: ${result.flush}; a small record after giving one block back: ${result.afterGivingBackOneBlock}`);
        expect(result.error !== null || result.shortWrite !== undefined, `quota ${megabytes(limit)}: ${megabytes(result.written)} were written and nothing refused`);
        measured.push({ limit, estimate, ...result });
      } finally {
        await small.close();
      }
    }
    return measured;
  });

  return results;
}
