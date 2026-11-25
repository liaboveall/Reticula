// Which tables a workbook has. A part of the first screen, so there is nothing
// here but what reading the catalogue takes.

/** A table of a workbook. */
export interface TableEntry {
  readonly id: string;
  readonly name: string;
  readonly rows: number;
  readonly cols: number;
  /** Size of its file. */
  readonly bytes: number;
}

export interface Catalog {
  readonly generation: number;
  readonly tables: readonly TableEntry[];
}

/** The catalogue is written to the two in turn, so that one of them is whole. */
export const SLOTS = ["catalog-a", "catalog-b"] as const;

/** A sum of a text that a part of it, or another text, rarely has. */
export function checksum(text: string): number {
  let sum = 0x811c9dc5;
  for (let at = 0; at < text.length; at++) {
    sum = Math.imul(sum ^ text.charCodeAt(at), 0x1000193);
  }
  return sum >>> 0;
}

/** What a slot holds: a checksum and a catalogue. Null when it is not whole. */
export function parseSlot(text: string): Catalog | null {
  const [sum, json = ""] = text.split("\n");
  return sum === `${checksum(json)}` ? JSON.parse(json) : null;
}

/** The directory with the tables of a workbook. */
export async function tablesDir(docId: string, create = false): Promise<FileSystemDirectoryHandle> {
  let dir = await navigator.storage.getDirectory();
  for (const name of ["reticula-v1", "tables", docId]) {
    dir = await dir.getDirectoryHandle(name, { create });
  }
  return dir;
}

/** What each slot holds. */
export function readSlots(dir: FileSystemDirectoryHandle): Promise<(Catalog | null)[]> {
  return Promise.all(
    SLOTS.map((name) =>
      dir
        .getFileHandle(name)
        .then((handle) => handle.getFile())
        .then((file) => file.text())
        .then(parseSlot)
        .catch(() => null),
    ),
  );
}

/** The newer of two. */
export function newest(slots: readonly (Catalog | null)[]): Catalog | null {
  return slots.reduce((a, b) => ((a?.generation ?? -1) < (b?.generation ?? -1) ? b : a), null);
}

/**
 * Whether there are files of tables that a workbook does not have: what is left of
 * an import that was cut short, or one that is under way. Taking the tables of the
 * workbook cleans up.
 */
export async function hasLeftovers(docId: string): Promise<boolean> {
  const dir = await tablesDir(docId).catch(() => null);
  if (!dir) {
    return false;
  }
  const tables = newest(await readSlots(dir))?.tables ?? [];
  let files = 0;
  for await (const name of (dir as unknown as { keys(): AsyncIterable<string> }).keys()) {
    files += Number(name.endsWith(".rtbl"));
  }
  return files > tables.length;
}

/**
 * The tables of a workbook in this browser, in their order. Null when there are
 * some but the catalogue is damaged: the worker then finds them.
 */
export async function readCatalog(docId: string): Promise<readonly TableEntry[] | null> {
  const dir = await tablesDir(docId).catch(() => null);
  return dir ? (newest(await readSlots(dir))?.tables ?? null) : [];
}
