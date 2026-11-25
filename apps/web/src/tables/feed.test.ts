import type { TableStep } from "@reticula/protocol/table";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { dequeue, enqueue, type QueuedUpload } from "./delete.ts";
import { FakeDirectory } from "./fake-storage.ts";
import { Feed, type FeedOptions } from "./feed.ts";
import type { TableEngine } from "./host.ts";
import type { Mirror } from "./mirror.ts";

/** The copy there is, or may be one. */
const copies = vi.hoisted(() => ({ opened: [] as unknown[][], next: null as unknown }));
vi.mock("./mirror.ts", () => ({
  openMirror: async (...ask: unknown[]) => {
    copies.opened.push(ask);
    return copies.next;
  },
}));

const BYTES = 1_000;
const TAG = '"5a"';
const Steps = { Working: 0, Done: 1, NeedInput: 2, Need: 3, Restart: 4, Failed: 5 };

/** A request that was made, and how it is answered. */
class Asked {
  readonly headers: Headers;
  readonly signal: AbortSignal;
  readonly from: number;
  readonly until: number;
  readonly #resolve: (response: Response) => void;
  readonly #reject: (error: unknown) => void;
  #body: ReadableStreamDefaultController<Uint8Array> | undefined;

  constructor(
    init: RequestInit,
    resolve: (response: Response) => void,
    reject: (error: unknown) => void,
  ) {
    this.headers = new Headers(init.headers);
    this.signal = init.signal as AbortSignal;
    this.#resolve = resolve;
    this.#reject = reject;
    const [, from, last] = /^bytes=(\d+)-(\d+)$/.exec(this.headers.get("Range") ?? "") ?? [];
    this.from = Number(from);
    this.until = Number(last) + 1;
    this.signal.addEventListener("abort", () => this.fail());
  }

  fail(): void {
    this.#reject(new TypeError("failed to fetch"));
    try {
      this.#body?.error(new TypeError("network error"));
    } catch {
      // It had ended.
    }
  }

  refuse(status: number, headers: Record<string, string> = {}): void {
    this.#resolve(new Response("no", { status, headers }));
  }

  begin(headers: Record<string, string> = {}, status = 206): void {
    const body = new ReadableStream<Uint8Array>({
      start: (controller) => {
        this.#body = controller;
      },
    });
    const range = `bytes ${this.from}-${this.until - 1}/${BYTES}`;
    const part = status === 206 ? { "Content-Range": range } : {};
    this.#resolve(new Response(body, { status, headers: { ETag: TAG, ...part, ...headers } }));
  }

  send(count: number): void {
    this.#body?.enqueue(new Uint8Array(count));
  }

  end(): void {
    this.#body?.close();
  }
}

let root: FakeDirectory;
let asked: Asked[];
let plans: number[][];
let feed: Feed;
let taken: [number, number][];
let ended: number[];
let wanted: number[];
let left: number;
/** What the table was told of where it comes from. */
let fetched: unknown[][];
let answer: TableStep;
let changes: number;
let crashes: unknown[];

/** What is due has been done. */
async function settle(ms = 5): Promise<void> {
  await vi.advanceTimersByTimeAsync(ms);
}

interface Opened extends Partial<FeedOptions> {
  readonly fill?: boolean;
  readonly mirror?: Mirror;
  readonly upload?: QueuedUpload;
}

function open({ fill = false, mirror, upload, ...options }: Opened = {}): Feed {
  vi.stubGlobal("navigator", {
    connection: { saveData: !fill },
    storage: { getDirectory: async () => root },
  });
  copies.next = mirror ?? null;
  feed = new Feed({
    target: { docId: "doc", userId: "user", canWrite: true },
    id: "table",
    bytes: BYTES,
    table: {
      plan: (requests) => {
        wanted.push(requests);
        return Float64Array.from(plans.shift() ?? []);
      },
      take: (at, bytes) => {
        taken.push([at, bytes.length]);
        return answer;
      },
      have: () => taken.reduce((sum, [, length]) => sum + length, 0),
      ended: (block) => ended.push(block),
      leaveLog: () => left++,
      fetch: (...from) => {
        fetched.push(from);
        return 70;
      },
    },
    steps: Steps as unknown as TableEngine["TableStep"],
    onChange: () => {
      changes++;
    },
    onCrash: (error) => crashes.push(error),
    ...options,
  });
  (upload ? feed.follow(upload) : feed.start("5a")).catch(() => {});
  return feed;
}

