import { describe, expect, it } from "vitest";
import { Turns } from "./turns.ts";

/** Time passes only when something is done, and turns come when they are called for. */
function bench(sliceMs = 8) {
  let time = 100;
  let deferred: (() => void)[] = [];
  const log: string[] = [];
  let pages = 0;
  const turns = new Turns({
    pages: () => {
      if (pages > 0) {
        pages--;
        time += 4;
        log.push("page");
      }
      return pages > 0;
    },
    sliceMs,
    now: () => time,
    defer: (run) => deferred.push(run),
  });
  return {
    turns,
    log,
    spend: (ms: number, what: string) => {
      time += ms;
      log.push(what);
    },
    due: (count: number) => {
      pages += count;
      turns.wake();
    },
    /** What was deferred, and what follows from it at once. */
    next: async () => {
      const now = deferred;
      deferred = [];
      for (const run of now) {
        run();
      }
      for (let step = 0; step < 20; step++) {
        await Promise.resolve();
      }
      return now.length;
    },
    get deferred() {
      return deferred.length;
    },
  };
}

async function job(b: ReturnType<typeof bench>, name: string, steps: number, ms: number) {
  const { signal } = new AbortController();
  for (let step = 0; step < steps; step++) {
    await b.turns.pause(signal);
    b.spend(ms, `${name}${step}`);
  }
}

describe("turns", () => {
  it("let a job go on until its slice is over", async () => {
    const b = bench();
    const done = job(b, "a", 5, 3);
    expect(b.log).toEqual([]);
    await b.next();
    expect(b.log).toEqual(["a0", "a1", "a2"]);
    await b.next();
    expect(b.log).toEqual(["a0", "a1", "a2", "a3", "a4"]);
    await done;
    expect(await b.next()).toBe(0);
  });

  it("put all pages before the next slice", async () => {
    const b = bench();
    const done = job(b, "a", 2, 9);
    await b.next();
    b.due(2);
    await b.next();
    await b.next();
    await b.next();
    expect(b.log).toEqual(["a0", "page", "page", "a1"]);
    await done;
  });

  it("go to and fro between jobs", async () => {
    const b = bench();
    const done = Promise.all([job(b, "a", 3, 9), job(b, "b", 3, 9)]);
    while ((await b.next()) > 0) {}
    expect(b.log).toEqual(["a0", "b0", "a1", "b1", "a2", "b2"]);
    await done;
  });

  it("are not taken while there is nothing to do", async () => {
    const b = bench();
    b.due(1);
    expect(b.deferred).toBe(1);
    b.due(0);
    expect(b.deferred).toBe(1);
    await b.next();
    expect(b.deferred).toBe(0);
  });

  it("end a job that was given up", async () => {
    const b = bench();
    const controller = new AbortController();
    const reason = new Error("given up");
    const pausing = b.turns.pause(controller.signal);
    controller.abort(reason);
    await b.next();
    await expect(pausing).rejects.toBe(reason);
    await expect(b.turns.pause(controller.signal)).rejects.toBe(reason);
  });
});
