import { deleteTable, type TableRecord } from "@reticula/protocol/collab-api";
import type { TableEntry } from "./catalog.ts";

/** What the files of tables are named after their identifiers. */
export const TABLE_EXTENSION = ".rtbl";
/** Where the copies of tables on the server are, and the last lists of them. */
const COPIES = "tables-remote";
/** Where what is to be uploaded is. Not among the workbooks, which may have any name. */
const QUEUE = "tables-queue";
/** What tabs tell each other on, and that somebody has signed out in one of them. */
const TABS_CHANNEL = "reticula:locks";
export const SIGNED_OUT = "signedOut";
/** That a table has been uploaded, and is the file with a digest. */
export const UPLOADED = "uploaded";
/** Files in use are tried for so long, and so often. */
const PATIENCE_MS = 3_000;
const RETRY_MS = 100;

/** Whoever holds this lock reads and changes the tables of a workbook. */
export function tablesLock(docId: string): string {
  return `reticula:tables:${docId}`;
}

/** A directory at the top of what is kept in this browser, if there is one. */
async function topDir(name: string, create = false): Promise<FileSystemDirectoryHandle> {
  const opfs = await navigator.storage.getDirectory();
  const root = await opfs.getDirectoryHandle("reticula-v1", { create });
  return root.getDirectoryHandle(name, { create });
}

/** What there is in a directory. */
export async function namesIn(dir: FileSystemDirectoryHandle): Promise<string[]> {
  const names: string[] = [];
  for await (const name of (dir as unknown as { keys(): AsyncIterable<string> }).keys()) {
    names.push(name);
  }
  return names;
}

/** Removes something from a directory. Whether it is gone. */
export function remove(dir: FileSystemDirectoryHandle, name: string): Promise<boolean> {
  return dir.removeEntry(name).then(
    () => true,
    (error: Error) => error.name === "NotFoundError",
  );
}

/** Tries something for a while, until it comes out: a file may be in use by a page that closes. */
async function patiently(run: () => Promise<boolean>): Promise<boolean> {
  for (let waited = 0; waited < PATIENCE_MS; waited += RETRY_MS) {
    if (await run()) {
      return true;
    }
    await new Promise((resolve) => setTimeout(resolve, RETRY_MS));
  }
  return false;
}

/** Removes some things from a directory, also those that are in use a little longer. */
export function removeAll(
  dir: FileSystemDirectoryHandle,
  names: readonly string[],
): Promise<boolean> {
  return patiently(async () =>
    (await Promise.all(names.map((name) => remove(dir, name)))).every(Boolean),
  );
}

/** Writes a file that is there from one moment to the next, or not at all. */
export async function writeText(
  dir: FileSystemDirectoryHandle,
  name: string,
  text: string,
): Promise<void> {
  const handle = await dir.getFileHandle(name, { create: true });
  const writable = await handle.createWritable();
  await writable.write(text);
  await writable.close();
}

/** Runs something that nothing else with that name runs beside. */
function alone<T>(name: string, run: () => Promise<T>): Promise<T> {
  return navigator.locks?.request ? navigator.locks.request(name, run) : run();
}

/** Changes what is in the queue. */
const changeQueue = <T>(run: () => Promise<T>) => alone("reticula:table-queue", run);

/** A table that is being uploaded, or is to be. */
export interface QueuedUpload {
  readonly userId: string;
  /** The workbook on the server. */
  readonly docId: string;
  /** What the table is there. */
  readonly entry: TableEntry;
  /** Empty until it was worked out. */
  readonly sha256: string;
  /** Whether the server knows of it. */
  readonly isDeclared?: boolean;
  readonly queuedAt: number;
  /** The table of a workbook in this browser that it is. Its file is in the queue otherwise. */
  readonly from: { readonly docId: string; readonly id: string } | null;
}

/** Where what is to be uploaded is. */
export function queueDir(create = false): Promise<FileSystemDirectoryHandle> {
  return topDir(QUEUE, create);
}

/** What is to be uploaded from this browser, in order. */
export async function queuedUploads(): Promise<QueuedUpload[]> {
  const queued: QueuedUpload[] = [];
  const dir = await queueDir().catch(() => null);
  for (const name of dir ? await namesIn(dir) : []) {
    try {
      if (name.endsWith(".json")) {
        const file = await dir?.getFileHandle(name);
        queued.push(JSON.parse((await (await file?.getFile())?.text()) ?? ""));
      }
    } catch {
      // It was taken out meanwhile, or there was no room to write it.
    }
  }
  return queued.sort((a, b) => a.queuedAt - b.queuedAt);
}

/** An upload that is in the queue. */
export async function queuedUpload(id: string): Promise<QueuedUpload | undefined> {
  return (await queuedUploads()).find(({ entry }) => entry.id === id);
}