function last(): Asked {
  const found = asked.at(-1);
  if (!found) {
    throw new Error("nothing was asked");
  }
  return found;
}

/** Waits for something that comes, or for why it does not. */
const outcome = (signal = new AbortController().signal) =>
  feed.next(signal).then(
    () => "came",
    (error: { code?: string }) => error.code ?? error,
  );

const mirror = () => {
  const file = { close: vi.fn(), restart: vi.fn(), size: 90 };
  return { file: file as unknown as Mirror["file"], remove: vi.fn(async () => true) };
};

beforeEach(() => {
  vi.useFakeTimers();
  asked = [];
  plans = [];
  taken = [];
  ended = [];
  wanted = [];
  left = 0;
  fetched = [];
  copies.opened = [];
  root = new FakeDirectory();
  FakeDirectory.room = Number.POSITIVE_INFINITY;
  vi.stubGlobal("navigator", { storage: { getDirectory: async () => root } });
  answer = Steps.Working as TableStep;
  changes = 0;
  crashes = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(
      (_: string, init: RequestInit) =>
        new Promise<Response>((resolve, reject) => asked.push(new Asked(init, resolve, reject))),
    ),
  );
});

afterEach(() => {
  feed.close();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("a table that comes from the server", () => {
  it("is asked for as the engine plans, as it was listed", async () => {
    plans = [[3, 300, 400, 7, 700, 900]];
    open();
    await settle();
    expect(fetch).toHaveBeenCalledWith(
      "/api/workbooks/doc/tables/table/data",
      expect.objectContaining({ credentials: "same-origin", redirect: "error" }),
    );
    expect(asked.map(({ from, until }) => [from, until])).toEqual([
      [300, 400],
      [700, 900],
    ]);
    expect(Object.fromEntries(last().headers)).toEqual({ range: "bytes=700-899", "if-range": TAG });
    const came = outcome();
    asked[0]?.begin();
    asked[0]?.send(60);
    await settle();
    expect(await came).toBe("came");
    asked[0]?.send(40);
    await settle();
    expect(taken).toEqual([
      [300, 60],
      [360, 40],
    ]);
    expect(ended).toEqual([3]);
    expect(feed.have).toBe(100);
    // What else there is to ask for then.
    expect(wanted).toEqual([0, 0, 0]);
  });

  it("is had with the link that was given", async () => {
    plans = [[0, 0, 10]];
    open({ target: { docId: "doc", userId: "", shareToken: "token", canWrite: false } });
    await settle();
    expect(last().headers.get("X-Reticula-Share")).toBe("token");
  });

  it("comes little by little, unless all of it is waited for", async () => {
    open({ fill: true });
    await settle();
    expect(wanted).toEqual([1]);
    const controller = new AbortController();
    const progress: number[] = [];
    const whole = feed.whole(controller.signal, () => progress.push(feed.have));
    plans = [[0, 0, BYTES]];
    await settle();
    expect(wanted).toEqual([1, 4]);
    last().begin();
    last().send(600);
    await settle();
    last().send(400);
    await settle();
    await whole;
    expect(progress).toEqual([0, 600]);
    expect(feed.isWhole).toBe(true);
    await settle();
    expect(wanted.at(-1)).toBe(1);
  });

  it("does not come faster any more once nobody waits", async () => {
    open();
    const controller = new AbortController();
    const whole = feed.whole(controller.signal, () => {}).catch((error: unknown) => error);
    await settle();
    expect(wanted.at(-1)).toBe(4);
    controller.abort("enough");
    expect(await whole).toBe("enough");
    await settle();
    expect(wanted.at(-1)).toBe(0);
  });

  it("is given up where the engine says so", async () => {
    plans = [
      [3, 300, 400],
      [3, 0, 0, 5, 500, 600],
    ];
    open();
    await settle();
    const first = last();
    first.begin();
    first.send(10);
    await settle();
    feed.ask();
    await settle();
    expect(first.signal.aborted).toBe(true);
    expect(last().from).toBe(500);
    // The engine knows, and it is no failure.
    expect(ended).toEqual([]);
    expect(feed.isStalled).toBe(false);
  });

  it("is closed with all that is under way", async () => {
    plans = [[3, 300, 400]];
    open();
    await settle();
    feed.close();
    expect(last().signal.aborted).toBe(true);
    feed.ask();
    await settle(60_000);
    expect(asked).toHaveLength(1);
    expect(changes).toBe(1);
  });
});

describe("what the server answers", () => {
  const failing = [
    ["another file", (one: Asked) => one.begin({ ETag: '"other"' }), "changed"],
    ["another range", (one: Asked) => one.begin({ "Content-Range": "bytes 1-2/3" }), "changed"],
    ["a file that is now another", (one: Asked) => one.begin({ ETag: '"b"' }, 200), "changed"],
    ["that there is no such range", (one: Asked) => one.refuse(416), "changed"],
    ["that nobody is signed in", (one: Asked) => one.refuse(401), "denied"],
    ["that it may not be had", (one: Asked) => one.refuse(403), "denied"],
    ["that there is no such table", (one: Asked) => one.refuse(404), "notFound"],
    ["something else", (one: Asked) => one.refuse(400), "refused"],
  ] as const;

  it.each(failing)("is %s", async (_, refuse, code) => {
    plans = [[0, 0, 10, 9, 90, 100]];
    const copy = mirror();
    open({ mirror: copy, fill: true });
    await settle();
    const came = outcome();
    refuse(asked[0] as Asked);
    await settle();
    expect(await came).toBe(code);
    expect(feed.failure?.code).toBe(code);
    expect(changes).toBe(2);
    expect(asked[1]?.signal.aborted).toBe(true);
    // There is no copy of what may not be had any more.
    const isLost = code === "denied" || code === "notFound";
    expect(copy.remove).toHaveBeenCalledTimes(isLost ? 1 : 0);
    expect(left).toBe(isLost ? 1 : 0);
    plans = [[5, 50, 60]];
    feed.ask();
    await settle(60_000);
    expect(asked).toHaveLength(2);
  });

  it("can be all of the file, of which no more is taken than was asked for", async () => {
    plans = [[5, 500, 600]];
    open();
    await settle();
    last().begin({}, 200);
    last().send(400);
    last().send(150);
    await settle();
    expect(ended).toEqual([]);
    last().send(200);
    await settle();
    expect(taken).toEqual([
      [0, 400],
      [400, 150],
      [550, 200],
    ]);
    expect(ended).toEqual([5]);
    expect(feed.failure).toBeNull();
  });

  it("is waited for when the server has no time, as long as it says", async () => {
    plans = [
      [0, 0, 10],
      [0, 0, 10],
    ];
    open();
    await settle();
    last().refuse(429, { "Retry-After": "7" });
    await settle(6_900);
    expect(feed.isStalled).toBe(true);
    expect(ended).toEqual([0]);
    feed.ask();
    await settle();
    expect(asked).toHaveLength(1);
    await settle(200);
    expect(asked).toHaveLength(2);
    last().begin();
    last().send(10);
    await settle();
    expect(feed.isStalled).toBe(false);
  });

  it("is asked for less and less often while there is none", async () => {
    plans = Array.from({ length: 8 }, () => [0, 0, 10]);
    open();
    const times: number[] = [];
    const began = Date.now();
    for (let tries = 0; tries < 8; tries++) {
      const count = asked.length;
      while (asked.length === count) {
        await settle(50);
      }
      times.push(Math.round((Date.now() - began) / 500) / 2);
      last()[tries % 2 ? "fail" : "refuse"](503);
    }
    expect(times).toEqual([0, 0.5, 1.5, 3.5, 7.5, 15.5, 25.5, 35.5]);
  });

  it("can end too early, after which the engine is asked anew", async () => {
    plans = [
      [0, 0, 100, 5, 500, 600],
      [0, 40, 100],
    ];
    open();
    await settle();
    asked[0]?.begin();
    asked[0]?.send(40);
    asked[0]?.end();
    await settle(600);
    // The other one would have failed as well.
    expect(asked[1]?.signal.aborted).toBe(true);
    expect(ended).toEqual([0, 5]);
    expect(last().from).toBe(40);
  });
});

describe("where a table is kept", () => {
  it("is a copy as far as it goes, for those who may have one", async () => {
    const copy = mirror();
    open({ mirror: copy });
    await settle();
    expect(copies.opened).toEqual([["user", "5a", BYTES]]);
    expect(fetched).toEqual([[copy.file, 90]]);
    expect(copy.file.restart).toHaveBeenCalledWith(70);
    copies.opened = [];
    open({ target: { docId: "doc", userId: "user", canWrite: false }, mirror: copy });
    await settle();
    expect(copies.opened).toEqual([]);
    expect(fetched.at(-1)).toEqual([undefined, 0]);
  });

  it("is not a copy that was opened too late", async () => {
    const copy = mirror();
    open({ mirror: copy });
    feed.close();
    await settle();
    expect(copy.file.close).toHaveBeenCalledOnce();
    expect(fetched).toEqual([]);
  });

  it("cannot be the memory when it is too large for that", async () => {
    open({ bytes: 600 << 20 });
    await expect(feed.start("5a")).rejects.toMatchObject({ code: "limit" });
  });

  it("is waited for anew by whoever waits for what was in the copy", async () => {
    const copy = mirror();
    open({ mirror: copy });
    await settle();
    const came = outcome();
    const before = changes;
    await feed.leaveDisk();
    expect(await came).toBe("came");
    expect(changes).toBe(before + 1);
    expect(copy.remove).toHaveBeenCalledOnce();
    await feed.leaveDisk();
    expect(copy.remove).toHaveBeenCalledOnce();
  });

  it("is the memory once there is no room for a copy, and no more comes than is waited for", async () => {
    plans = [[0, 0, 100]];
    const copy = mirror();
    open({ mirror: copy, fill: true });
    await settle();
    answer = Steps.Restart as TableStep;
    last().begin();
    last().send(100);
    await settle();
    expect(copy.remove).toHaveBeenCalledOnce();
    expect(feed.failure).toBeNull();
    expect(wanted.at(-1)).toBe(0);
  });

  it("is nowhere without memory", async () => {
    plans = [[0, 0, 100]];
    open();
    await settle();
    answer = Steps.Failed as TableStep;
    last().begin();
    last().send(100);
    await settle();
    expect(feed.failure?.code).toBe("memory");
  });

  it("is of no use when the engine breaks", async () => {
    plans = [[0, 0, 100]];
    const trap = new WebAssembly.RuntimeError("unreachable");
    const table = {
      plan: () => Float64Array.from(plans.shift() ?? []),
      take: () => {
        throw trap;
      },
      have: () => 0,
      ended: () => {},
      leaveLog: () => {},
      fetch: () => 0,
    };
    open({ table });
    await settle();
    last().begin();
    last().send(100);
    await settle();
    expect(crashes).toEqual([trap]);
  });
});

describe("a table that is being uploaded", () => {
  const queued = (sha256: string): QueuedUpload => ({
    userId: "user",
    docId: "doc",
    entry: { id: "table", name: "t", rows: 1, cols: 1, bytes: BYTES },
    sha256,
    queuedAt: 1,
    from: null,
  });

  it("is not asked for, and then comes as what was uploaded, and out of the copy of that", async () => {
    await enqueue(queued(""));
    plans = [[0, 0, 100]];
    const copy = mirror();
    open({ upload: queued(""), mirror: copy, fill: true });
    const came = outcome();
    await settle(20_000);
    // Its digest is worked out later.
    await enqueue(queued("77"));
    await settle(20_000);
    expect(asked).toHaveLength(0);
    expect(copies.opened).toEqual([]);
    expect(fetched).toEqual([[undefined, 0]]);
    await dequeue("table");
    await settle(1_000);
    expect(await came).toBe("came");
    expect(copies.opened).toEqual([["user", "77", BYTES]]);
    expect(fetched).toEqual([
      [undefined, 0],
      [copy.file, 90],
    ]);
    expect(last().headers.get("If-Range")).toBe('"77"');
    expect(wanted).toEqual([1]);
  });

  it("comes as what it was told to be, if it was uploaded before its digest was seen", async () => {
    await enqueue(queued(""));
    plans = [[0, 0, 100]];
    open({ upload: queued("") });
    feed.uploaded("78");
    await dequeue("table");
    await settle(1_000);
    expect(last().headers.get("If-Range")).toBe('"78"');
  });

  it("is not had when it was given up", async () => {
    await enqueue(queued(""));
    plans = [[0, 0, 100]];
    open({ upload: queued("") });
    await dequeue("table");
    await settle(1_000);
    const came = outcome();
    last().refuse(404);
    await settle();
    expect(await came).toBe("notFound");
  });

  it("is no longer looked for once it is closed", async () => {
    await enqueue(queued(""));
    open({ upload: queued("") });
    feed.close();
    await dequeue("table");
    await settle(5_000);
    expect(fetched).toHaveLength(1);
  });
});
