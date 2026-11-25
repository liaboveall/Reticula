import {
  ApiError,
  listTables,
  me,
  type Role,
  type TableRecord,
  workbook,
} from "@reticula/protocol/collab-api";
import {
  copiesDir,
  deleteCopies,
  dequeue,
  lastList,
  type QueuedUpload,
  queuedUploads,
  remove,
  writeText,
} from "../tables/delete.ts";
import type { TableFailure } from "../tables/failure.ts";
import type { RemoteTarget } from "../tables/remote-api.ts";
import type { UploadProgress } from "../tables/uploads.ts";

/** How often it is looked whether somebody else changed the tables of a workbook. */
const REFRESH_MS = 20_000;
/** The longest it is waited until the server is tried again. */
const MOST_WAIT_MS = 300_000;

/** Loads what uploads tables. */
export type LoadUploads = () => Promise<
  Pick<typeof import("../tables/client.ts"), "resumeUploads">
>;

export interface RemoteHandlers {
  readonly loadUploads: LoadUploads;
  /** There are other tables now, or it is known which. */
  onTables(tables: readonly TableRecord[]): void;
  /** Who may do what is not as it was. Without a role nothing may be had any more. */
  onRole?(role: Role | null): void;
  onUpload?(progress: UploadProgress): void;
}

/** Whoever may have one of the tables open. */
export interface TableReader {
  /** Takes over who may do what. */
  retarget(target: RemoteTarget): Promise<void>;
  /** Closes a table if it is the one that is open. */
  lose(id: string, failure: TableFailure): Promise<void>;
}

type TableEdit = (tables: readonly TableRecord[]) => readonly TableRecord[];

/** The tables of a workbook on the server: which there are, and who may do what. */
export class RemoteTables {
  readonly #handlers: RemoteHandlers;
  readonly #refreshMs: number;
  readonly #readers = new Set<TableReader>();
  readonly #stopping = new AbortController();
  /** Changes that the server has yet to be seen to have made. */
  readonly #edits = new Set<TableEdit>();
  #target: RemoteTarget;
  #version: string | undefined;
  /** The tables as the server has them. */
  #listed: readonly TableRecord[] = [];
  #queued: readonly QueuedUpload[] = [];
  #tables: readonly TableRecord[] = [];
  #told = "";
  #failures = 0;
  #refreshing: Promise<void> | null = null;
  #timer: ReturnType<typeof setTimeout> | undefined;

  constructor(target: RemoteTarget, handlers: RemoteHandlers, refreshMs: number = REFRESH_MS) {
    this.#target = target;
    this.#handlers = handlers;
    this.#refreshMs = refreshMs;
  }

  get target(): RemoteTarget {
    return this.#target;
  }

  /** Those that are on the server, and those that are to be. */
  get tables(): readonly TableRecord[] {
    return this.#tables;
  }

