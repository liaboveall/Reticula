import type {
  ImportReport,
  Json,
  ParquetInfo,
  Table,
  TableInfo,
  TableJob,
  TableQuery,
  TableStep,
  TableSummary,
  TableValues,
  TableWork,
} from "@reticula/protocol/table";
import type { TableEntry } from "./catalog.ts";
import { markedEncoding, readHead, TextInput } from "./csv-input.ts";
import {
  dequeue,
  dropCopies,
  enqueue,
  type QueuedUpload,
  queueDir,
  queuedUpload,
  queuedUploads,
  requeue,
  SIGNED_OUT,
  TABLE_EXTENSION,
  UPLOADED,
} from "./delete.ts";
import { TableError, type TableFailure } from "./failure.ts";
import { Feed } from "./feed.ts";
import { fileFailure, onDisk, openStored, PickedFile, type StoredFile } from "./files.ts";
import { TablesLock } from "./locks.ts";
import {
  type CellRange,
  type CopiedCells,
  type CsvSource,
  type ImportedTable,
  MAX_COPY_CELLS,
  type OpenedTable,
  type SniffedText,
  type TableCommand,
  type TableMessage,
  type TableProgress,
  type TableRequestKind,
  type TableRequests,
  type TableSubscription,
  type ViewInfo,
  type WorkerName,
} from "./protocol.ts";
import { JSON_TYPE, Later, type RemoteTarget, send, tableUrl, whole } from "./remote-api.ts";
import { TableStore } from "./store.ts";
import { type TurnOptions, Turns } from "./turns.ts";
import { sourceOf, Uploader } from "./uploads.ts";

type Bindings = typeof import("@reticula/protocol/table");

/** What of the engine's bindings is used. */
export type TableEngine = Pick<
  Bindings,
  | "Table"
  | "TableJob"
  | "TableStep"
  | "TableWork"
  | "crash"
  | "csvHeadBytes"
  | "csvSniff"
  | "lastTrouble"
  | "panicMessage"
>;

/** How long pages are packed before commands get their turn. */
const PAGE_SLICE_MS = 4;

/** How long a tab that the tables were taken from may still have its files open. */
const TAKEOVER_FILE_PATIENCE_MS = 30_000;
const PROGRESS_EVERY_MS = 100;
/** How much of a file is copied at a time. */
const COPY_BYTES = 4 << 20;
/** How much text the engine is given at a time. */
const FEED_BYTES = 4 << 20;
/** About how many cells a step of a question handles. */
const STEP_CELLS = 65_536;

export interface TablesHostOptions extends WorkerName {
  readonly engine: TableEngine;
  /** Bytes of memory the engine has. */
  readonly memoryBytes: () => number;
  readonly post: (message: TableMessage | ArrayBuffer, transfer?: Transferable[]) => void;
  readonly locks: LockManager | undefined;
  readonly channel: BroadcastChannel | null;
  /** See `Turns`. */
  readonly defer?: TurnOptions["defer"];
}

/** A file, and why the last call of it failed. */
interface FailingFile {
  readonly failure: TableFailure | null;
}

/** What something that is done to its end is done with. */
interface Doing {
  readonly request: number;
  readonly signal: AbortSignal;
  /** What is written. */
  readonly file: FailingFile & { restart(): void };
  /** What is read. */
  readonly source?: FailingFile;
  readonly input?: TextInput;
  /** How much is done, when the engine does not know. */
  readonly share?: (() => number) | undefined;
}

/** The table that is shown. */
interface Shown {
  readonly id: string;
  readonly sheet: number;
  /** Its file, if that is in this browser. */
  readonly file: StoredFile | null;
  /** Where it comes from otherwise. */
  readonly feed: Feed | null;
  readonly table: Table;
  readonly info: TableInfo;
  view: number;
  /** Aborted when the table is closed. */
  readonly closing: AbortController;
  /** Aborted when other cells are copied, or they no longer are where they were. */
  copying: AbortController;
  /** What was asked and is not answered yet. */
  readonly asked: Map<TableJob, Asked>;
  /** Whether it is being worked on. */
  working: boolean;
}

/** A question about a table. */
interface Asked {
  /** Asks the engine. */
  readonly begin: () => TableStep;
  readonly resolve: () => void;
  readonly reject: (error: unknown) => void;
  readonly progress: (progress: () => TableProgress) => void;
}

