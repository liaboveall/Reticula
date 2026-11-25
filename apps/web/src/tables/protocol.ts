import type { TableRecord } from "@reticula/protocol/collab-api";
import type {
  CsvOptions,
  CsvSniffed,
  ImportReport,
  ParquetInfo,
  TableInfo,
  TableQuery,
  TableSummary,
  TableValues,
} from "@reticula/protocol/table";
import type { TableEntry } from "./catalog.ts";
import type { TextEncoding } from "./csv-input.ts";
import type { TableFailure } from "./failure.ts";
import type { RemoteTarget } from "./remote-api.ts";
import type { UploadProgress } from "./uploads.ts";

// What the page and the worker behind tables say to each other. What the engine
// takes and gives is generated from it; here is what the worker adds.

/** Cells to hold pages of, as the grid asks for them, and those that are on screen. */
export interface TableSubscription {
  readonly epoch: number;
  readonly resend: boolean;
  readonly firstRow: number;
  readonly rowCount: number;
  readonly firstCol: number;
  readonly colCount: number;
  readonly visible: {
    readonly firstRow: number;
    readonly endRow: number;
    readonly firstCol: number;
    readonly endCol: number;
  };
}

export interface CellRange {
  readonly firstRow: number;
  readonly rows: number;
  readonly firstCol: number;
  readonly cols: number;
}

/** Most cells that can be copied at once, as in a sheet. */
export const MAX_COPY_CELLS = 200_000;

/** What can be asked for, and what the answer is. */
export interface TableRequests {
  /**
   * Takes the tables of the workbook. With `takeOver` from whoever has them. Those
   * on the server, where `remote` says, can be had by any number.
   */
  acquire: {
    ask: { readonly takeOver: boolean; readonly remote?: RemoteTarget | undefined };
    answer: readonly TableEntry[];
  };
  open: {
    ask: {
      readonly id: string;
      /** What the server has listed of it. */
      readonly record?: Pick<TableRecord, "bytes" | "sha256" | "status"> | undefined;
    };
    answer: OpenedTable;
  };
  closeTable: { ask: object; answer: null };
  query: { ask: { readonly query: TableQuery }; answer: ViewInfo };
  summary: { ask: { readonly columns: readonly number[] }; answer: TableSummary };
  values: {
    ask: { readonly column: number; readonly search: string; readonly limit: number };
    answer: TableValues;
  };
  text: { ask: { readonly row: number; readonly col: number }; answer: string };
  copy: { ask: { readonly range: CellRange }; answer: CopiedCells };
  rename: { ask: { readonly id: string; readonly name: string }; answer: null };
  /** Puts a table where so many others are before it. */
  move: { ask: { readonly id: string; readonly position: number }; answer: null };
  delete: { ask: { readonly id: string }; answer: null };
  export: { ask: { readonly id: string }; answer: File };
  sniffCsv: { ask: CsvSource; answer: SniffedText };
  importCsv: { ask: CsvSource & { readonly name: string }; answer: ImportedTable };
  inspectParquet: { ask: { readonly file: Blob }; answer: ParquetInfo };
  importParquet: {
    ask: {
      readonly file: Blob;
      readonly name: string;
      /** The columns to take, as `inspectParquet` lists them. All when not given. */
      readonly columns?: readonly number[] | undefined;
    };
    answer: ImportedTable;
  };
  importFile: { ask: { readonly file: Blob; readonly name: string }; answer: ImportedTable };
  sample: {
    ask: { readonly name: string; readonly rows: number; readonly seed: number };
    answer: ImportedTable;
  };
  /** Uploads what somebody has in the queue. */
  upload: { ask: { readonly userId: string; readonly afresh: boolean }; answer: null };
  memoryLimit: { ask: { readonly bytes: number | null }; answer: null };
  /** Bytes of memory the engine takes for the table that is open, and in all. */
  memory: { ask: object; answer: { readonly table: number; readonly total: number } };
  crash: { ask: object; answer: null };
  close: { ask: object; answer: null };
}

export type TableRequestKind = keyof TableRequests;

export interface OpenedTable {
  /** What the pages of the table have for a sheet. */
  readonly sheet: number;
  readonly view: ViewInfo;
  readonly info: TableInfo;
}

/** The rows that are shown, and in which order. */
export interface ViewInfo {
  /** Subscriptions are to one view, and those to another are of no use. */
  readonly id: number;
  readonly rows: number;
  /** The pages that are on screen when the view is shown from its start. */
  readonly pages: readonly ArrayBuffer[];
}

export interface CopiedCells {
  readonly tsv: string;
  readonly html: string;
}

export interface CsvSource {
  readonly file: Blob;
  /** To be found out when not given: what its first bytes say, or UTF-8. */
  readonly encoding?: TextEncoding | undefined;
  readonly options: CsvOptions;
}

export interface SniffedText extends CsvSniffed {
  readonly encoding: TextEncoding;
}

export interface ImportedTable {
  readonly entry: TableEntry;
  /** What became of the file it comes from, unless it was one of a table. */
  readonly report: ImportReport | null;
}

export type TableCommand =
  | {
      [K in TableRequestKind]: {
        readonly kind: K;
        readonly id: number;
        readonly ask: TableRequests[K]["ask"];
      };
    }[TableRequestKind]
  | {
      readonly kind: "subscribe";
      readonly view: number;
      readonly frozenCols: number;
      readonly subscription: TableSubscription;
    }
  /** Gives up what was asked for. */
  | { readonly kind: "cancel"; readonly id: number };

/** How far something has got. */
export interface TableProgress {
  /** From 0 to 1. */
  readonly share: number;
  readonly rows?: number | undefined;
  /** Which time the file is being read: the types of its columns may turn out to be others. */
  readonly pass?: number | undefined;
  /** Bytes of the table that are here, while not all of them are. */
  readonly have?: number | undefined;
}

/** How much of the open table is here, which is on the server. */
export interface RemoteProgress {
  readonly have: number;
  readonly total: number;
  /** Whether the server could not be had when it was asked last. It is asked again. */
  readonly isStalled: boolean;
}

export type TableMessage =
  | { readonly kind: "answer"; readonly id: number; readonly answer: unknown }
  | { readonly kind: "refused"; readonly id: number; readonly failure: TableFailure }
  | { readonly kind: "progress"; readonly id: number; readonly progress: TableProgress }
  | { readonly kind: "catalog"; readonly tables: readonly TableEntry[] }
  | { readonly kind: "remote"; readonly remote: RemoteProgress }
  | { readonly kind: "upload"; readonly upload: UploadProgress }
  /** Another tab has the tables now; `evicted`: it took them. */
  | { readonly kind: "yielded"; readonly evicted: boolean }
  /** The open table cannot be read any further. */
  | { readonly kind: "tableFailed"; readonly failure: TableFailure }
  /** The worker cannot go on. */
  | { readonly kind: "failed"; readonly failure: TableFailure };

/** What a table worker is started with, in its name. */
export interface WorkerName {
  readonly docId: string;
  readonly tabId: string;
}
