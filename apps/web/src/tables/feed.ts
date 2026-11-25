import type { Table } from "@reticula/protocol/table";
import { type QueuedUpload, queuedUpload } from "./delete.ts";
import { TableError, type TableFailure } from "./failure.ts";
import type { TableEngine } from "./host.ts";
import { type Mirror, openMirror } from "./mirror.ts";
import { Later, type RemoteTarget, send, tableUrl } from "./remote-api.ts";

/** Requests at once when all of a table is waited for. */
const MOST_REQUESTS = 4;
/** The longest pause before the server is asked again. */
const MOST_PAUSE_MS = 10_000;
/** How often the server is asked in vain before it is said that it cannot be had. */
const MOST_FAILURES = 3;
/** What a table on the server may take in memory. */
const MOST_IN_MEMORY = 512 << 20;
/** How often it is looked whether a table has been uploaded. */
const LOOK_MS = 500;

export interface FeedOptions {
  readonly target: RemoteTarget;
  readonly id: string;
  readonly bytes: number;
  readonly table: Pick<Table, "plan" | "take" | "have" | "ended" | "fetch" | "leaveLog">;
  readonly steps: TableEngine["TableStep"];
  /** Something has come, or it turned out that nothing will. */
  readonly onChange: () => void;
  /** The engine is broken. */
  readonly onCrash: (error: unknown) => void;
}

/** Has a table come from the server: what the engine waits for, and then the rest. */
export class Feed {
  readonly #options: FeedOptions;
  readonly #requests = new Map<number, AbortController>();
  #mirror: Mirror | null = null;
  /** What it is on the server. Nothing comes as long as that is not known. */
  #etag = "";
  #uploaded = "";
  /** Whether copies may be kept. */
  #isKept: boolean;
  /** Whether to have all of it come, little by little. */
  #fill = false;
  /** How many wait for all of it. */
  #hurry = 0;
  #waiting: (() => void)[] = [];
  #timer: ReturnType<typeof setTimeout> | undefined;
  #failures = 0;
  #isClosed = false;
  /** Why nothing more will come. */
  failure: TableFailure | null = null;

  constructor(options: FeedOptions) {
    this.#options = options;
    this.#isKept = options.target.canWrite;
  }

  /** Has it come, as the file with that digest. */
  async start(sha256: string): Promise<void> {
    const { target, bytes, table } = this.#options;
    const mirror = this.#isKept ? await openMirror(target.userId, sha256, bytes) : null;
    if (this.#isClosed) {
      mirror?.file.close();
      return;
    }
    if (!mirror && bytes > MOST_IN_MEMORY) {
      throw new TableError("limit");
    }
    this.#mirror = mirror;
    this.#etag = `"${sha256}"`;
    this.#fill = target.userId !== "" && !savesData();
    const end = table.fetch(mirror?.file, mirror?.file.size ?? 0);
    mirror?.file.restart(end);
    this.wake();
    this.ask();
  }