interface Running {
  readonly controller: AbortController;
  readonly done: Promise<unknown>;
}

/** What the worker behind the tables of a workbook does. */
export class TablesHost {
  readonly #options: TablesHostOptions;
  readonly #lock: TablesLock;
  readonly #turns: Turns;
  readonly #running = new Map<number, Running>();
  #store: TableStore | null = null;
  #shown: Shown | null = null;
  /** Where the tables are, unless they are in this browser. */
  #remote: RemoteTarget | null = null;
  #opening = new AbortController();
  #uploader: Uploader | null = null;
  readonly #signedOut = new AbortController();
  #toldAt = 0;
  #importing = false;
  #memoryLimit: number | null = null;
  #sheets = 0;
  #views = 0;
  #stopped = false;
  /** What changes which tables there are, or which one is shown, happens one after the other. */
  #changes: Promise<unknown> = Promise.resolve();

  constructor(options: TablesHostOptions) {
    this.#options = options;
    this.#lock = new TablesLock({ ...options, onYield: (evicted) => this.#yield(evicted) });
    options.channel?.addEventListener("message", ({ data }) => {
      const feed = this.#shown?.feed;
      if (data?.kind === SIGNED_OUT) {
        this.#signedOut.abort(new TableError("closed"));
        void feed?.leaveDisk();
      }
      if (data?.kind === UPLOADED && this.#shown?.id === data.id) {
        feed?.uploaded(data.sha256);
      }
    });
    this.#turns = new Turns({
      ...(options.defer ? { defer: options.defer } : {}),
      pages: () => {
        try {
          return this.#sendPages();
        } catch (error) {
          this.fail(error);
          return false;
        }
      },
    });
  }

  handle(command: TableCommand): void {
    if (this.#stopped) {
      return;
    }
    if (command.kind === "subscribe") {
      this.#subscribe(command.view, command.frozenCols, command.subscription);
      return;
    }
    if (command.kind === "cancel") {
      this.#running.get(command.id)?.controller.abort(new TableError("cancelled"));
      return;
    }
    const { id } = command;
    const controller = new AbortController();
    const done = this.#work(command, controller.signal).then(
      (answer) => {
        this.#options.post({ kind: "answer", id, answer }, pagesOf(answer));
      },
      (error: unknown) => {
        const failure = this.#isFatal(error) ? null : knownFailure(error);
        if (failure) {
          this.#options.post({ kind: "refused", id, failure });
        } else {
          this.fail(error);
        }
      },
    );
    this.#running.set(id, { controller, done });
    void done.finally(() => this.#running.delete(id));
  }

  /** After a failure of the engine nothing of it can be trusted. */
  fail(error: unknown): void {
    if (this.#stopped) {
      return;
    }
    this.#stopped = true;
    for (const { controller } of this.#running.values()) {
      controller.abort(error);
    }
    this.#closeFiles();
    this.#lock.dispose();
    this.#options.post({ kind: "failed", failure: this.#crashFailure(error) });
  }

  async #work(
    command: Exclude<TableCommand, { kind: "subscribe" | "cancel" }>,
    signal: AbortSignal,
  ): Promise<TableRequests[TableRequestKind]["answer"]> {
    const { TableJob: Job } = this.#options.engine;
    switch (command.kind) {
      case "acquire":
        return this.#change(() => this.#acquire(command.ask, signal));
      case "open": {
        const either = AbortSignal.any([signal, this.#supersede()]);
        return this.#change(() => this.#open(command.ask, either));
      }
      case "closeTable":
        this.#supersede();
        return this.#change(() => this.#closeShown());
      case "query":
        return this.#query(command.id, command.ask.query, signal);
      case "summary":
        return this.#find(Job.Summary, command.ask.columns, command.id, signal);
      case "values":
        return this.#find(Job.Values, command.ask, command.id, signal);
      case "text":
        return this.#text(command.ask.row, command.ask.col, signal);
      case "copy":
        return this.#copy(command.ask.range, signal);
      case "rename":
      case "move": {
        const { id, ...change } = command.ask;
        if (this.#remote) {
          return this.#changeRemote(id, change);
        }
        return this.#edit((store) =>
          "name" in change ? store.rename(id, change.name) : store.move(id, change.position),
        );
      }
      case "delete": {
        const { id } = command.ask;
        if (this.#remote) {
          await this.#askServer(id, "", { method: "DELETE" });
          return null;
        }
        return this.#edit(async (store) => {
          if ((await queuedUploads()).some(({ from }) => from?.id === id)) {
            throw new TableError("uploading");
          }
          if (this.#shown?.id === id) {
            await this.#closeShown();
          }
          await store.delete(id);
        });
      }
      case "export":
        return this.#export(command.ask.id);
      case "sniffCsv":
        return this.#sniffCsv(command.ask);
      case "importCsv":
        return this.#importCsv(command.id, command.ask, signal);
      case "inspectParquet":
        return this.#inspectParquet(command.ask.file);
      case "importParquet":
        return this.#importParquet(command.id, command.ask, signal);
      case "importFile":
        return this.#importFile(command.id, command.ask.file, command.ask.name, signal);
      case "sample":
        return this.#sample(command.id, command.ask, signal);
      case "upload": {
        const { userId, afresh } = command.ask;
        this.#uploader ??= new Uploader(
          this.#options.engine,
          (upload) => this.#options.post({ kind: "upload", upload }),
          this.#options.channel,
        );
        const either = AbortSignal.any([signal, this.#signedOut.signal]);
        await this.#uploader.run(userId, afresh, either);
        return null;
      }
      case "memoryLimit":
        this.#memoryLimit = command.ask.bytes;
        this.#limitMemory();
        return null;
      case "memory":
        return {
          table: this.#shown?.table.memory() ?? 0,
          total: this.#options.memoryBytes(),
        };
      case "crash":
        this.#options.engine.crash();
        return null;
      case "close":
        await this.#letGo(new TableError("closed"), command.id);
        this.#lock.release();
        return null;
    }
  }

  /** Runs something after the changes before it. */
  #change<T>(run: () => Promise<T>): Promise<T> {
    const result = this.#changes.then(run);
    this.#changes = result.catch(() => {});
    return result;
  }

  #edit(run: (store: TableStore) => Promise<unknown>): Promise<null> {
    return this.#change(async () => {
      await run(this.#held());
      this.#tell();
      return null;
    });
  }

  /** Asks the server for something about a table. */
  async #askServer(id: string, of: string, init: RequestInit): Promise<Blob> {
    const target = this.#remote as RemoteTarget;
    try {
      // What is gone need not be deleted.
      const expected = init.method === "DELETE" ? [404] : [];
      return await whole(
        (await send(target, tableUrl(target.docId, id) + of, init, expected)).blob(),
      );
    } catch (error) {
      throw error instanceof Later ? new TableError("offline") : error;
    }
  }

  async #changeRemote(id: string, change: { name?: string; position?: number }): Promise<null> {
    const queued = await queuedUpload(id);
    if (queued) {
      // Where it is among others does not matter here.
      await requeue(id, (upload) => ({ ...upload, entry: { ...upload.entry, ...change } }));
      if (!queued.isDeclared) {
        return null;
      }
    }
    const body = JSON.stringify(change);
    await this.#askServer(id, "", { method: "PATCH", headers: JSON_TYPE, body });
    return null;
  }

  async #export(id: string): Promise<File> {
    if (!this.#remote) {
      return this.#held().blob(id);
    }
    const queued = await queuedUpload(id);
    return queued ? sourceOf(queued) : new File([await this.#askServer(id, "/data", {})], id);
  }

  #tell(): void {
    if (!this.#remote) {
      this.#options.post({ kind: "catalog", tables: this.#store?.tables ?? [] });
    }
  }

  /** The tables, which nobody has without the lock. */
  #held(): TableStore {
    if (!this.#store || !this.#lock.held) {
      throw new TableError("yielded");
    }
    return this.#store;
  }

  async #acquire(
    { takeOver, remote }: TableRequests["acquire"]["ask"],
    signal: AbortSignal,
  ): Promise<readonly TableEntry[]> {
    if (remote) {
      this.#remote = remote;
      if (!remote.canWrite) {
        await this.#shown?.feed?.leaveDisk();
      }
      return [];
    }
    if (this.#store && this.#lock.held) {
      return this.#store.tables;
    }
    const waited = !(await this.#lock.acquire(false));
    if (waited && !(await this.#lock.acquire(takeOver))) {
      throw new TableError("blocked");
    }
    try {
      const store = await onDisk(() =>
        TableStore.open(
          this.#options.docId,
          (file) => this.#inspect(file),
          waited ? TAKEOVER_FILE_PATIENCE_MS : undefined,
        ),
      );
      // They may have been asked for in the meantime.
      signal.throwIfAborted();
      this.#store = store;
      return store.tables;
    } catch (error) {
      this.#lock.release();
      throw error;
    }
  }

  #inspect(file: StoredFile): { rows: number; cols: number } | null {
    const table = new this.#options.engine.Table(file, file.size);
    try {
      if (table.open() !== this.#options.engine.TableStep.Done) {
        return null;
      }
      const { rows, columns } = this.#info(table);
      return { rows, cols: columns.length };
    } finally {
      table.free();
    }
  }

  #info(table: Table): TableInfo {
    return read(table.answer(this.#options.engine.TableJob.None)) as TableInfo;
  }

  /** What is being opened is no longer. */
  #supersede(): AbortSignal {
    this.#opening.abort(new TableError("superseded"));
    this.#opening = new AbortController();
    return this.#opening.signal;
  }

  async #openFile(id: string): Promise<StoredFile> {
    const store = this.#held();
    const file = await store.openFile(id);
    if (this.#store !== store) {
      file.close();
      throw new TableError("yielded");
    }
    return file;
  }

  async #open(
    { id, record }: TableRequests["open"]["ask"],
    signal: AbortSignal,
  ): Promise<OpenedTable> {
    await this.#closeShown();
    signal.throwIfAborted();
    const { engine } = this.#options;
    let file: StoredFile | null = null;
    let feed: Feed | null = null;
    let queued: QueuedUpload | undefined;
    let table: Table;
    const target = this.#remote;
    if (record && target) {
      const { bytes, status } = record;
      queued = await queuedUpload(id);
      if (!queued && status !== "ready") {
        throw new TableError(status === "missing" ? "notFound" : "uploading");
      }
      table = new engine.Table(queued && new PickedFile(await sourceOf(queued)), bytes);
      feed = new Feed({
        target,
        id,
        bytes,
        table,
        steps: engine.TableStep,
        onChange: () => this.#arrived(feed as Feed),
        onCrash: (error) => this.fail(error),
      });
    } else {
      file = await this.#openFile(id);
      table = new engine.Table(file, file.size);
    }
    const closing = new AbortController();
    const shown = { file, feed, table, closing };
    try {
      if (queued) {
        void feed?.follow(queued);
      } else if (record) {
        await feed?.start(record.sha256);
      }
      while (table.open() !== engine.TableStep.Done) {
        if (feed?.isLost) {
          throw new TableError("offline");
        }
        await this.#wait(shown, signal);
      }
      this.#shown = {
        ...shown,
        id,
        sheet: ++this.#sheets,
        info: this.#info(table),
        view: ++this.#views,
        copying: new AbortController(),
        asked: new Map(),
        working: false,
      };
      this.#limitMemory();
    } catch (error) {
      feed?.close();
      table.free();
      file?.close();
      throw error;
    }
    if (feed) {
      this.#arrived(feed);
    }
    const { sheet, info } = this.#shown;
    return { sheet, info, view: this.#viewInfo(this.#shown) };
  }

  /** Something of a table on the server has come, or it turned out that nothing will. */
  #arrived(feed: Feed): void {
    if (this.#shown?.feed !== feed) {
      return;
    }
    const { failure, have, isWhole, isStalled } = feed;
    if (failure) {
      this.#options.post({ kind: "tableFailed", failure });
      void this.#change(() => this.#closeShown());
      return;
    }
    this.#turns.wake();
    const now = performance.now();
    if (isWhole || isStalled || now - this.#toldAt >= PROGRESS_EVERY_MS) {
      this.#toldAt = now;
      this.#options.post({ kind: "remote", remote: { have, total: feed.bytes, isStalled } });
    }
  }

  /** Waits for what the engine missed just now, if it will come. */
  async #wait(
    { table, feed, file }: Pick<Shown, "table" | "feed" | "file">,
    signal: AbortSignal,
  ): Promise<void> {
    if (!feed || feed.failure || !table.isWaiting()) {
      throw new TableError(feed?.failure ?? this.#lastFailure(file));
    }
    await feed.next(signal);
  }

  #closeFiles(): void {
    this.#shown?.feed?.close();
    this.#shown?.file?.close();
  }

  async #closeShown(): Promise<null> {
    const shown = this.#shown;
    if (shown) {
      const closed = new TableError("closed");
      shown.closing.abort(closed);
      for (const { reject } of shown.asked.values()) {
        reject(closed);
      }
      this.#closeFiles();
      shown.table.free();
    }
    this.#shown = null;
    return null;
  }

  /** The table that is shown. */
  #current(): Shown {
    if (!this.#shown) {
      throw new TableError("closed");
    }
    return this.#shown;
  }

  #subscribe(view: number, frozenCols: number, cells: TableSubscription): void {
    const shown = this.#shown;
    if (!shown || shown.view !== view) {
      return;
    }
    const { firstRow, rowCount, firstCol, colCount, visible } = cells;
    shown.table.subscribe(
      shown.sheet,
      cells.epoch,
      cells.resend,
      Uint32Array.of(
        frozenCols,
        firstRow,
        rowCount,
        firstCol,
        colCount,
        visible.firstRow,
        visible.endRow,
        visible.firstCol,
        visible.endCol,
      ),
    );
    // What is waited for otherwise is so anew.
    shown.feed?.wake();
    this.#turns.wake();
  }

  /** Sends pages for a slice. Whether there are more. */
  #sendPages(): boolean {
    const shown = this.#shown;
    if (this.#stopped || !shown) {
      return false;
    }
    const started = performance.now();
    for (;;) {
      const page = shown.table.nextPage();
      if (!page) {
        // What was missing is to come.
        shown.feed?.ask();
        return false;
      }
      if (page.length === 0) {
        this.#options.post({ kind: "tableFailed", failure: this.#lastFailure(shown.file) });
        return false;
      }
      const buffer = page.buffer as ArrayBuffer;
      this.#options.post(buffer, [buffer]);
      if (performance.now() - started >= PAGE_SLICE_MS) {
        return true;
      }
    }
  }

  /** What is shown first of a view, for the grid to show all at once. */
  #viewInfo({ table, view }: Shown): ViewInfo {
    const pages: ArrayBuffer[] = [];
    // Nothing more is sent for the view before: it would be shown as this one.
    const rows = table.toTop();
    for (let page = table.nextPage(); page?.length; page = table.nextPage()) {
      pages.push(page.buffer as ArrayBuffer);
    }
    return { id: view, rows, pages };
  }

  #limitMemory(): void {
    this.#shown?.table.setMemoryLimit(this.#memoryLimit ?? Number.MAX_SAFE_INTEGER);
  }

  /** Shows other rows, or in another order. What is shown stays until that is there. */
  async #query(id: number, query: TableQuery, signal: AbortSignal): Promise<ViewInfo> {
    const shown = this.#current();
    await this.#ask(shown, this.#options.engine.TableJob.View, query, id, signal);
    shown.copying.abort(new TableError("superseded"));
    shown.view = ++this.#views;
    return this.#viewInfo(shown);
  }

  /** What there is to find out about the rows that are shown. */
  async #find<T extends TableSummary | TableValues>(
    job: TableJob,
    question: object,
    id: number,
    signal: AbortSignal,
  ): Promise<T> {
    const shown = this.#current();
    await this.#ask(shown, job, question, id, signal);
    return read(shown.table.answer(job)) as T;
  }

  /**
   * Asks something, and waits for the answer. A question of the same kind that
   * was asked before is not answered any more.
   */
  async #ask(
    shown: Shown,
    job: TableJob,
    question: object,
    id: number,
    signal: AbortSignal,
  ): Promise<void> {
    const begin = () => shown.table.ask(job, JSON.stringify(question));
    const progress = this.#progress(id);
    const { feed, asked: questions, closing } = shown;
    if (feed && !feed.isWhole) {
      // It is about all rows, some of which are not here.
      const waiting = new AbortController();
      const reject = (error: unknown) => waiting.abort(error);
      questions.get(job)?.reject(new TableError("superseded"));
      shown.table.cancel(job);
      questions.set(job, { begin, progress, reject, resolve: () => {} });
      await feed.whole(AbortSignal.any([signal, closing.signal, waiting.signal]), () => {
        const { have, bytes } = feed;
        progress(() => ({ share: have / bytes, have }));
      });
    }
    this.#finished(begin(), { failure: shown.file?.failure ?? null });
    questions.get(job)?.reject(new TableError("superseded"));
    return new Promise((resolve, reject) => {
      const asked: Asked = { begin, resolve, reject, progress };
      shown.asked.set(job, asked);
      signal.addEventListener("abort", () => {
        if (shown.asked.get(job) === asked) {
          shown.asked.delete(job);
          if (!shown.closing.signal.aborted) {
            shown.table.cancel(job);
          }
        }
        reject(signal.reason);
      });
      if (!shown.working) {
        shown.working = true;
        this.#answer(shown).catch((error: unknown) => {
          if (!shown.closing.signal.aborted) {
            this.fail(error);
          }
        });
      }
    });
  }

  /** Works on what was asked until all of it is answered. */
  async #answer(shown: Shown): Promise<void> {
    const { TableJob, TableStep } = this.#options.engine;
    const { table, asked, closing } = shown;
    for (;;) {
      await this.#turns.pause(closing.signal);
      const job = table.job();
      if (job === TableJob.None) {
        shown.working = false;
        return;
      }
      const step = table.step(STEP_CELLS);
      const question = asked.get(job);
      if (step === TableStep.Working) {
        question?.progress(() => ({ share: table.share() }));
        continue;
      }
      if (step === TableStep.Need || step === TableStep.Restart) {
        if (step === TableStep.Restart) {
          // Something is damaged, for which the engine gave it up.
          question?.begin();
        }
        // When nothing will come the table is closed.
        await shown.feed?.next(closing.signal).catch(() => {});
        continue;
      }
      asked.delete(job);
      if (step === TableStep.Done) {
        question?.resolve();
      } else {
        table.cancel(job);
        question?.reject(new TableError(this.#lastFailure(shown.file)));
      }
    }
  }

  async #text(row: number, col: number, signal: AbortSignal): Promise<string> {
    const shown = this.#current();
    const either = AbortSignal.any([signal, shown.closing.signal]);
    for (;;) {
      const text = shown.table.text(row, col);
      if (text !== undefined) {
        return text;
      }
      await this.#wait(shown, either);
    }
  }

  async #copy(range: CellRange, signal: AbortSignal): Promise<CopiedCells> {
    const shown = this.#current();
    const { table, closing } = shown;
    if (range.rows * range.cols > MAX_COPY_CELLS) {
      throw new TableError("tooLarge");
    }
    shown.copying.abort(new TableError("superseded"));
    shown.copying = new AbortController();
    const either = AbortSignal.any([signal, closing.signal, shown.copying.signal]);
    await this.#turns.pause(either);
    table.beginCopy(range.firstRow, range.rows, range.firstCol, range.cols);
    const { TableStep } = this.#options.engine;
    for (;;) {
      const step = table.stepCopy();
      if (step === TableStep.Done) {
        break;
      }
      await (step === TableStep.Working ? this.#turns.pause(either) : this.#wait(shown, either));
    }
    return { tsv: table.takeCopy(false), html: table.takeCopy(true) };
  }

  /** Adds a table, whose file `write` writes. Nothing is left of one that fails. */
  async #import(
    name: string,
    signal: AbortSignal,
    write: (file: StoredFile) => Promise<ImportReport | null>,
  ): Promise<ImportedTable> {
    const place = this.#remote ? await queueOf(this.#remote) : this.#held();
    if (this.#importing) {
      throw new TableError("busy");
    }
    this.#importing = true;
    const id = crypto.randomUUID();
    let file: StoredFile | null = null;
    try {
      file = await place.create(id);
      const report = await this.#withRoom(file, write);
      file.flush();
      const size = this.#inspect(file);
      if (!size) {
        throw new TableError("corrupt");
      }
      signal.throwIfAborted();
      const entry: TableEntry = { id, name: place.freeName(name), ...size, bytes: file.size };
      return await this.#change(async () => {
        signal.throwIfAborted();
        await place.add(entry);
        this.#tell();
        return { entry, report };
      });
    } catch (error) {
      file?.close();
      await place.discard(id).catch(() => {});
      throw error;
    } finally {
      file?.close();
      this.#importing = false;
    }
  }

  /** Writes a file, once more if there is room only without the copies of tables. */
  async #withRoom<T>(file: StoredFile, write: (file: StoredFile) => Promise<T>): Promise<T> {
    try {
      return await write(file);
    } catch (error) {
      if (fileFailure(error).code !== "quota") {
        throw error;
      }
      await this.#shown?.feed?.leaveDisk();
      await dropCopies();
      file.restart();
      file.failure = null;
      return write(file);
    }
  }

  /** Does something that takes a while. What became of it. */
  async #do(
    work: TableWork,
    { request, signal, file, source = file, input, share }: Doing,
  ): Promise<ImportReport> {
    const { TableStep } = this.#options.engine;
    const progress = this.#progress(request);
    let piece: Uint8Array = new Uint8Array();
    try {
      this.#finished(work.open(), source);
      for (;;) {
        await this.#turns.pause(signal);
        const step = work.step();
        if (step === TableStep.Restart) {
          file.restart();
          input?.restart();
          piece = new Uint8Array();
        } else if (step === TableStep.NeedInput) {
          while (piece.length === 0) {
            const next = await input?.read();
            if (!next) {
              break;
            }
            piece = next;
          }
          if (!work.feed(piece.subarray(0, FEED_BYTES))) {
            throw new TableError("memory");
          }
          piece = piece.subarray(FEED_BYTES);
        } else if (this.#finished(step, { failure: source.failure ?? file.failure })) {
          return read(work.report()) as ImportReport;
        }
        progress(() => {
          const [done = 0, rows, pass] = work.progress();
          return { share: share?.() ?? done, rows: rows || undefined, pass };
        });
      }
    } finally {
      work.free();
    }
  }

  async #importFile(
    request: number,
    source: Blob,
    name: string,
    signal: AbortSignal,
  ): Promise<ImportedTable> {
    const progress = this.#progress(request);
    return this.#import(name, signal, async (file) => {
      for (let at = 0; at < source.size; at += COPY_BYTES) {
        await this.#turns.pause(signal);
        const bytes = await source.slice(at, at + COPY_BYTES).arrayBuffer();
        if (!file.append(new Uint8Array(bytes))) {
          throw new TableError(file.failure ?? "io");
        }
        progress(() => ({ share: file.size / source.size }));
      }
      await this.#do(this.#options.engine.TableWork.check(file, file.size), {
        request,
        signal,
        file,
      });
      return null;
    });
  }

  async #sniffCsv({ file, encoding: given, options }: CsvSource): Promise<SniffedText> {
    const { engine } = this.#options;
    const encoding = given ?? (await markedEncoding(file)) ?? "utf-8";
    const bytes = engine.csvHeadBytes();
    const head = await readHead(file, encoding, bytes);
    return {
      ...read(engine.csvSniff(head, head.length < bytes, JSON.stringify(options))),
      encoding,
    };
  }

  async #importCsv(
    request: number,
    { file: source, encoding: given, options, name }: TableRequests["importCsv"]["ask"],
    signal: AbortSignal,
  ): Promise<ImportedTable> {
    const { engine } = this.#options;
    return this.#import(name, signal, async (file) => {
      const encoding = given ?? (await markedEncoding(source)) ?? "utf-8";
      const input = new TextInput(source, encoding);
      const head = await readHead(source, encoding, engine.csvHeadBytes());
      const work = engine.TableWork.csv(file, JSON.stringify(options), head);
      const share = () => input.position / source.size;
      return this.#do(work, { request, signal, file, input, share });
    });
  }

  async #inspectParquet(source: Blob): Promise<ParquetInfo> {
    const file = new PickedFile(source);
    const work = this.#options.engine.TableWork.parquet(file, source.size, file, "");
    try {
      this.#finished(work.open(), file);
      return read(work.report()) as ParquetInfo;
    } finally {
      work.free();
    }
  }

  async #importParquet(
    request: number,
    { file: picked, name, columns }: TableRequests["importParquet"]["ask"],
    signal: AbortSignal,
  ): Promise<ImportedTable> {
    return this.#import(name, signal, async (file) => {
      const source = new PickedFile(picked);
      const chosen = columns ? JSON.stringify(columns) : "";
      const work = this.#options.engine.TableWork.parquet(source, picked.size, file, chosen);
      return this.#do(work, { request, signal, file, source });
    });
  }

  async #sample(
    request: number,
    { name, rows, seed }: TableRequests["sample"]["ask"],
    signal: AbortSignal,
  ): Promise<ImportedTable> {
    return this.#import(name, signal, async (file) => {
      const work = this.#options.engine.TableWork.sample(file, rows, seed);
      await this.#do(work, { request, signal, file });
      return null;
    });
  }

  /** Says how far something is, not too often. */
  #progress(id: number): (progress: () => TableProgress) => void {
    let last = 0;
    return (progress) => {
      const now = performance.now();
      if (now - last >= PROGRESS_EVERY_MS) {
        last = now;
        this.#options.post({ kind: "progress", id, progress: progress() });
      }
    };
  }

  async #yield(evicted: boolean): Promise<void> {
    await this.#letGo(new TableError("yielded"));
    this.#options.post({ kind: "yielded", evicted });
  }

  /** Gives up what is being done, and closes all files. */
  async #letGo(reason: TableError, except?: number): Promise<void> {
    this.#store?.close();
    this.#store = null;
    const running = [...this.#running].filter(([id]) => id !== except).map(([, job]) => job);
    for (const { controller } of running) {
      controller.abort(reason);
    }
    await this.#closeShown();
    await Promise.all(running.map(({ done }) => done));
  }

  /** Whether a step was the last. Throws when it failed. */
  #finished(step: TableStep, file: FailingFile): boolean {
    const { TableStep } = this.#options.engine;
    if (step === TableStep.Failed || step === TableStep.Need) {
      throw new TableError(this.#lastFailure(file));
    }
    return step === TableStep.Done;
  }

  /** Why the engine failed last. */
  #lastFailure(file: FailingFile | null): TableFailure {
    const { reason: code, ...rest } = read(this.#options.engine.lastTrouble());
    return (code === "io" && file?.failure) || { code, ...rest };
  }

  /** Whether an exception comes out of the engine itself. */
  #isFatal(error: unknown): boolean {
    return (
      error instanceof WebAssembly.RuntimeError ||
      (error instanceof Error && /too much recursion|call stack/i.test(error.message)) ||
      this.#panic() !== undefined
    );
  }

  #panic(): string | undefined {
    try {
      return this.#options.engine.panicMessage();
    } catch {
      return undefined;
    }
  }

  #crashFailure(error: unknown): TableFailure {
    const panic = this.#panic();
    const detail = panic ?? String((error as Error)?.message ?? error);
    // The engine gives up in the same way when it is not given memory, but says nothing.
    const memory = panic === undefined && error instanceof WebAssembly.RuntimeError;
    return { code: memory && /unreachable/i.test(detail) ? "memory" : "crashed", detail };
  }
}

