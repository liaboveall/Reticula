import type { TableQuery } from "@reticula/protocol/table";
import type { TableEntry } from "../catalog.ts";
import { TableClient, type TableHome, type TableTask } from "../client.ts";
import { TableError, type TableFailure } from "../failure.ts";
import type { ImportedTable, RemoteProgress, TableProgress, ViewInfo } from "../protocol.ts";

/** How long the worker stays when no table is in view. */
const IDLE_MS = 60_000;

/** Whoever shows a table. */
export interface SessionUser {
  acceptPage(page: ArrayBuffer): void;
  /** These rows are there now, in answer to a question that was given up. */
  acceptView(view: ViewInfo, query: TableQuery): void;
  /** Of a table on the server, so much is here. */
  acceptPart(progress: RemoteProgress): void;
  fail(failure: TableFailure): void;
}

/** A file that is being imported, or something that is made up. */
export interface Importing {
  readonly name: string;
  readonly progress: TableProgress | null;
  readonly cancel: () => void;
}

export interface SessionHost extends TableHome {
  readonly onCatalog: (tables: readonly TableEntry[]) => void;
}

export function failureOf(error: unknown): TableFailure {
  return error instanceof TableError
    ? error.failure
    : { code: "crashed", detail: error instanceof Error ? error.message : String(error) };
}

/** Workers that are letting go of the tables of workbooks. */
const closing = new Map<string, Promise<void>>();

/** The worker behind the tables of a workbook, for as long as there is a use for it. */
export class TableSession {
  readonly #host: SessionHost;
  #client: TableClient | null = null;
  #acquiring: Promise<TableClient> | null = null;
  #user: SessionUser | null = null;
  #importing: Importing | null = null;
  #listeners = new Set<() => void>();
  #idle: ReturnType<typeof setTimeout> | undefined;
  #closed = false;
  /** Whether the worker that had the tables is gone, and may not have let go of them yet. */
  #isOwner = false;

  constructor(host: SessionHost) {
    this.#host = host;
  }

  get isRemote(): boolean {
    return this.#host.remote !== undefined;
  }

  get importing(): Importing | null {
    return this.#importing;
  }

  subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  /** Who has the pages, and is told of failures. Without anybody the worker goes after a while. */
  use(user: SessionUser | null): void {
    this.#user = user;
    this.#wind();
  }

  /** Whether somebody still has the pages. */
  isUsedBy(user: SessionUser): boolean {
    return this.#user === user;
  }

  /** A worker, which need not have the tables. */
  started(): TableClient {
    const running = this.#client;
    if (running && !running.stopped) {
      return running;
    }
    const isCurrent = () => this.#client === client;
    const lose = (failure: TableFailure, isOwner: boolean) => {
      if (isCurrent()) {
        this.#acquiring = null;
        this.#isOwner = isOwner;
        this.#user?.fail(failure);
      }
    };
    const client: TableClient = new TableClient(this.#host, {
      onPage: (page) => isCurrent() && this.#user?.acceptPage(page),
      onView: (view, query) => isCurrent() && this.#user?.acceptView(view, query),
      onCatalog: this.#host.onCatalog,
      onRemote: (progress) => isCurrent() && this.#user?.acceptPart(progress),
      onYielded: () => lose({ code: "yielded" }, false),
      onTableFailed: (failure) => isCurrent() && this.#user?.fail(failure),
      onFailed: (failure) => lose(failure, true),
    });
    this.#client = client;
    this.#acquiring = null;
    this.#wind();
    return client;
  }

  /** A worker that has the tables. */
  client(takeOver = this.#isOwner): Promise<TableClient> {
    if (this.#closed) {
      return Promise.reject(new TableError("closed"));
    }
    const client = this.started();
    if (!this.#acquiring || takeOver) {
      const free = closing.get(this.#host.docId) ?? Promise.resolve();
      const acquiring = free
        .then(() => client.acquire(takeOver))
        .then((tables) => {
          this.#isOwner = false;
          this.#host.onCatalog(tables);
          return client;
        });
      this.#acquiring = acquiring;
      acquiring.catch(() => {
        if (this.#acquiring === acquiring) {
          this.#acquiring = null;
        }
      });
    }
    return this.#acquiring;
  }

  /** Has files removed that belong to no table, unless somebody else has the tables. */
  tidy(): void {
    this.client().catch(() => {
      if (!this.#user && !this.#importing) {
        this.#end();
      }
    });
  }

  /** Adds a table, with the progress of it for all to see. */
  async import(
    name: string,
    start: (client: TableClient, task: TableTask) => Promise<ImportedTable>,
  ): Promise<ImportedTable> {
    if (this.#importing) {
      throw new TableError("busy");
    }
    const controller = new AbortController();
    // An update of the app would put an end to it, which `sw-client.ts` asks about.
    const done = Promise.withResolvers<void>();
    navigator.locks
      ?.request("reticula:importing", { mode: "shared" }, () => done.promise)
      .catch(() => {});
    const show = (progress: TableProgress | null) =>
      this.#setImporting({ name, progress, cancel: () => controller.abort() });
    show(null);
    try {
      const client = await this.client();
      return await start(client, { signal: controller.signal, onProgress: show });
    } finally {
      done.resolve();
      this.#setImporting(null);
    }
  }

  /** Takes back that it was closed, which React's strict mode has happen for a moment. */
  reopen(): void {
    this.#closed = false;
  }

  /** Ends the worker. What is being imported is given up. */
  close(): void {
    this.#closed = true;
    clearTimeout(this.#idle);
    this.#user = null;
    this.#end();
  }

  #end(): void {
    const { docId } = this.#host;
    const closed = this.#client?.close();
    this.#client = null;
    this.#acquiring = null;
    if (closed) {
      closing.set(docId, closed);
      void closed.finally(() => closing.get(docId) === closed && closing.delete(docId));
    }
  }

  #setImporting(importing: Importing | null): void {
    this.#importing = importing;
    for (const listener of this.#listeners) {
      listener();
    }
    this.#wind();
  }

  #wind(): void {
    clearTimeout(this.#idle);
    if (!this.#user && !this.#importing && this.#client && !this.#closed) {
      this.#idle = setTimeout(() => this.#end(), IDLE_MS);
    }
  }
}
