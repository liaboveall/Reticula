import type { ParquetInfo, TableQuery, TableSummary, TableValues } from "@reticula/protocol/table";
import { SHEET_AT } from "@reticula/protocol/viewport";
import type { RemoteTables, TableReader } from "../editor/remote-tables.ts";
import { reportLoadFailure } from "../load-failure.ts";
import type { TableEntry } from "./catalog.ts";
import { queuedUploads } from "./delete.ts";
import { TableError, type TableFailure } from "./failure.ts";
import type {
  CellRange,
  CopiedCells,
  CsvSource,
  ImportedTable,
  OpenedTable,
  RemoteProgress,
  SniffedText,
  TableCommand,
  TableMessage,
  TableProgress,
  TableRequestKind,
  TableRequests,
  TableSubscription,
  ViewInfo,
  WorkerName,
} from "./protocol.ts";
import type { RemoteTarget } from "./remote-api.ts";
import { type OpenTables, type TableStatus, tableShown, tablesOpened } from "./shell.ts";
import type { UploadProgress } from "./uploads.ts";

/** How long the worker has to close its files before it is ended. */
const CLOSE_PATIENCE_MS = 2_500;

export interface TableHandlers {
  /** A page of the table that is open. The buffer belongs to the receiver. */
  onPage?(page: ArrayBuffer): void;
  /** There are other tables now, or others in another order. */
  onCatalog?(tables: readonly TableEntry[]): void;
  /** Another tab has the tables, and this client is stopped. With `evicted` they were taken. */
  onYielded?(evicted: boolean): void;
  /**
   * The answer to a question that was given up had been on its way: these are the
   * rows that there are pages of from now on, and they have to be shown.
   */
  onView?(view: ViewInfo, query: TableQuery): void;
  /** How much of the table that is open, which is on the server, is here. */
  onRemote?(progress: RemoteProgress): void;
  onUpload?(progress: UploadProgress): void;
  /** The table that is open cannot be read any further. */
  onTableFailed?(failure: TableFailure): void;
  /** The worker is gone. Another client can be started. */
  onFailed?(failure: TableFailure): void;
}

/** For what takes a while. */
export interface TableTask {
  readonly signal?: AbortSignal | undefined;
  readonly onProgress?: ((progress: TableProgress) => void) | undefined;
}

interface Waiting {
  readonly resolve: (answer: never) => void;
  readonly reject: (error: TableError) => void;
  readonly onProgress: TableTask["onProgress"];
}

/** What a client needs of a worker. */
export type TableWorker = Pick<
  Worker,
  "postMessage" | "terminate" | "onmessage" | "onerror" | "onmessageerror"
>;

/** Whose tables they are. */
export interface TableHome extends WorkerName {
  /** Where they are, unless they are in this browser. */
  readonly remote?: RemoteTables | undefined;
}

function startWorker({ docId, tabId }: WorkerName): TableWorker {
  return new Worker(new URL("./table-worker.ts", import.meta.url), {
    type: "module",
    name: JSON.stringify({ docId, tabId } satisfies WorkerName),
  });
}

/**
 * The tables of a workbook, in a worker of their own. Whatever becomes of it, the
 * engine and the rest of the app go on.
 */
export class TableClient implements OpenTables, TableReader {
  readonly docId: string;
  readonly #remote: RemoteTables | undefined;
  readonly #worker: TableWorker;
  readonly #handlers: TableHandlers;
  readonly #waiting = new Map<number, Waiting>();
  readonly #imports = new Set<number>();
  readonly #unregister: () => void;
  readonly #detach: (() => void) | undefined;
  #ids = 0;
  #openId: string | null = null;
  #sheet = 0;
  #view = 0;
  #stopped = false;
  #state: TableStatus["state"] = "idle";
  #failure: TableFailure | null = null;
  #pages = 0;