/** Where what is imported for a workbook on the server is, until it is uploaded. */
async function queueOf({ docId, userId, canWrite }: RemoteTarget) {
  if (!canWrite) {
    throw new TableError("denied");
  }
  const dir = await onDisk(() => queueDir(true));
  return {
    create: (id: string) => openStored(dir, id + TABLE_EXTENSION, true, 0),
    freeName: (name: string) => name,
    // While its file is still open: without this it would be one that is left over.
    add: (entry: TableEntry) =>
      enqueue({ userId, docId, entry, sha256: "", queuedAt: Date.now(), from: null }),
    discard: dequeue,
  };
}

/** The pages in an answer, which are handed over and not copied. */
function pagesOf(answer: TableRequests[TableRequestKind]["answer"]): ArrayBuffer[] {
  if (typeof answer !== "object" || answer === null) {
    return [];
  }
  const view = "view" in answer ? answer.view : answer;
  return "pages" in view ? [...view.pages] : [];
}

/** What an exception means that was to be reckoned with. */
function knownFailure(error: unknown): TableFailure | null {
  if (error instanceof RangeError && /alloc/i.test(error.message)) {
    return { code: "memory", detail: error.message };
  }
  const isKnown = error instanceof TableError || error instanceof DOMException;
  return isKnown ? fileFailure(error) : null;
}

/** What a text in JSON says. */
function read<T>(json: Json<T>): T {
  return JSON.parse(json);
}
