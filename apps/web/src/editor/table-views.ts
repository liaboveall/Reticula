import type { GridView } from "@reticula/grid";
import type { ImportReport, TableQuery } from "@reticula/protocol/table";
import { isPosition, isRecord, parseView, StoredDocuments } from "./view-state.ts";

/** Where it is kept, in this browser, how the data tables of workbooks were left. */
const STORAGE_KEY = "reticula:table-view:v1";
/** What is remembered of one table, at most: a filter may name any number of values. */
const MOST_CHARACTERS = 100_000;

/** How a data table was left. */
export interface TableLook {
  readonly query: TableQuery;
  readonly widths: readonly (readonly [col: number, px: number])[];
  readonly frozenCols: number;
  readonly view: GridView | null;
  /** What became of the file it comes from. */
  readonly report: ImportReport | null;
  /** How often in a row opening it has come to a crash. */
  readonly crashes: number;
}

const ALL_ROWS: TableQuery = { filters: [], order: [] };

export const FRESH_LOOK: TableLook = {
  query: ALL_ROWS,
  widths: [],
  frozenCols: 0,
  view: null,
  report: null,
  crashes: 0,
};

interface StoredDocument {
  readonly id: string;
  /** The table in view. */
  readonly active: string | null;
  readonly tables: Readonly<Record<string, unknown>>;
}

const isWidth = (value: unknown): value is readonly [number, number] =>
  Array.isArray(value) && value.length === 2 && value.every(isPosition);

const listOf = (value: unknown): readonly unknown[] => (Array.isArray(value) ? value : []);

function parseQuery(value: unknown): TableQuery {
  if (!isRecord(value)) {
    return ALL_ROWS;
  }
  // Whether the conditions are for their columns is for the engine to say.
  const isFilter = (filter: unknown): filter is TableQuery["filters"][number] =>
    isRecord(filter) &&
    isPosition(filter.column) &&
    isRecord(filter.condition) &&
    typeof filter.condition.kind === "string" &&
    (filter.condition.kind !== "oneOf" || Array.isArray(filter.condition.keys));
  const isKey = (key: unknown): key is TableQuery["order"][number] =>
    isRecord(key) && isPosition(key.column) && typeof key.descending === "boolean";
  return {
    filters: listOf(value.filters).filter(isFilter),
    order: listOf(value.order).filter(isKey),
  };
}

/** How a table was left as it was stored, or as fresh as what there is of it. */
export function parseLook(value: unknown): TableLook {
  if (!isRecord(value)) {
    return FRESH_LOOK;
  }
  const { report } = value;
  return {
    query: parseQuery(value.query),
    widths: listOf(value.widths).filter(isWidth),
    frozenCols: isPosition(value.frozenCols) ? value.frozenCols : 0,
    view: parseView(value.view),
    report:
      isRecord(report) && Array.isArray(report.columns) && Array.isArray(report.skipped)
        ? (report as unknown as ImportReport)
        : null,
    crashes: isPosition(value.crashes) ? value.crashes : 0,
  };
}

/** How tables were left. */
export class TableViews {
  #documents: StoredDocuments<StoredDocument>;

  constructor(storage?: () => Storage) {
    this.#documents = new StoredDocuments(
      STORAGE_KEY,
      ({ active, tables }) =>
        isRecord(tables) ? { active: typeof active === "string" ? active : null, tables } : null,
      storage,
    );
  }

  /** The table that was in view of a workbook, if one was. */
  active(documentId: string): string | null {
    return this.#find(documentId).active;
  }

  setActive(documentId: string, active: string | null): void {
    const stored = this.#find(documentId);
    if (stored.active !== active) {
      this.#documents.write({ ...stored, active });
    }
  }

  read(documentId: string, table: string): TableLook {
    const { tables } = this.#find(documentId);
    return parseLook(Object.hasOwn(tables, table) ? tables[table] : null);
  }

  /** Remembers how a table is. Those of other tables may have changed in another tab. */
  write(documentId: string, table: string, look: TableLook): void {
    const stored = this.#find(documentId);
    const isSmall = (kept: TableLook) => JSON.stringify(kept).length <= MOST_CHARACTERS;
    const kept = [look, { ...look, query: ALL_ROWS }, { ...look, query: ALL_ROWS, report: null }];
    this.#documents.write({
      ...stored,
      tables: { ...stored.tables, [table]: kept.find(isSmall) ?? FRESH_LOOK },
    });
  }

  /** Forgets a table. */
  forget(documentId: string, table: string): void {
    const stored = this.#find(documentId);
    const { [table]: _, ...tables } = stored.tables;
    const active = stored.active === table ? null : stored.active;
    this.#documents.write({ ...stored, active, tables });
  }

  #find(id: string): StoredDocument {
    return this.#documents.find(id) ?? { id, active: null, tables: {} };
  }
}
