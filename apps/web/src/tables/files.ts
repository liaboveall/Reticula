import type { TableFile } from "@reticula/protocol/table";
import { FILE_IN_USE_ERRORS, type SyncAccessHandleLike } from "../engine/storage.ts";
import { TableError, type TableFailure, type TableFailureCode } from "./failure.ts";

interface WorkerFileHandle extends FileSystemFileHandle {
  createSyncAccessHandle?: () => Promise<SyncAccessHandleLike>;
}

/** Which only workers have. */
declare class FileReaderSync {
  readAsArrayBuffer(blob: Blob): ArrayBuffer;
}

const RETRY_MS = 25;
/** How much of a picked file is read at once, at least. */
const WINDOW_BYTES = 1 << 16;

/** What an exception of the file system means. */
export function fileFailure(error: unknown): TableFailure {
  if (error instanceof TableError) {
    return error.failure;
  }
  const { name = "", message = "" } = (error ?? {}) as Partial<Error>;
  let code: TableFailureCode = "io";
  if (/quota|disk full/i.test(name + message)) {
    code = "quota";
  } else if (FILE_IN_USE_ERRORS.has(name)) {
    code = "inUse";
  } else if (name === "NotFoundError") {
    code = "notFound";
  } else if (name === "NotReadableError") {
    // A file on the user's disk that is no longer the one that was chosen.
    code = "changed";
  }
  return { code, detail: `${name}: ${message}` };
}

/** Does something for which there have to be files, which not every browser keeps. */
export async function onDisk<T>(run: () => Promise<T>): Promise<T> {
  try {
    return await run();
  } catch (error) {
    const isUnknown = error instanceof DOMException && fileFailure(error).code === "io";
    throw isUnknown || !navigator.storage?.getDirectory ? new TableError("unsupported") : error;
  }
}

/** A file in the browser's storage that nobody else can open meanwhile. */
export class StoredFile implements TableFile {
  readonly #handle: SyncAccessHandleLike;
  #size: number;
  #closed = false;
  /** Why the last call failed. */
  failure: TableFailure | null = null;

  constructor(handle: SyncAccessHandleLike) {
    this.#handle = handle;
    this.#size = handle.getSize();
  }

  get size(): number {
    return this.#size;
  }

  readAt(at: number, into: Uint8Array): number {
    try {
      let done = 0;
      while (done < into.length) {
        const part = into.subarray(done) as Uint8Array<ArrayBuffer>;
        const count = this.#handle.read(part, { at: at + done });
        if (!(count > 0)) {
          this.failure = { code: "corrupt" };
          return 2;
        }
        done += count;
      }
      return 0;
    } catch (error) {
      this.failure = fileFailure(error);
      return 2;
    }
  }

  append(bytes: Uint8Array): boolean {
    try {
      let done = 0;
      while (done < bytes.length) {
        const part = bytes.subarray(done) as Uint8Array<ArrayBuffer>;
        const count = this.#handle.write(part, { at: this.#size });
        if (!(count > 0)) {
          // Not an exception in every browser.
          this.failure = { code: "quota" };
          return false;
        }
        done += count;
        this.#size += count;
      }
      return true;
    } catch (error) {
      this.failure = fileFailure(error);
      return false;
    }
  }

  /** Has the file end there, or empties it. */
  restart(size = 0): void {
    this.#handle.truncate(size);
    this.#size = size;
  }

  /** Has the file be one that was changed lately. */
  touch(): void {
    const first = new Uint8Array(1);
    if (this.readAt(0, first) === 0) {
      this.#handle.write(first, { at: 0 });
    }
  }

  /** Has everything that was written reach the disk. */
  flush(): void {
    this.#handle.flush();
  }

  close(): void {
    if (!this.#closed) {
      this.#closed = true;
      this.#handle.close();
    }
  }
}

/**
 * Opens a file. Another tab that was asked to let go of it may take a moment,
 * which is waited for as long as `patienceMs`.
 */
export async function openStored(
  dir: FileSystemDirectoryHandle,
  name: string,
  create: boolean,
  patienceMs: number,
): Promise<StoredFile> {
  const handle = (await dir.getFileHandle(name, { create })) as WorkerFileHandle;
  if (!handle.createSyncAccessHandle) {
    throw new TableError("unsupported");
  }
  const started = performance.now();
  for (;;) {
    try {
      return new StoredFile(await handle.createSyncAccessHandle());
    } catch (error) {
      if (fileFailure(error).code !== "inUse" || performance.now() - started >= patienceMs) {
        throw error;
      }
      await new Promise((resolve) => setTimeout(resolve, RETRY_MS));
    }
  }
}

/** A file on the user's disk. */
export class PickedFile implements TableFile {
  readonly #file: Blob;
  readonly #reader = new FileReaderSync();
  #window = new Uint8Array();
  #windowAt = 0;
  failure: TableFailure | null = null;

  constructor(file: Blob) {
    this.#file = file;
  }

  readAt(at: number, into: Uint8Array): number {
    try {
      const end = at + into.length;
      if (at < this.#windowAt || end > this.#windowAt + this.#window.length) {
        // What is read next is what follows, for the most part.
        const until = Math.max(end, at + WINDOW_BYTES);
        this.#window = new Uint8Array(this.#reader.readAsArrayBuffer(this.#file.slice(at, until)));
        this.#windowAt = at;
        if (this.#window.length < into.length) {
          this.failure = { code: "corrupt" };
          return 2;
        }
      }
      into.set(this.#window.subarray(at - this.#windowAt, end - this.#windowAt));
      return 0;
    } catch (error) {
      this.failure = fileFailure(error);
      return 2;
    }
  }

  append(): boolean {
    return false;
  }
}