/** Writes down what is to be uploaded. */
export async function enqueue(upload: QueuedUpload): Promise<void> {
  const dir = await queueDir(true);
  await changeQueue(() => writeText(dir, `${upload.entry.id}.json`, JSON.stringify(upload)));
}

/** Changes an upload, unless it was given up. */
export async function requeue(
  id: string,
  change: (upload: QueuedUpload) => QueuedUpload,
): Promise<void> {
  const dir = await queueDir(true);
  await changeQueue(async () => {
    const upload = await queuedUpload(id);
    if (upload) {
      await writeText(dir, `${id}.json`, JSON.stringify(change(upload)));
    }
  });
}

/** Takes an upload out of the queue, and the file it is of if that is there. */
export async function dequeue(id: string): Promise<void> {
  const dir = await queueDir(true);
  await changeQueue(async () => {
    await remove(dir, `${id}.json`);
    await remove(dir, id + TABLE_EXTENSION);
  });
}

/** Gives up uploading a table to a workbook. The server may know of it. */
export async function giveUp(docId: string, id: string): Promise<void> {
  await dequeue(id);
  await deleteTable(docId, id).catch(() => {});
}

/** Removes what is in the queue without being queued: imports that were cut short. */
export async function sweepQueue(): Promise<void> {
  const dir = await queueDir(true);
  await changeQueue(async () => {
    const ids = (await queuedUploads()).map(({ entry }) => entry.id);
    for (const name of await namesIn(dir)) {
      if (!ids.includes(name.replace(/\..*/, ""))) {
        // One that is being written is in use, and stays.
        await remove(dir, name);
      }
    }
  });
}

/**
 * Deletes the tables of a workbook, unless they are open somewhere, or are still
 * to be uploaded. Whether there are none now.
 */
export async function deleteTables(docId: string): Promise<boolean> {
  if ((await queuedUploads()).some(({ from }) => from?.docId === docId)) {
    return false;
  }
  const removeTables = async (): Promise<boolean> => {
    const tables = await topDir("tables").catch(() => null);
    try {
      await tables?.removeEntry(docId, { recursive: true });
    } catch (error) {
      if ((error as Error).name !== "NotFoundError") {
        throw error;
      }
    }
    return true;
  };
  return navigator.locks?.request
    ? navigator.locks.request(tablesLock(docId), { ifAvailable: true }, (lock) =>
        lock ? removeTables() : false,
      )
    : removeTables();
}

/** The directory with somebody's copies of tables on the server. */
export async function copiesDir(
  userId: string,
  create = false,
): Promise<FileSystemDirectoryHandle> {
  return (await topDir(COPIES, create)).getDirectoryHandle(userId, { create });
}

/** The list of the tables of a workbook that somebody had last: its version, and what is on it. */
export async function lastList(
  userId: string,
  docId: string,
): Promise<[string | undefined, readonly TableRecord[]]> {
  const handle = await (await copiesDir(userId)).getFileHandle(`${docId}.json`);
  return JSON.parse(await (await handle.getFile()).text());
}

/** The directories with copies of tables, except those of somebody. */
async function copiesOfOthers(userId?: string): Promise<[string, FileSystemDirectoryHandle][]> {
  const copies = await topDir(COPIES).catch(() => null);
  const names = copies ? await namesIn(copies) : [];
  return Promise.all(
    names.filter((name) => name !== userId).map(async (name) => [name, await copiesDir(name)]),
  );
}

/**
 * Deletes the copies of tables on the server, except those of somebody, as far as
 * they are not in use. Whether there are none left.
 */
export async function dropCopies(exceptUserId?: string): Promise<boolean> {
  const dropped = (await copiesOfOthers(exceptUserId)).map(async ([, dir]) =>
    // One by one: a file that is in use keeps the directory, and only itself.
    Promise.all((await namesIn(dir)).map((name) => remove(dir, name))),
  );
  return (await Promise.all(dropped)).flat().every(Boolean);
}

/** Deletes the copies of the tables of a workbook, which may not be had any more. */
export async function dropCopiesOf(docId: string): Promise<void> {
  for (const [userId, dir] of await copiesOfOthers()) {
    const [, tables = []] = await lastList(userId, docId).catch(() => []);
    // Without the list nobody would know of those that are left.
    if (
      await removeAll(
        dir,
        tables.map(({ sha256 }) => sha256),
      )
    ) {
      await remove(dir, `${docId}.json`);
    }
  }
}

/**
 * Deletes the copies of tables on the server, except those of somebody. Without
 * one it is to be called when somebody signs out: whatever is done for them in
 * this browser ends.
 */
export async function deleteCopies(exceptUserId?: string): Promise<void> {
  if (exceptUserId === undefined && typeof BroadcastChannel === "function") {
    const channel = new BroadcastChannel(TABS_CHANNEL);
    channel.postMessage({ kind: SIGNED_OUT });
    channel.close();
  }
  await patiently(() => dropCopies(exceptUserId));
}
