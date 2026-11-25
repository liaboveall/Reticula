// What the rest of the app has to know of open tables, without loading what
// they need.

import type { TableFailure } from "./failure.ts";

/** What tests see of open tables. */
export interface TableStatus {
  readonly docId: string;
  /** `open`: a table is. `failed`: the worker is gone. */
  readonly state: "idle" | "open" | "yielded" | "failed" | "closed";
  readonly failure: TableFailure | null;
  readonly busy: boolean;
  readonly pages: number;
}

/** The tables of a workbook, while something holds them open. */
export interface OpenTables {
  /** Whether something is being imported, which would be lost. */
  readonly busy: boolean;
  /** Lets go of the files and of the lock. */
  close(): Promise<void>;
  status(): TableStatus;
  crash(): void;
  setMemoryLimit(bytes: number | null): Promise<void>;
}

const open = new Set<OpenTables>();
let shown: OpenTables | null = null;
let last: OpenTables | null = null;

/** Says that tables are open. The function returned says that they no longer are. */
export function tablesOpened(tables: OpenTables): () => void {
  open.add(tables);
  last = tables;
  return () => open.delete(tables);
}

/** Says that one of them is shown. */
export function tableShown(tables: OpenTables): void {
  shown = tables;
}

/** The tables one of which was shown last, or else those opened last, whatever became of them. */
export function lastTables(): OpenTables | null {
  return shown ?? last;
}

/** Whether a table is being imported. */
export function tablesBusy(): boolean {
  return [...open].some((tables) => tables.busy);
}

/**
 * Closes all tables, as before an update. One that is being imported is given up:
 * ask before, see `tablesBusy`.
 */
export async function closeTables(): Promise<void> {
  await Promise.all([...open].map((tables) => tables.close()));
}