  /** Starts with the last list that was had. */
  async start(): Promise<void> {
    const { signal } = this.#stopping;
    document.addEventListener("visibilitychange", this.#onVisible, { signal });
    window.addEventListener("online", this.#onVisible, { signal });
    const { userId, docId, canWrite } = this.#target;
    if (userId !== "") {
      void deleteCopies(userId);
    }
    try {
      const [version, tables] = await lastList(userId, docId);
      this.#listed = tables;
      if (canWrite) {
        this.#version = version;
        this.#tell();
      } else {
        // Copies were kept when they might be.
        await this.#leave(tables);
      }
    } catch {
      // It was never had.
    }
    await this.refresh();
  }

  stop(): void {
    this.#stopping.abort();
    clearTimeout(this.#timer);
  }

  /** Has somebody told of what becomes of the tables. Returns how to stop that. */
  attach(reader: TableReader): () => void {
    this.#readers.add(reader);
    return () => this.#readers.delete(reader);
  }

  /** Looks which tables there are, as when the server can be had again. */
  refresh(): Promise<void> {
    this.#refreshing ??= this.#refresh().finally(() => {
      this.#refreshing = null;
      if (!this.#stopping.signal.aborted && document.visibilityState === "visible") {
        const wait = this.#refreshMs * 2 ** this.#failures;
        this.#timer = setTimeout(() => this.refresh(), Math.min(wait, MOST_WAIT_MS));
      }
    });
    return this.#refreshing;
  }

  readonly #onVisible = (): void => {
    clearTimeout(this.#timer);
    if (document.visibilityState === "visible") {
      void this.refresh();
    }
  };

  async #refresh(): Promise<void> {
    clearTimeout(this.#timer);
    const { docId, userId, shareToken, canWrite } = this.#target;
    const { signal } = this.#stopping;
    const share = shareToken ? { xReticulaShare: shareToken } : {};
    const since = this.#version === undefined ? {} : { since: this.#version };
    try {
      const [list, book] = await Promise.all([
        listTables(docId, signal, { ...share, ...since }),
        // Who may do what has no version.
        userId === "" ? null : workbook(docId, signal, share),
      ]);
      this.#failures = 0;
      if (book && (book.role !== "viewer") !== canWrite) {
        await this.#setRole(book.role);
      }
      if (list.tables) {
        this.#version = list.version;
        await this.#take(list.tables);
      }
    } catch (error) {
      if (signal.aborted) {
        return;
      }
      if (error instanceof ApiError && /40[134]/.test(`${error.status}`)) {
        this.stop();
        await this.#take([], { code: "denied" });
        this.#handlers.onRole?.(null);
        return;
      }
      this.#failures++;
    }
    await this.look().catch(() => {});
  }

  async #setRole(role: Role): Promise<void> {
    this.#target = { ...this.#target, canWrite: role !== "viewer" };
    // Another list for another role.
    this.#version = undefined;
    await Promise.all([...this.#readers].map((reader) => reader.retarget(this.#target)));
    await this.#leave(role === "viewer" ? this.#listed : []);
    this.#handlers.onRole?.(role);
  }

  async #take(tables: readonly TableRecord[], failure: TableFailure = { code: "notFound" }) {
    const gone = this.#listed.filter(({ id }) => !tables.some((table) => table.id === id));
    this.#listed = tables;
    this.#tell();
    // What cannot be had any more is not shown, and there are no copies of it.
    for (const { id } of gone) {
      await Promise.all([...this.#readers].map((reader) => reader.lose(id, failure)));
    }
    await this.#leave(gone);
  }

  /** Deletes the copies of tables. The list is kept for those who may keep copies. */
  async #leave(tables: readonly TableRecord[]): Promise<void> {
    const { userId, docId, canWrite } = this.#target;
    const isKept = canWrite && !this.#stopping.signal.aborted;
    try {
      const dir = await copiesDir(userId, isKept);
      await Promise.all(tables.map(({ sha256 }) => remove(dir, sha256)));
      await (isKept
        ? writeText(dir, `${docId}.json`, JSON.stringify([this.#version, this.#listed]))
        : remove(dir, `${docId}.json`));
    } catch {
      // There are none, or there cannot be any.
    }
  }

  /** Looks what is to be uploaded, and has it uploaded; `afresh`: also what could not be. */
  async look(afresh = false): Promise<void> {
    const { userId, docId, canWrite } = this.#target;
    const queued = (await queuedUploads()).filter((upload) => upload.userId === userId);
    this.#queued = queued.filter((upload) => upload.docId === docId);
    this.#tell();
    if (queued.length > 0 && canWrite) {
      const { resumeUploads } = await this.#handlers.loadUploads();
      void resumeUploads(userId, this.#onUpload, afresh).catch(() => {});
    }
  }

  readonly #onUpload = (progress: UploadProgress): void => {
    this.#handlers.onUpload?.(progress);
    if (progress.state === "done") {
      void this.refresh();
    }
  };

  /** Shows a change until it is seen whether the server has made it. */
  async change(edit: TableEdit, ask: () => Promise<unknown>): Promise<void> {
    this.#edits.add(edit);
    this.#tell();
    try {
      await ask();
      // A list that was asked for before may come without the change.
      await this.#refreshing;
      await this.refresh();
    } finally {
      this.#edits.delete(edit);
      this.#tell();
    }
  }

  /** Takes a table out of the queue. Whether it was in it. */
  async giveUp(id: string): Promise<boolean> {
    const isQueued = this.#queued.some(({ entry }) => entry.id === id);
    if (isQueued) {
      await dequeue(id);
      this.#queued = this.#queued.filter(({ entry }) => entry.id !== id);
    }
    return isQueued;
  }

  #tell(): void {
    const listed = this.#listed;
    const uploading = this.#queued.filter(({ entry }) => !listed.some(({ id }) => id === entry.id));
    this.#tables = [...this.#edits].reduce<readonly TableRecord[]>(
      (tables, edit) => edit(tables),
      [
        ...listed,
        ...uploading.map(
          ({ entry, sha256 }): TableRecord => ({
            ...entry,
            sha256,
            status: "uploading",
            position: 0,
            createdAt: "",
          }),
        ),
      ],
    );
    const now = JSON.stringify(this.#tables);
    if (now !== this.#told) {
      this.#told = now;
      this.#handlers.onTables(this.#tables);
    }
  }
}

/**
 * Has what was being uploaded go on, whatever workbook is open now; `afresh`: also
 * what could not be.
 */
export async function resumeQueued(
  loadUploads: LoadUploads,
  onUpload: (progress: UploadProgress) => void,
  afresh = false,
): Promise<void> {
  if ((await queuedUploads()).length > 0) {
    const { id } = await me();
    const { resumeUploads } = await loadUploads();
    await resumeUploads(id, onUpload, afresh);
  }
}