  /** Reads the file that is uploaded as long as it is here, and has it come then. */
  async follow(upload: QueuedUpload): Promise<void> {
    this.#options.table.fetch(undefined, 0);
    let { sha256 } = upload;
    for (
      let queued: QueuedUpload | undefined = upload;
      queued && !this.#uploaded;
      queued = await queuedUpload(upload.entry.id)
    ) {
      sha256 = queued.sha256 || sha256;
      await new Promise((resolve) => setTimeout(resolve, LOOK_MS));
      if (this.#isClosed) {
        return;
      }
    }
    await this.start(this.#uploaded || sha256).catch((error: unknown) => this.#trouble(error));
  }

  /** What is followed has been uploaded, as the file with that digest. */
  uploaded(sha256: string): void {
    this.#uploaded = sha256;
  }

  get bytes(): number {
    return this.#options.bytes;
  }

  /** Bytes that are here. */
  get have(): number {
    return this.#options.table.have();
  }

  get isWhole(): boolean {
    return this.have === this.bytes;
  }

  /** Whether the server could not be had when it was asked last. */
  get isStalled(): boolean {
    return this.#failures > 0;
  }

  /** Whether it could not be had for a while. */
  get isLost(): boolean {
    return this.#failures >= MOST_FAILURES;
  }

  /** Has what the engine waits for by now come. */
  ask(ms = 0): void {
    // Not during a pause, which is there for the server to recover.
    if (this.#etag && this.#timer === undefined && !this.#isClosed && !this.failure) {
      this.#timer = setTimeout(() => {
        this.#timer = undefined;
        const plan = this.#options.table.plan(this.#hurry ? MOST_REQUESTS : Number(this.#fill));
        for (let at = 0; at < plan.length; at += 3) {
          const [block, from, until] = plan.subarray(at) as unknown as [number, number, number];
          if (from === until) {
            this.#requests.get(block)?.abort();
            this.#requests.delete(block);
          } else {
            void this.#request(block, from, until);
          }
        }
      }, ms);
    }
  }

  /** Waits until something has come. */
  async next(signal: AbortSignal): Promise<void> {
    this.ask();
    const { promise, resolve, reject } = Promise.withResolvers<void>();
    const give = () => reject(signal.reason);
    signal.addEventListener("abort", give);
    this.#waiting.push(resolve);
    try {
      signal.throwIfAborted();
      await promise;
    } finally {
      signal.removeEventListener("abort", give);
    }
    if (this.failure) {
      throw new TableError(this.failure);
    }
  }

  /** Has all of it come, and waits until it has. */
  async whole(signal: AbortSignal, onProgress: () => void): Promise<void> {
    this.#hurry++;
    try {
      while (!this.isWhole) {
        onProgress();
        await this.next(signal);
      }
    } finally {
      this.#hurry--;
      this.ask();
    }
  }

  /** Keeps nothing on the disk any more, and has no more come than is waited for. */
  async leaveDisk(): Promise<void> {
    this.#isKept = false;
    this.#fill = false;
    const mirror = this.#mirror;
    this.#mirror = null;
    if (mirror) {
      this.#options.table.leaveLog();
      if (!this.failure) {
        this.wake();
      }
      await mirror.remove();
    }
  }

  close(): void {
    this.#isClosed = true;
    clearTimeout(this.#timer);
    for (const controller of this.#requests.values()) {
      controller.abort();
    }
    this.#mirror?.file.close();
  }

  async #request(block: number, from: number, until: number): Promise<void> {
    const { target, id, bytes, table, steps } = this.#options;
    const etag = this.#etag;
    const controller = new AbortController();
    this.#requests.set(block, controller);
    const { signal } = controller;
    try {
      const url = `${tableUrl(target.docId, id)}/data`;
      const { status, headers, body } = await send(target, url, {
        signal,
        headers: { Range: `bytes=${from}-${until - 1}`, "If-Range": etag },
      });
      const range = headers.get("Content-Range");
      const isPart = range === `bytes ${from}-${until - 1}/${bytes}`;
      if (headers.get("ETag") !== etag || !body || (status !== 200 && !isPart)) {
        throw new TableError({ code: "changed", detail: `${status} ${range}` });
      }
      // All of the file comes if something between here and the server does not
      // know ranges.
      let at = isPart ? from : 0;
      const reader = body.getReader();
      while (at < until) {
        const { value } = await reader.read();
        if (signal.aborted) {
          return;
        }
        if (!value) {
          throw new Later();
        }
        this.#failures = 0;
        const had = this.have;
        const taken = table.take(at, value);
        at += value.length;
        if (taken === steps.Failed) {
          throw new TableError("memory");
        }
        if (taken === steps.Restart) {
          // There is no room for a copy, which is not worth that of a workbook.
          void this.leaveDisk();
        }
        if (this.have !== had) {
          this.wake();
        }
      }
      void reader.cancel();
      this.#requests.delete(block);
      table.ended(block);
      this.ask();
    } catch (error) {
      if (error instanceof WebAssembly.RuntimeError) {
        this.#options.onCrash(error);
      } else if (!signal.aborted) {
        this.#trouble(error);
      }
    }
  }

  #trouble(error: unknown): void {
    // The others would fail as well. What has come of them is kept.
    for (const [block, controller] of this.#requests) {
      controller.abort();
      this.#options.table.ended(block);
    }
    this.#requests.clear();
    if (error instanceof TableError) {
      this.failure = error.failure;
      if (error.code === "denied" || error.code === "notFound") {
        void this.leaveDisk();
      }
    } else {
      const { afterMs = null } = error instanceof Later ? error : {};
      clearTimeout(this.#timer);
      this.#timer = undefined;
      this.ask(afterMs ?? Math.min(500 * 2 ** this.#failures, MOST_PAUSE_MS));
      this.#failures++;
    }
    this.wake();
  }

  /** Has whoever waits look again. */
  wake(): void {
    for (const resolve of this.#waiting.splice(0)) {
      resolve();
    }
    this.#options.onChange();
  }
}

/** Whether less is to be loaded than could be. */
function savesData(): boolean {
  const { connection } = navigator as { connection?: { readonly saveData?: boolean } };
  return connection?.saveData === true;
}
