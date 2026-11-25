import { Grid, type GridView, type Selection } from "@reticula/grid";
import { autofitSelectedCols } from "@reticula/grid/arrange";
import { NamedColumns } from "@reticula/grid/columns";
import type {
  ImportReport,
  TableColumnSummary,
  TableInfo,
  TableQuery,
  TableValues,
} from "@reticula/protocol/table";
import type { EditorHandle, TableView } from "../../commands/handle.ts";
import { plainSelectionText } from "../../editor/cell-announcer.ts";
import type { TableLook, TableViews } from "../../editor/table-views.ts";
import type { ZoomSignal } from "../../editor/ZoomControl.tsx";
import { fmtInt, t } from "../../i18n/index.ts";
import { rendererPreference } from "../../renderer.ts";
import { announce } from "../../ui/announcer.ts";
import type { TableClient } from "../client.ts";
import { TableError, type TableFailure } from "../failure.ts";
import { MAX_COPY_CELLS, type RemoteProgress, type ViewInfo } from "../protocol.ts";
import {
  conditionOn,
  filteredBy,
  fitting,
  initialWidth,
  isPlain,
  markTexts,
  parseTarget,
  sortedAlsoBy,
  sortedBy,
  unsorted,
} from "./query.ts";
import { failureOf, type TableSession } from "./session.ts";
import { failureText } from "./text.ts";

/** A table that has crashed so often in a row is only opened again when that is asked for. */
const MOST_CRASHES = 2;
/** A table that has been open for this long has not crashed. */
const SURVIVED_MS = 10_000;
/** Columns that are added up at a time, so that there are some to show early. */
const SUMMED_TOGETHER = 8;
/** Moving the selection asks for texts and totals once it has paused for this long. */
const PAUSE_MS = 120;
/** What has happened is written down once it has paused for this long. */
const SAVE_PAUSE_MS = 500;

/** Something that is open over the table. */
export type TablePart =
  | { readonly kind: "menu"; readonly x: number; readonly y: number }
  | { readonly kind: "values" | "condition"; readonly col: number };

export interface TableState {
  readonly status: "opening" | "open" | "failed";
  readonly failure: TableFailure | null;
  readonly info: TableInfo | null;
  /** The rows that are shown. */
  readonly rows: number;
  /** What was asked for them. */
  readonly query: TableQuery;
  /** What is being done, and how much of it is, from 0 to 1. */
  readonly job: { readonly kind: "query" | "copy" | "fill"; readonly share: number } | null;
  /** How much of it is here, if it is on the server. */
  readonly here: RemoteProgress | null;
  /** Why what was asked last came to nothing. */
  readonly refused: TableFailure | null;
  readonly summaries: ReadonlyMap<number, TableColumnSummary>;
  readonly selection: Selection | null;
  /** All of the text in the active cell. */
  readonly text: string;
  readonly part: TablePart | null;
  readonly frozenCols: number;
  readonly report: ImportReport | null;
  readonly memory: number | null;
}

export interface TableModelOptions {
  readonly id: string;
  readonly docId: string;
  readonly session: TableSession;
  readonly handle: EditorHandle;
  readonly views: TableViews;
  readonly zoom: ZoomSignal;
  /** Whether the panel is open. */
  readonly panel: () => boolean;
  /** Carries out a command about the table as a whole. */
  readonly runWhole: (command: string) => unknown;
  readonly canWhole: (command: string) => boolean;
}

const ALL_ROWS: TableQuery = { filters: [], order: [] };

/** Commands about the column of the active cell, or of the menu that is open. */
const isColumnCommand = (command: string) =>
  /^(?:filterColumn|contextMenu|tableSort|tableFilter)/.test(command);
/** Commands about what is selected, for which there have to be rows. */
const isCellCommand = (command: string) => /^(?:copy|select|autofit|colWidth)/.test(command);

/** A table in view: its grid, and what is asked about it. */
export class TableModel implements TableView {
  readonly #options: TableModelOptions;
  #state: TableState;
  #listeners = new Set<() => void>();
  #grid: Grid | null = null;
  #host: HTMLElement | null = null;
  #client: TableClient | null = null;
  #look: TableLook;
  #columns = new NamedColumns([]);
  #asking: AbortController | null = null;
  /** What is being asked, until it is shown. */
  #asked: TableQuery | null = null;
  #summing: AbortController | null = null;
  #wanted: number[] = [];
  #pauseTimer = 0;
  #saveTimer = 0;
  #crashTimer = 0;
  #column = 0;
  /** What was read out last. */
  #said = "";
  #gone = false;

