import type { TableRecord, TableUpload } from "@reticula/protocol/collab-api";
import { tablesDir } from "./catalog.ts";
import {
  dequeue,
  type QueuedUpload,
  queueDir,
  queuedUpload,
  queuedUploads,
  requeue,
  sweepQueue,
  TABLE_EXTENSION,
  UPLOADED,
} from "./delete.ts";
import { TableError, type TableFailure } from "./failure.ts";
import { fileFailure } from "./files.ts";
import type { TableEngine } from "./host.ts";
import { openMirror } from "./mirror.ts";
import { JSON_TYPE, Later, send, tableUrl, whole } from "./remote-api.ts";

/** The most the server takes at a time. */
const PIECE_BYTES = 8 << 20;
/** How much of a file is read at a time. */
const READ_BYTES = 1 << 20;
/** The shortest and the longest pause before the server is asked again. */
const LEAST_PAUSE_MS = 1_000;
const MOST_PAUSE_MS = 30_000;
/** How often it is looked whether there is more to upload. */
const LOOK_MS = 1_000;
/** The most characters of a name that the server takes. */
const MOST_NAME = 120;

/** How far an upload is. */
export interface UploadProgress {
  readonly docId: string;
  readonly id: string;
  readonly name: string;
  /** `waiting`: the server is not there, or has no time for it. */
  readonly state: "sending" | "waiting" | "done" | "failed";
  /** Bytes that have arrived, and those there are. */
  readonly sent: number;
  readonly total: number;
  readonly failure?: TableFailure | undefined;
}

/** What is to be uploaded. */
export async function sourceOf({ entry, from }: QueuedUpload): Promise<File> {
  const dir = await (from ? tablesDir(from.docId) : queueDir());
  return (await dir.getFileHandle((from ?? entry).id + TABLE_EXTENSION)).getFile();
}

/** An upload that was tried. */
interface Tried {
  /** When to go on with it. Never with what the server does not take. */
  readonly dueAt: number;
  /** How long it is waited for next. */
  readonly pauseMs: number;
  /** Bytes that have arrived. */
  readonly sent: number;
}

/** Uploads what is in the queue. Only one of these may be at work in a browser. */
export class Uploader {
  readonly #engine: TableEngine;
  readonly #report: (progress: UploadProgress) => void;
  readonly #channel: BroadcastChannel | null;
  readonly #tried = new Map<string, Tried>();
  #isAfresh = false;
  #running: Promise<void> | null = null;

  /** What is uploaded is told of on `channel`. */
  constructor(
    engine: TableEngine,
    report: (progress: UploadProgress) => void,
    channel: BroadcastChannel | null = null,
  ) {
    this.#engine = engine;
    this.#report = report;
    this.#channel = channel;
  }