  constructor(
    name: TableHome,
    handlers: TableHandlers,
    start: (name: WorkerName) => TableWorker = startWorker,
  ) {
    this.docId = name.docId;
    this.#remote = name.remote;
    this.#handlers = handlers;
    this.#worker = start(name);
    this.#worker.onmessage = ({ data }: MessageEvent<TableMessage | ArrayBuffer>) =>
      this.#receive(data);
    this.#worker.onerror = (event) => {
      // An error in the script has a message. A plain event means that the script
      // could not be loaded: the page is older than the build on the server.
      const loaded = typeof ErrorEvent !== "undefined" && event instanceof ErrorEvent;
      if (!loaded) {
        reportLoadFailure();
      }
      this.#fail({
        code: loaded ? "crashed" : "load",
        detail: (event as Partial<ErrorEvent>).message,
      });
    };
    this.#worker.onmessageerror = () =>
      this.#fail({ code: "crashed", detail: "a message was lost" });
    this.#unregister = tablesOpened(this);
    this.#detach = name.remote?.attach(this);
    // A page that is put away may not hold on to files that another tab can ask for.
    globalThis.addEventListener?.("pagehide", this.#onPageHide);
  }

  /** Whether something is being imported. */
  get busy(): boolean {
    return this.#imports.size > 0;
  }

  get stopped(): boolean {
    return this.#stopped;
  }

  status(): TableStatus {
    return {
      docId: this.docId,
      state: this.#state,
      failure: this.#failure,
      busy: this.busy,
      pages: this.#pages,
    };
  }

  /**
   * Takes the tables of the workbook, and lists them. Refused with `blocked` when
   * another tab has them, unless `takeOver`, and with `unsupported` where the
   * browser cannot keep tables.
   */
  async acquire(takeOver = false): Promise<readonly TableEntry[]> {
    const remote = this.#remote;
    const tables = await this.#ask("acquire", { takeOver, remote: remote?.target });
    return remote?.tables ?? tables;
  }

  /** Takes over who may do what, for tables on the server. */
  async retarget(remote: RemoteTarget): Promise<void> {
    await this.#ask("acquire", { takeOver: false, remote }).catch(() => {});
  }

  /** Closes a table if it is the one that is open, which is said to have failed. */
  async lose(id: string, failure: TableFailure): Promise<void> {
    if (this.#openId === id) {
      await this.closeTable().catch(() => {});
      this.#lost(failure);
    }
  }

  /** Opens a table, in place of the one that is. */
  async open(id: string): Promise<OpenedTable> {
    const record = this.#remote?.tables.find((table) => table.id === id);
    if (this.#remote && !record) {
      throw new TableError("notFound");
    }
    this.#openId = id;
    const opened = await this.#ask("open", { id, record });
    this.#sheet = opened.sheet;
    this.#view = opened.view.id;
    this.#state = "open";
    tableShown(this);
    return opened;
  }

  async closeTable(): Promise<void> {
    this.#openId = null;
    this.#sheet = 0;
    this.#state = "idle";
    await this.#ask("closeTable", {});
  }

  /** Says which cells pages are to be sent of, and which of them are on screen. */
  subscribe(subscription: TableSubscription, frozenCols = 0): void {
    this.#post({ kind: "subscribe", view: this.#view, frozenCols, subscription });
  }

  /**
   * Shows other rows, or in another order. What is shown stays until the answer,
   * which has the pages to show in its place. A question that another one follows
   * is refused with `superseded`. If it was answered already, `onView` is told.
   */
  async query(query: TableQuery, task?: TableTask): Promise<ViewInfo> {
    const view = await this.#ask("query", { query }, task);
    this.#view = view.id;
    if (task?.signal?.aborted) {
      this.#handlers.onView?.(view, query);
      throw new TableError("superseded");
    }
    return view;
  }

  /** Totals of columns over the rows that are shown. */
  summary(columns: readonly number[], task?: TableTask): Promise<TableSummary> {
    return this.#ask("summary", { columns }, task);
  }

  /**
   * The values of a column that contain a text: the most frequent ones, counted
   * over the rows that the conditions on other columns hold for.
   */
  values(column: number, search = "", limit = 1_000, task?: TableTask): Promise<TableValues> {
    return this.#ask("values", { column, search, limit }, task);
  }

  /** The whole text of a cell. */
  text(row: number, col: number): Promise<string> {
    return this.#ask("text", { row, col });
  }

  /** Cells for the clipboard. At most `MAX_COPY_CELLS` of them. */
  copy(range: CellRange, task?: TableTask): Promise<CopiedCells> {
    return this.#ask("copy", { range }, task);
  }

  rename(id: string, name: string): Promise<void> {
    return this.#change(
      (tables) => tables.map((table) => (table.id === id ? { ...table, name } : table)),
      () => this.#ask("rename", { id, name }),
    );
  }

  /** Puts a table where so many others are before it. */
  move(id: string, position: number): Promise<void> {
    return this.#change(
      (tables) =>
        tables
          .filter((table) => table.id !== id)
          .toSpliced(position, 0, ...tables.filter((table) => table.id === id)),
      () => this.#ask("move", { id, position }),
    );
  }

  /** Deletes a table for good. */
  delete(id: string): Promise<void> {
    return this.#change(
      (tables) => tables.filter((table) => table.id !== id),
      async () => {
        const isQueued = await this.#remote?.giveUp(id);
        try {
          // The server may know of one that is in the queue as well.
          await this.#ask("delete", { id });
        } catch (error) {
          if (!isQueued || (error as TableError).code !== "offline") {
            throw error;
          }
        }
      },
    );
  }

  /** Changes the list of tables, for tables on the server before it has. */
  async #change(
    edit: Parameters<RemoteTables["change"]>[0],
    ask: () => Promise<unknown>,
  ): Promise<void> {
    await (this.#remote?.change(edit, ask) ?? ask());
  }

  /** The file of a table, which is not read for it. */
  export(id: string): Promise<File> {
    return this.#ask("export", { id });
  }

  /** Uploads what somebody has in the queue. See `resumeUploads`. */
  async upload(userId: string, afresh: boolean): Promise<void> {
    await this.#ask("upload", { userId, afresh });
  }

  /** What a text file seems to hold. */
  sniffCsv(source: CsvSource): Promise<SniffedText> {
    return this.#ask("sniffCsv", source);
  }

  /** Adds a table from a text file. */
  importCsv(source: CsvSource, name: string, task?: TableTask): Promise<ImportedTable> {
    return this.#import("importCsv", { ...source, name }, task);
  }

  /** What is in a Parquet file. */
  inspectParquet(file: Blob): Promise<ParquetInfo> {
    return this.#ask("inspectParquet", { file });
  }

  /** Adds a table from the columns of a Parquet file, or from some of them. */
  importParquet(
    file: Blob,
    name: string,
    columns?: readonly number[],
    task?: TableTask,
  ): Promise<ImportedTable> {
    return this.#import("importParquet", { file, name, columns }, task);
  }

  /** Adds a table from a file that `export` gave. */
  importFile(file: Blob, name: string, task?: TableTask): Promise<ImportedTable> {
    return this.#import("importFile", { file, name }, task);
  }

  /** Adds a table that is made up. */
  sample(name: string, rows: number, seed: number, task?: TableTask): Promise<ImportedTable> {
    return this.#import("sample", { name, rows, seed }, task);
  }

  /** For tests: how much memory the engine takes for a question, at most. */
  async setMemoryLimit(bytes: number | null): Promise<void> {
    await this.#ask("memoryLimit", { bytes });
  }

  /** Bytes of memory that are in use for the table that is open, and in all. */
  memory(): Promise<{ readonly table: number; readonly total: number }> {
    return this.#ask("memory", {});
  }

  /** For tests: has the engine fail as a defect would. */
  crash(): void {
    this.#ask("crash", {}).catch(() => {});
  }

  /** Lets go of the files and of the lock, and ends the worker. */
  async close(): Promise<void> {
    if (this.#stopped) {
      return;
    }
    let timer: ReturnType<typeof setTimeout> | undefined;
    const patience = new Promise<void>((resolve) => {
      timer = setTimeout(resolve, CLOSE_PATIENCE_MS);
    });
    await Promise.race([this.#ask("close", {}).catch(() => {}), patience]);
    clearTimeout(timer);
    this.stop();
  }

  /** Ends the worker at once. The browser closes its files and gives up its lock. */
  stop(): void {
    this.#stop({ code: "closed" });
  }

  #stop(failure: TableFailure): void {
    if (this.#stopped) {
      return;
    }
    this.#stopped = true;
    this.#state = failure.code === "closed" || failure.code === "yielded" ? failure.code : "failed";
    this.#failure = failure;
    globalThis.removeEventListener?.("pagehide", this.#onPageHide);
    this.#worker.terminate();
    this.#unregister();
    this.#detach?.();
    for (const { reject } of this.#waiting.values()) {
      reject(new TableError(failure));
    }
    this.#waiting.clear();
    this.#imports.clear();
  }

  #lost(failure: TableFailure): void {
    this.#failure = failure;
    this.#handlers.onTableFailed?.(failure);
  }

  #fail(failure: TableFailure): void {
    if (!this.#stopped) {
      this.#stop(failure);
      this.#handlers.onFailed?.(failure);
    }
  }

  readonly #onPageHide = (): void => this.#fail({ code: "closed" });

  async #import<K extends "importCsv" | "importParquet" | "importFile" | "sample">(
    kind: K,
    ask: TableRequests[K]["ask"],
    task?: TableTask,
  ): Promise<TableRequests[K]["answer"]> {
    const answer = this.#ask(kind, ask, task);
    const id = this.#ids;
    this.#imports.add(id);
    try {
      const imported = await answer;
      // For a workbook on the server it goes there, in its time.
      await this.#remote?.look();
      return imported;
    } finally {
      this.#imports.delete(id);
    }
  }

  #ask<K extends TableRequestKind>(
    kind: K,
    ask: TableRequests[K]["ask"],
    { signal, onProgress }: TableTask = {},
  ): Promise<TableRequests[K]["answer"]> {
    const id = ++this.#ids;
    return new Promise((resolve, reject) => {
      if (this.#stopped) {
        reject(new TableError("closed"));
        return;
      }
      if (signal?.aborted) {
        reject(new TableError("cancelled"));
        return;
      }
      const cancel = () => this.#post({ kind: "cancel", id });
      signal?.addEventListener("abort", cancel);
      this.#waiting.set(id, {
        resolve: (answer) => {
          signal?.removeEventListener("abort", cancel);
          resolve(answer);
        },
        reject: (error) => {
          signal?.removeEventListener("abort", cancel);
          reject(error);
        },
        onProgress,
      });
      this.#post({ kind, id, ask } as TableCommand);
    });
  }

  #post(command: TableCommand): void {
    if (!this.#stopped) {
      this.#worker.postMessage(command);
    }
  }

  #receive(message: TableMessage | ArrayBuffer): void {
    if (this.#stopped) {
      return;
    }
    if (message instanceof ArrayBuffer) {
      // What a table that is no longer open had sent.
      if (new DataView(message).getUint32(SHEET_AT, true) === this.#sheet) {
        this.#pages++;
        this.#handlers.onPage?.(message);
      }
      return;
    }
    switch (message.kind) {
      case "answer":
        this.#waiting.get(message.id)?.resolve(message.answer as never);
        this.#waiting.delete(message.id);
        break;
      case "refused":
        this.#waiting.get(message.id)?.reject(new TableError(message.failure));
        this.#waiting.delete(message.id);
        break;
      case "progress":
        this.#waiting.get(message.id)?.onProgress?.(message.progress);
        break;
      case "catalog":
        this.#handlers.onCatalog?.(message.tables);
        break;
      case "yielded":
        // What it holds in memory is of no use any more.
        this.#stop({ code: "yielded" });
        this.#handlers.onYielded?.(message.evicted);
        break;
      case "remote":
        this.#handlers.onRemote?.(message.remote);
        break;
      case "upload":
        this.#handlers.onUpload?.(message.upload);
        break;
      case "tableFailed":
        this.#lost(message.failure);
        // Others may be gone as well.
        void this.#remote?.refresh();
        break;
      case "failed":
        this.#fail(message.failure);
        break;
    }
  }
}