  constructor(options: TableModelOptions) {
    this.#options = options;
    const { views, docId, id } = options;
    this.#look = views.read(docId, id);
    this.#state = {
      status: "opening",
      failure: null,
      info: null,
      rows: 0,
      query: ALL_ROWS,
      job: null,
      refused: null,
      summaries: new Map(),
      selection: null,
      text: "",
      part: null,
      here: null,
      frozenCols: this.#look.frozenCols,
      report: this.#look.report,
      memory: null,
    };
  }

  get state(): TableState {
    return this.#state;
  }

  getState = (): TableState => this.#state;

  subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  #set(change: Partial<TableState>): void {
    if (!this.#gone) {
      this.#state = { ...this.#state, ...change };
      for (const listener of this.#listeners) {
        listener();
      }
    }
  }

  // --- Coming and going -------------------------------------------------------

  /** Shows the table in an element. Returns how to take it away. */
  attach(host: HTMLElement): () => void {
    const { session, zoom, views, docId, id } = this.#options;
    this.#gone = false;
    this.#host = host;
    const grid = new Grid(host, {
      subscribe: (subscription) => {
        if (this.#client && !this.#client.stopped) {
          this.#client.subscribe(subscription, this.#state.frozenCols);
        }
      },
      readOnly: true,
      clipText: true,
      zoom: true,
      onZoomChange: zoom.set,
      onSelectionChange: (selection) => this.#select(selection),
      onColumnButton: (col, { left, bottom }) => this.#openMenu(col, left, bottom),
      onLocalColumnResize: (sizes) => {
        const widths = new Map(this.#look.widths);
        for (const { col, sizePx } of sizes) {
          if (sizePx === null) {
            widths.delete(col);
          } else {
            widths.set(col, sizePx);
          }
        }
        this.#look = { ...this.#look, widths: [...widths] };
        this.#saveSoon();
        if (sizes.some(({ sizePx }) => sizePx === null)) {
          // The standard width of a column depends on what is in it.
          queueMicrotask(() => {
            const { rows, query } = this.#state;
            this.#grid?.updateSheet(this.#config(rows, query));
          });
        }
      },
      onContextMenu: ({ clientX, clientY }) =>
        this.#set({ part: { kind: "menu", x: clientX, y: clientY } }),
    });
    this.#grid = grid;
    // The status bar has had that of the worksheet.
    zoom.set(this.#look.view?.zoom ?? 1);
    grid.setRenderer(rendererPreference());
    grid.prepareTouch();
    grid.watchFrames(() => this.#saveSoon());
    host.addEventListener("focusin", this.#onFocusIn);
    host.addEventListener("keydown", this.#onKeyDown);
    window.addEventListener("pagehide", this.#save);
    window.addEventListener("pageshow", this.#onPageShow);
    session.use(this);
    views.setActive(docId, id);
    if (this.#look.crashes >= MOST_CRASHES) {
      this.#set({ status: "failed", failure: { code: "crashed" } });
    } else {
      void this.open();
    }
    return () => {
      this.#save();
      this.#gone = true;
      this.#asking?.abort();
      this.#summing?.abort();
      window.clearTimeout(this.#pauseTimer);
      window.clearTimeout(this.#saveTimer);
      window.clearTimeout(this.#crashTimer);
      host.removeEventListener("focusin", this.#onFocusIn);
      host.removeEventListener("keydown", this.#onKeyDown);
      window.removeEventListener("pagehide", this.#save);
      window.removeEventListener("pageshow", this.#onPageShow);
      const client = this.#client;
      this.#client = null;
      this.#grid = null;
      grid.destroy();
      if (session.isUsedBy(this)) {
        session.use(null);
        if (client && !client.stopped) {
          client.closeTable().catch(() => {});
        }
      }
    };
  }

  /** Opens the table, once more if something has come of it before. */
  async open(takeOver?: boolean): Promise<void> {
    const { session, id } = this.#options;
    this.#set({ status: "opening", failure: null });
    try {
      const client = await session.client(takeOver);
      if (this.#gone || !session.isUsedBy(this)) {
        return;
      }
      this.#client = client;
      const { info, view } = await client.open(id);
      if (this.#gone || this.#client !== client) {
        return;
      }
      // Of a question that was asked before something failed, or the page was left.
      const query = fitting(this.#look.query, info.columns);
      this.#set({ info, rows: view.rows, summaries: new Map(), refused: null });
      // It is looked at as it is until all of it is here.
      const isLater = !isPlain(query) && !this.#isHere;
      const isShown =
        !isLater && !isPlain(query) && (await this.#show(query, () => this.#look.view ?? {}));
      if (this.#gone || this.#client !== client) {
        return;
      }
      if (!isShown) {
        this.#set({ query: ALL_ROWS });
        this.#grid?.setSheet(this.#config(view.rows, ALL_ROWS), this.#look.view ?? { zoom: 1 });
      }
      this.#set({ status: "open" });
      if (isLater) {
        void this.ask(query);
      }
      this.#say();
      window.clearTimeout(this.#crashTimer);
      this.#crashTimer = window.setTimeout(() => this.#remember({ crashes: 0 }), SURVIVED_MS);
      this.#refresh();
    } catch (error) {
      if (!this.#gone && this.#state.status === "opening") {
        this.#set({ status: "failed", failure: failureOf(error) });
      }
    }
  }

  acceptPage(page: ArrayBuffer): void {
    this.#grid?.acceptPage(page);
  }

  acceptView(view: ViewInfo, query: TableQuery): void {
    this.#apply(view, query, this.#atTop());
    this.#remember({ query });
    this.#refresh();
  }

  acceptPart(here: RemoteProgress): void {
    this.#set({ here });
    if (here.have === here.total) {
      // Totals are not asked for while that would have all of the table come.
      this.#refresh();
    }
  }

  /** Whether all of the table is here. */
  get #isHere(): boolean {
    const { here } = this.#state;
    return !this.#options.session.isRemote || (here !== null && here.have === here.total);
  }

  fail(failure: TableFailure): void {
    const { code } = failure;
    // The page may be about to be left.
    this.#save();
    this.#client = null;
    this.#asking?.abort();
    this.#summing?.abort();
    window.clearTimeout(this.#crashTimer);
    if (code === "crashed" || code === "memory") {
      this.#remember({ crashes: this.#look.crashes + 1 });
    }
    this.#set({ status: "failed", failure, job: null, part: null });
    announce(failureText(failure), { politeness: "assertive" });
  }

  readonly #onPageShow = (event: PageTransitionEvent): void => {
    if (event.persisted && this.#state.status === "failed") {
      void this.open();
    }
  };

  // --- What is remembered -----------------------------------------------------

  #remember(change: Partial<TableLook>): void {
    const { views, docId, id } = this.#options;
    this.#look = { ...this.#look, ...change };
    views.write(docId, id, this.#look);
  }

  #saveSoon(): void {
    if (this.#saveTimer === 0) {
      this.#saveTimer = window.setTimeout(this.#save, SAVE_PAUSE_MS);
    }
  }

  readonly #save = (): void => {
    window.clearTimeout(this.#saveTimer);
    this.#saveTimer = 0;
    const { status, frozenCols } = this.#state;
    if (status === "open" && this.#grid) {
      this.#remember({ frozenCols, view: this.#grid.view() });
    }
  };

  // --- What is asked ----------------------------------------------------------

  get #names(): readonly string[] {
    return this.#state.info?.columns.map(({ name }) => name) ?? [];
  }

  #config(rows: number, query: TableQuery) {
    const { info, frozenCols } = this.#state;
    const { order, filters } = query;
    this.#columns = new NamedColumns(
      (info?.columns ?? []).map(({ name }, column) => {
        const at = order.findIndex((key) => key.column === column);
        return {
          name,
          sort: at < 0 ? null : order[at]?.descending ? "descending" : "ascending",
          sortOrder: at >= 0 && order.length > 1 ? at + 1 : undefined,
          filtered: filters.some((filter) => filter.column === column),
        };
      }),
    );
    return {
      rows,
      cols: info?.columns.length ?? 0,
      totalRows: info?.rows ?? rows,
      columns: this.#columns,
      frozenCols: Math.min(frozenCols, info?.columns.length ?? 0),
      customCols: [
        ...new Map([
          ...(info?.columns ?? []).map((column, col) => [col, initialWidth(column)] as const),
          ...this.#look.widths,
        ]),
      ],
    };
  }

  /** Has the rows shown that are asked for. What is shown stays until they are there. */
  async #show(query: TableQuery, view: () => Partial<GridView>): Promise<boolean> {
    const client = this.#client;
    if (!client) {
      return false;
    }
    this.#asking?.abort();
    const asking = new AbortController();
    this.#asking = asking;
    this.#asked = query;
    this.#summing?.abort();
    this.#set({ job: { kind: "query", share: 0 }, refused: null });
    try {
      const found = await client.query(query, {
        signal: asking.signal,
        onProgress: ({ share, have }) =>
          this.#asking === asking &&
          this.#set({ job: { kind: have === undefined ? "query" : "fill", share } }),
      });
      if (this.#asking !== asking || this.#gone) {
        return false;
      }
      this.#set({ job: null });
      this.#apply(found, query, view());
      return true;
    } catch (error) {
      if (this.#asking === asking) {
        this.#set({ job: null });
        const failure = failureOf(error);
        const reason = failureText(failure);
        if (reason) {
          this.#set({ refused: failure });
          announce(reason, { politeness: "assertive" });
        }
        this.#refresh();
      }
      return false;
    } finally {
      if (this.#asking === asking) {
        this.#asking = null;
        this.#asked = null;
      }
    }
  }

  /** Shows rows, with the pages that came with them. */
  #apply({ rows, pages }: ViewInfo, query: TableQuery, view: Partial<GridView>): void {
    this.#grid?.refresh(this.#config(rows, query), view, pages);
    this.#set({ rows, query, summaries: new Map() });
  }

  /** The first row, as far to the right as it is now. */
  #atTop(): Partial<GridView> {
    const { col = 0, colFraction = 0 } = this.#grid?.view() ?? {};
    const active = { row: 0, col: this.#grid?.selection?.col ?? col };
    return { col, colFraction, selection: { anchor: active, focus: active, scope: "cells" } };
  }

  /** What was asked last, which may not be shown yet. What is asked next goes by it. */
  get asked(): TableQuery {
    return this.#asked ?? this.#state.query;
  }

  /** Asks something else of the table. Returns whether that is shown then. */
  async ask(query: TableQuery): Promise<boolean> {
    const before = this.#state.query;
    const isShown = await this.#show(query, () => this.#atTop());
    if (isShown) {
      this.#remember({ query });
      this.#announceView(before, query);
      this.#refresh();
    }
    return isShown;
  }

  /** Says what has come of a question: the order if that is another one now, else the rows. */
  #announceView(before: TableQuery, query: TableQuery): void {
    const { rows, info } = this.#state;
    if (query.order === before.order) {
      announce(
        t("{n} of {total} rows shown", { n: fmtInt(rows), total: fmtInt(info?.rows ?? rows) }),
      );
      return;
    }
    const keys = query.order.map(({ column, descending }) => {
      const name = this.#names[column] ?? "";
      return descending ? t("{name}, descending", { name }) : t("{name}, ascending", { name });
    });
    announce(
      keys.length > 0
        ? t("Sorted by {keys}", { keys: keys.join(t("; |separator")) })
        : t("Not sorted any more"),
    );
  }

  /** The values of a column that contain a text, as far as the other conditions leave them. */
  values(column: number, search: string, limit: number, signal: AbortSignal): Promise<TableValues> {
    return this.#client
      ? this.#client.values(column, search, limit, { signal })
      : Promise.reject(new TableError("closed"));
  }

  clearRefused(): void {
    this.#set({ refused: null });
  }

  cancel(): void {
    this.#asking?.abort();
  }

  /** Has totals of columns shown that are not known yet, the ones given first. */
  wantSummaries(columns: readonly number[]): void {
    const { summaries } = this.#state;
    const isNew = (column: number) => !summaries.has(column);
    this.#wanted = [...new Set([...columns.filter(isNew), ...this.#wanted.filter(isNew)])];
    if (!this.#summing && !this.#asking && this.#wanted.length > 0 && this.#isHere) {
      void this.#sumUp();
    }
  }

  async #sumUp(): Promise<void> {
    const client = this.#client;
    const summing = new AbortController();
    this.#summing = summing;
    try {
      while (client && this.#client === client && this.#wanted.length > 0) {
        const columns = this.#wanted.slice(0, SUMMED_TOGETHER);
        const { columns: found } = await client.summary(columns, { signal: summing.signal });
        if (summing.signal.aborted) {
          return;
        }
        this.#wanted = this.#wanted.filter((column) => !columns.includes(column));
        const summaries = new Map(this.#state.summaries);
        for (const summary of found) {
          summaries.set(summary.column, summary);
        }
        this.#set({ summaries });
      }
      const { table } = (await client?.memory()) ?? {};
      this.#set({ memory: table ?? null });
    } catch {
      // They are asked for again when they are to be shown again.
      this.#wanted = [];
    } finally {
      if (this.#summing === summing) {
        this.#summing = null;
      }
    }
  }

  /** Asks for what is to be shown about what is in view now. */
  #refresh(): void {
    window.clearTimeout(this.#pauseTimer);
    this.#pauseTimer = window.setTimeout(() => {
      const { selection, info } = this.#state;
      if (!selection || !info || this.#gone) {
        return;
      }
      const selected = selection.scope === "cols" ? this.selectedColumns() : [];
      const all = this.#options.panel() ? info.columns.map((_, column) => column) : [];
      this.wantSummaries([...selected, ...all]);
      const { row, col } = selection;
      this.#client?.text(row, col).then(
        (text) => {
          const now = this.#state.selection;
          if (now?.row === row && now.col === col) {
            this.#set({ text });
          }
        },
        () => {},
      );
    }, PAUSE_MS);
  }

  /** To be called when the panel was opened. */
  panelOpened(): void {
    this.#refresh();
  }

  // --- The selection ----------------------------------------------------------

  #select(selection: Selection | null): void {
    const before = this.#state.selection;
    const isSame = selection?.row === before?.row && selection?.col === before?.col;
    this.#column = selection?.col ?? this.#column;
    this.#set({
      selection,
      text: selection?.text ?? (isSame ? this.#state.text : ""),
    });
    this.#say();
    this.#refresh();
  }

  selectedColumns(): number[] {
    return (this.#grid?.selectedLines("col") ?? []).flatMap(({ first, end }) =>
      Array.from({ length: end - first }, (_, at) => first + at),
    );
  }

  #say(): void {
    const { selection, status } = this.#state;
    const isElsewhere =
      document.activeElement?.closest("aside") != null ||
      document.querySelector("dialog:modal") !== null;
    // A text that has not come yet is not blank.
    if (!selection || status !== "open" || selection.text === null || isElsewhere) {
      return;
    }
    const text = plainSelectionText(selection, (col) => this.#names[col] ?? "");
    if (text !== this.#said) {
      this.#said = text;
      announce(text, { isPlace: true });
    }
  }

  readonly #onFocusIn = (): void => {
    // Whoever comes back to the grid wants to know where that is.
    this.#said = "";
    this.#say();
  };

  readonly #onKeyDown = (event: KeyboardEvent): void => {
    const { key, altKey, shiftKey, ctrlKey, metaKey } = event;
    if (key.toLowerCase() === "c" && (ctrlKey || metaKey) && !altKey && !shiftKey) {
      event.preventDefault();
      void this.#copy();
    }
  };

  async #copy(): Promise<boolean> {
    const grid = this.#grid;
    const client = this.#client;
    const range = grid?.selectionRange;
    if (!grid || !client || !range) {
      return false;
    }
    const rows = range.endRow - range.firstRow;
    const cols = range.endCol - range.firstCol;
    if (rows * cols > MAX_COPY_CELLS) {
      this.#refuse({ code: "tooLarge" });
      return true;
    }
    this.#set({ job: { kind: "copy", share: 0 }, refused: null });
    const copied = client.copy({ firstRow: range.firstRow, rows, firstCol: range.firstCol, cols });
    const isText = this.#state.info?.columns
      .slice(range.firstCol, range.endCol)
      .map(({ type }) => type.kind === "text");
    const part = (type: "tsv" | "html", mime: string) =>
      copied.then((cells) => {
        const text = type === "html" ? markTexts(cells.html, isText ?? []) : cells.tsv;
        return new Blob([text], { type: mime });
      });
    try {
      // While it is being worked out: the browser only takes it for a moment after a key.
      await navigator.clipboard.write([
        new ClipboardItem({
          "text/plain": part("tsv", "text/plain"),
          "text/html": part("html", "text/html"),
        }),
      ]);
      grid.setClipboardRange(range);
      announce(t("{n} cells copied", { n: fmtInt(rows * cols) }));
    } catch (error) {
      const failure = await copied.then(() => null, failureOf);
      if (failure) {
        this.#refuse(failure);
      } else {
        this.#options.handle.reject({
          say: () => t("The browser does not let the page write to the clipboard."),
        });
        console.warn(error);
      }
    } finally {
      this.#set({ job: this.#asking ? { kind: "query", share: 0 } : null });
    }
    return true;
  }

  #refuse(failure: TableFailure): void {
    this.#set({ refused: failure });
    announce(failureText(failure), { politeness: "assertive" });
  }

  // --- Commands ---------------------------------------------------------------

  grid(): Grid | null {
    return this.#grid;
  }

  focus(): void {
    this.#host?.focus({ preventScroll: true });
  }

  /** The column that commands are about. Without rows there is no active cell. */
  get column(): number {
    return this.#column;
  }

  openPart(part: TablePart | null): void {
    this.#set({ part });
  }

  /** Opens the menu of a column, which is selected in its first cell that is in view then. */
  #openMenu(col: number, x: number, y: number): void {
    const grid = this.#grid;
    const active = grid?.selection;
    if (grid && active?.col !== col) {
      grid.select({ row: active?.row ?? 0, col });
    }
    this.#column = col;
    this.focus();
    this.#set({ part: { kind: "menu", x, y } });
  }

  /** Shows a column, and selects it. */
  goToColumn(col: number): void {
    this.#grid?.selectCols(col, col);
    this.focus();
  }

  can(command: string): boolean {
    const { status, selection, query, job, frozenCols } = this.#state;
    const col = this.column;
    switch (command) {
      case "tableSortClear":
        return query.order.length > 0;
      case "tableFilterClear":
        return conditionOn(query, col) !== null;
      case "clearFilters":
        return query.filters.length > 0;
      case "tableCancel":
        return (job && job.kind !== "copy") || this.#options.session.importing !== null;
      case "freeze:none":
        return frozenCols > 0;
      case "freeze:col1":
        return status === "open" && frozenCols !== 1;
      case "freeze:active":
        return status === "open" && col > 0;
    }
    if (isColumnCommand(command)) {
      return status === "open";
    }
    return isCellCommand(command)
      ? status === "open" && selection !== null
      : this.#options.canWhole(command);
  }

  run(command: string): unknown {
    const grid = this.#grid;
    const { selection } = this.#state;
    const query = this.asked;
    const col = this.column;
    if (!grid) {
      return false;
    }
    const range = grid.selectionRange;
    switch (command) {
      case "copy":
        return this.#copy();
      case "selectRows":
        return range && grid.selectRows(range.firstRow, range.endRow - 1);
      case "selectCols":
        return range && grid.selectCols(range.firstCol, range.endCol - 1);
      case "selectAll":
        return grid.selectAll();
      case "autofitCols":
        return autofitSelectedCols(grid);
      case "colWidth":
        return this.#options.handle.openDialog({ kind: "size", axis: "col" });
      case "filterColumn":
      case "contextMenu": {
        grid.reveal(selection?.row ?? 0, col);
        const { left, bottom } = grid.columnButtonRect(col);
        return this.#openMenu(col, left, bottom);
      }
      case "freeze:none":
        return this.#freeze(0);
      case "freeze:col1":
        return this.#freeze(1);
      case "freeze:active":
        return this.#freeze(col);
      case "tableSortAscending":
        return this.ask(sortedBy(query, col, false));
      case "tableSortDescending":
        return this.ask(sortedBy(query, col, true));
      case "tableSortAdd":
        return this.ask(sortedAlsoBy(query, col));
      case "tableSortClear":
        return this.ask(unsorted(query));
      case "tableFilterValues":
        return this.#set({ part: { kind: "values", col } });
      case "tableFilterCondition":
        return this.#set({ part: { kind: "condition", col } });
      case "tableFilterClear":
        return this.ask(filteredBy(query, col, null));
      case "clearFilters":
        return this.ask({ ...query, filters: [] });
      case "tableCancel":
        this.cancel();
        return this.#options.session.importing?.cancel();
      case "zoomIn":
        return grid.zoomIn();
      case "zoomOut":
        return grid.zoomOut();
    }
    const percent = /^zoom:(\d+)$/.exec(command)?.[1];
    return percent ? grid.setZoom(Number(percent) / 100) : this.#options.runWhole(command);
  }

  #freeze(cols: number): void {
    const { rows, query } = this.#state;
    this.#set({ frozenCols: cols });
    this.#grid?.updateSheet(this.#config(rows, query));
    this.#saveSoon();
    announce(cols === 0 ? t("No columns frozen") : t("Columns frozen: {n}", { n: fmtInt(cols) }));
  }

  goTo(text: string): string | null {
    const grid = this.#grid;
    const target = parseTarget(text, this.#names, this.#state.rows);
    if (!grid || !target) {
      return t("“{name}” is neither the number of a row nor a column.", { name: text.trim() });
    }
    const { row, col } = target;
    if (row === null) {
      grid.selectCols(col ?? 0, col ?? 0);
    } else {
      grid.jumpTo(row, col ?? grid.selection?.col ?? 0, false);
    }
    return null;
  }
}