  /**
   * Uploads all that somebody has in the queue, also what comes meanwhile;
   * `afresh`: also what could not be. Ends when somebody else is signed in.
   */
  run(userId: string, afresh: boolean, signal: AbortSignal): Promise<void> {
    if (afresh) {
      this.#tried.clear();
      this.#isAfresh = true;
    }
    this.#running ??= this.#run(userId, signal).finally(() => {
      this.#running = null;
    });
    return this.#running;
  }

  async #run(userId: string, signal: AbortSignal): Promise<void> {
    await sweepQueue();
    for (;;) {
      const [next] = (await queuedUploads())
        .filter((upload) => upload.userId === userId && this.#of(upload).dueAt < Infinity)
        .sort((a, b) => this.#of(a).dueAt - this.#of(b).dueAt);
      if (!next) {
        return;
      }
      try {
        const waitMs = this.#of(next).dueAt - Date.now();
        if (waitMs > 0) {
          // What comes meanwhile need not wait.
          await new Promise((resolve) => setTimeout(resolve, Math.min(waitMs, LOOK_MS)));
          signal.throwIfAborted();
          continue;
        }
        await this.#upload(next, signal);
        this.#tell(next, "done");
      } catch (error) {
        const failure = fileFailure(error);
        if (signal.aborted || failure.code === "closed") {
          // Nothing more is done here for now.
          this.#tell(next, "failed", { code: "cancelled" });
          signal.throwIfAborted();
          return;
        }
        if (error instanceof Later) {
          const { pauseMs } = this.#of(next);
          this.#try(next, {
            dueAt: Date.now() + (error.afterMs ?? pauseMs),
            pauseMs: Math.min(2 * pauseMs, MOST_PAUSE_MS),
          });
          this.#tell(next, "waiting");
          continue;
        }
        if (failure.code === "cancelled") {
          // The server may have come to know of it meanwhile.
          const url = tableUrl(next.docId, next.entry.id);
          await send({}, url, { method: "DELETE", signal }).catch(() => {});
        }
        this.#try(next, { dueAt: Infinity });
        this.#tell(next, "failed", failure);
      }
    }
  }

  #of({ entry }: QueuedUpload): Tried {
    return this.#tried.get(entry.id) ?? { dueAt: 0, pauseMs: LEAST_PAUSE_MS, sent: 0 };
  }

  #try(upload: QueuedUpload, change: Partial<Tried>): void {
    this.#tried.set(upload.entry.id, { ...this.#of(upload), ...change });
  }

  #tell(upload: QueuedUpload, state: UploadProgress["state"], failure?: TableFailure): void {
    const { id, name, bytes: total } = upload.entry;
    const { sent } = this.#of(upload);
    this.#report({ docId: upload.docId, id, name, state, sent, total, failure });
  }

  async #upload(upload: QueuedUpload, signal: AbortSignal): Promise<void> {
    const { userId, entry } = upload;
    const file = await sourceOf(upload);
    let { sha256 } = upload;
    if (sha256 === "") {
      sha256 = await this.#digest(file, signal);
      await requeue(entry.id, (queued) => ({ ...queued, sha256 }));
    }
    await this.#transfer(upload, sha256, file, signal).catch((error: TableError) => {
      // What has arrived was thrown away.
      if (error.code !== "corrupt") {
        throw error;
      }
      return this.#transfer(upload, sha256, file, signal);
    });
    await this.#check(upload, signal);
    await this.#keep(userId, sha256, file);
    await dequeue(entry.id);
    this.#channel?.postMessage({ kind: UPLOADED, id: entry.id, sha256 });
  }

  /** Throws when an upload was given up, or is not of whoever is signed in. */
  async #check({ entry, userId }: QueuedUpload, signal: AbortSignal): Promise<void> {
    if (!(await queuedUpload(entry.id))) {
      throw new TableError("cancelled");
    }
    const answer = await send({}, "/api/auth/me", { signal }, [401]);
    if ((await whole(answer.json())).id !== userId) {
      throw new TableError("closed");
    }
  }

  async #digest(file: File, signal: AbortSignal): Promise<string> {
    const digest = this.#engine.TableWork.digest();
    try {
      for (let at = 0; at < file.size; at += READ_BYTES) {
        digest.feed(await read(file, at));
        signal.throwIfAborted();
      }
      return digest.report();
    } finally {
      digest.free();
    }
  }

  async #transfer(
    upload: QueuedUpload,
    sha256: string,
    file: File,
    signal: AbortSignal,
  ): Promise<void> {
    const { docId, entry, isDeclared } = upload;
    const url = tableUrl(docId, entry.id);
    const ask = async (
      method: string,
      to: string,
      headers: HeadersInit = {},
      body: BodyInit | null = null,
    ): Promise<Partial<TableRecord & TableUpload>> => {
      await this.#check(upload, signal);
      const init = { method, headers, body, signal };
      const answer = await whole((await send({}, to, init, [409])).json());
      // It is listened to again.
      this.#try(upload, { pauseMs: LEAST_PAUSE_MS });
      return answer;
    };
    const complete = () => ask("POST", `${url}/complete`);
    let answer: Partial<TableRecord & TableUpload>;
    if (isDeclared && (this.#tried.has(entry.id) || !this.#isAfresh)) {
      // Not declared again: somebody may have deleted it.
      answer = await complete();
    } else {
      const name =
        // A character may take two of what a text is counted in.
        [...entry.name.replace(/[\p{Cc}\p{Cs}]/gu, " ").trim()]
          .slice(0, MOST_NAME)
          .join("")
          .trim() || "Table";
      const table = { id: entry.id, name, bytes: file.size, sha256 };
      const tables = url.slice(0, url.lastIndexOf("/"));
      answer = await ask("POST", tables, JSON_TYPE, JSON.stringify(table));
      await requeue(entry.id, (queued) => ({ ...queued, isDeclared: true }));
    }
    while (answer.status !== "ready") {
      const { uploadOffset: sent } = answer;
      if (typeof sent !== "number") {
        throw new TableError("refused");
      }
      this.#try(upload, { sent });
      this.#tell(upload, "sending");
      // It says where the upload goes on, be it there or elsewhere.
      answer =
        sent < file.size
          ? await ask(
              "PATCH",
              `${url}/data`,
              { "Upload-Offset": `${sent}` },
              // Read here, where it is known that it is the file that cannot be.
              await read(file, sent, PIECE_BYTES),
            )
          : await complete();
    }
  }

  /** Has a file that was uploaded be among the copies, from where it need not be had again. */
  async #keep(userId: string, sha256: string, file: File): Promise<void> {
    const mirror = await openMirror(userId, sha256, file.size);
    if (mirror) {
      const { Table, TableStep } = this.#engine;
      const table = new Table(undefined, file.size);
      mirror.file.restart(table.fetch(mirror.file, mirror.file.size));
      for (let at = 0; at < file.size; at += READ_BYTES) {
        if (table.take(at, await read(file, at)) !== TableStep.Working) {
          await mirror.remove();
          break;
        }
      }
      table.free();
      mirror.file.close();
    }
  }
}

async function read(file: File, at: number, bytes = READ_BYTES): Promise<Uint8Array<ArrayBuffer>> {
  return new Uint8Array(await file.slice(at, at + bytes).arrayBuffer());
}