let uploading: Promise<void> | null = null;
/** What uploads now. */
let uploader: TableClient | null = null;
/** What the server did not take, and is only tried again when that is asked for. */
const refused = new Set<string>();

/**
 * Uploads what somebody has in the queue, in a worker that is there for nothing
 * else; `afresh`: also what could not be. One tab at a time does.
 */
export function resumeUploads(
  userId: string,
  onUpload: (progress: UploadProgress) => void,
  afresh = false,
): Promise<void> {
  if (afresh) {
    refused.clear();
    void uploader?.upload(userId, true).catch(() => {});
  }
  const isDue = async () =>
    (await queuedUploads()).some(
      (upload) => upload.userId === userId && !refused.has(upload.entry.id),
    );
  uploading ??= (async () => {
    if (!(await isDue())) {
      return;
    }
    await navigator.locks.request("reticula:table-uploads", async () => {
      // Another tab may have done so meanwhile.
      if (!(await isDue())) {
        return;
      }
      const client = new TableClient(
        { docId: "", tabId: "" },
        {
          onUpload: (progress) => {
            if (progress.state === "failed" && progress.failure?.code !== "cancelled") {
              refused.add(progress.id);
            }
            onUpload(progress);
          },
        },
      );
      uploader = client;
      try {
        await client.upload(userId, afresh);
      } finally {
        uploader = null;
        client.stop();
      }
    });
  })().finally(() => {
    uploading = null;
  });
  return uploading;
}
