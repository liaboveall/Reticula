export type StorageBackendKind = "opfs-sync" | "opfs-writable" | "memory";

export interface WorkbookCatalogEntry {
  readonly id: string;
  readonly title: string;
  readonly createdAt: number;
  readonly updatedAt: number;
  readonly sizeBytes: number;
}

export interface LocalDocState {
  activeSlot: "a" | "b";
  snapshotSeq: number;
  lastFlushedSeq: number;
  openingCount: number;
}

/**
 *Dedicated-worker `FileSystemSyncAccessHandle` surface (`lib.dom` omits this
 * worker-only interface).
 */
export interface SyncAccessHandleLike {
  getSize(): number;
  read(buffer: Uint8Array<ArrayBuffer>, options?: { at?: number }): number;
  write(buffer: Uint8Array<ArrayBuffer>, options?: { at?: number }): number;
  truncate(newSize: number): void;
  flush(): void;
  close(): void;
}

interface WorkerFileHandle extends FileSystemFileHandle {
  createSyncAccessHandle?: () => Promise<SyncAccessHandleLike>;
}

export const OPFS_ROOT_DIR = "reticula-v1";
const CATALOG_FILE = "catalog.json";
const LOCAL_DIR = "local";

/**
 * Error names thrown when `createSyncAccessHandle()` finds a file already open
 * in another context (`NoModificationAllowedError` on Chromium/Firefox,
 * `InvalidStateError` on Safari/macOS WebKit — `14-m1b.md` 7.2).
 */
export const FILE_IN_USE_ERRORS: ReadonlySet<string> = new Set([
  "NoModificationAllowedError",
  "InvalidStateError",
]);

export function errorName(err: unknown): string {
  return err !== null &&
    typeof err === "object" &&
    "name" in err &&
    typeof (err as { name: unknown }).name === "string"
    ? (err as { name: string }).name
    : "";
}

function errorMessage(err: unknown): string {
  return err !== null &&
    typeof err === "object" &&
    "message" in err &&
    typeof (err as { message: unknown }).message === "string"
    ? (err as { message: string }).message
    : "";
}

export function isQuotaExceededError(err: unknown): boolean {
  const name = errorName(err);
  const msg = errorMessage(err).toLowerCase();
  return name === "QuotaExceededError" || msg.includes("quota") || msg.includes("disk full");
}

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function toArrayBufferBacked(bytes: Uint8Array): Uint8Array<ArrayBuffer> {
  if (bytes.buffer instanceof ArrayBuffer) {
    return new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  }
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy;
}

function readAllSync(sync: SyncAccessHandleLike): Uint8Array {
  const total = sync.getSize();
  const out = new Uint8Array(total);
  let pos = 0;
  while (pos < total) {
    const n = sync.read(out.subarray(pos), { at: pos });
    if (n <= 0) {
      break;
    }
    pos += n;
  }
  return pos === total ? out : out.subarray(0, pos);
}

function writeAllSync(sync: SyncAccessHandleLike, bytes: Uint8Array, truncateAfter: boolean): void {
  const view = toArrayBufferBacked(bytes);
  let pos = 0;
  while (pos < view.byteLength) {
    const n = sync.write(view.subarray(pos), { at: pos });
    if (n <= 0) {
      throw new Error("SyncAccessHandle wrote 0 bytes.");
    }
    pos += n;
  }
  if (truncateAfter && sync.getSize() !== view.byteLength) {
    sync.truncate(view.byteLength);
  }
  sync.flush();
}

// Fallback in-memory FS used only if OPFS itself is completely unavailable.
const memoryFiles = new Map<string, Uint8Array>();

export function readMemoryFile(key: string): Uint8Array | null {
  const raw = memoryFiles.get(key);
  return raw ? raw.slice() : null;
}

export function writeMemoryFile(key: string, bytes: Uint8Array): void {
  memoryFiles.set(key, bytes.slice());
}

export function deleteMemoryPrefix(prefix: string): void {
  for (const key of [...memoryFiles.keys()]) {
    if (key.startsWith(prefix)) {
      memoryFiles.delete(key);
    }
  }
}

export function listMemoryKeys(prefix: string): string[] {
  const out: string[] = [];
  for (const key of memoryFiles.keys()) {
    if (key.startsWith(prefix)) {
      out.push(key);
    }
  }
  return out;
}

let cachedBackend: StorageBackendKind | null = null;

/**
 * Probes OPFS + `createSyncAccessHandle()` once per worker under a dedicated
 * Web Lock so two concurrent tabs probing at startup never collide.
 */
export async function detectStorageBackend(): Promise<StorageBackendKind> {
  if (cachedBackend !== null) {
    return cachedBackend;
  }
  if (typeof navigator === "undefined" || !navigator.storage?.getDirectory) {
    cachedBackend = "memory";
    return cachedBackend;
  }

  const probeImpl = async (): Promise<StorageBackendKind> => {
    try {
      const root = await navigator.storage.getDirectory();
      const dir = await root.getDirectoryHandle(OPFS_ROOT_DIR, { create: true });
      const probeHandle = (await dir.getFileHandle(".sync-probe", {
        create: true,
      })) as WorkerFileHandle;
      if (typeof probeHandle.createSyncAccessHandle === "function") {
        try {
          const sync = await probeHandle.createSyncAccessHandle();
          sync.close();
          return "opfs-sync";
        } catch {
          // WinCairo WebKit on Windows lacks SyncAccessHandle; check async writable next.
        }
      }
      if (typeof probeHandle.createWritable === "function") {
        const writable = await probeHandle.createWritable();
        await writable.close();
        return "opfs-writable";
      }
      return "memory";
    } catch {
      return "memory";
    }
  };

  if (navigator.locks?.request) {
    cachedBackend = await navigator.locks.request(
      "reticula:storage-probe",
      { mode: "exclusive" },
      probeImpl,
    );
  } else {
    cachedBackend = await probeImpl();
  }
  return cachedBackend;
}

export async function getRootDir(create = true): Promise<FileSystemDirectoryHandle> {
  const opfs = await navigator.storage.getDirectory();
  return opfs.getDirectoryHandle(OPFS_ROOT_DIR, { create });
}

async function getDocDir(docId: string, create = true): Promise<FileSystemDirectoryHandle> {
  const root = await getRootDir(create);
  const local = await root.getDirectoryHandle(LOCAL_DIR, { create });
  return local.getDirectoryHandle(docId, { create });
}

export async function readOpfsFileBytes(
  dir: FileSystemDirectoryHandle,
  name: string,
): Promise<Uint8Array | null> {
  try {
    const fh = await dir.getFileHandle(name, { create: false });
    const file = await fh.getFile();
    return new Uint8Array(await file.arrayBuffer());
  } catch (err) {
    if (errorName(err) === "NotFoundError") {
      return null;
    }
    throw err;
  }
}

export async function writeOpfsFileAtomic(
  dir: FileSystemDirectoryHandle,
  name: string,
  bytes: Uint8Array,
  backend: StorageBackendKind,
): Promise<void> {
  const fh = (await dir.getFileHandle(name, { create: true })) as WorkerFileHandle;
  if (backend === "opfs-sync" && typeof fh.createSyncAccessHandle === "function") {
    const sync = await fh.createSyncAccessHandle();
    try {
      writeAllSync(sync, bytes, true);
    } finally {
      sync.close();
    }
    return;
  }
  const writable = await fh.createWritable({ keepExistingData: false });
  await writable.write(toArrayBufferBacked(bytes));
  await writable.close();
}

// --- Catalog & Doc State ----------------------------------------------------

export async function readCatalog(backend: StorageBackendKind): Promise<WorkbookCatalogEntry[]> {
  if (backend === "memory") {
    const raw = memoryFiles.get(CATALOG_FILE);
    if (!raw) {
      return [];
    }
    try {
      return JSON.parse(decoder.decode(raw)) as WorkbookCatalogEntry[];
    } catch {
      return [];
    }
  }
  const root = await getRootDir(true);
  const bytes = await readOpfsFileBytes(root, CATALOG_FILE);
  if (!bytes) {
    return [];
  }
  try {
    const parsed = JSON.parse(decoder.decode(bytes));
    return Array.isArray(parsed) ? (parsed as WorkbookCatalogEntry[]) : [];
  } catch {
    return [];
  }
}

export async function writeCatalog(
  entries: readonly WorkbookCatalogEntry[],
  backend: StorageBackendKind,
): Promise<void> {
  const bytes = encoder.encode(JSON.stringify(entries));
  if (backend === "memory") {
    memoryFiles.set(CATALOG_FILE, bytes);
    return;
  }
  const root = await getRootDir(true);
  await writeOpfsFileAtomic(root, CATALOG_FILE, bytes, backend);
}

export async function upsertCatalogEntry(
  entry: WorkbookCatalogEntry,
  backend: StorageBackendKind,
): Promise<WorkbookCatalogEntry[]> {
  const list = await readCatalog(backend);
  const idx = list.findIndex((item) => item.id === entry.id);
  const next = [...list];
  if (idx >= 0) {
    const existing = next[idx];
    next[idx] = {
      ...entry,
      createdAt: existing ? existing.createdAt : entry.createdAt,
    };
  } else {
    next.push(entry);
  }
  next.sort((a, b) => b.updatedAt - a.updatedAt);
  await writeCatalog(next, backend);
  return next;
}

export async function removeDocStorage(
  docId: string,
  backend: StorageBackendKind,
): Promise<WorkbookCatalogEntry[]> {
  if (backend === "memory") {
    for (const key of [...memoryFiles.keys()]) {
      if (key.startsWith(`${LOCAL_DIR}/${docId}/`)) {
        memoryFiles.delete(key);
      }
    }
  } else {
    try {
      const root = await getRootDir(true);
      const local = await root.getDirectoryHandle(LOCAL_DIR, { create: true });
      await local.removeEntry(docId, { recursive: true });
    } catch (err) {
      if (errorName(err) !== "NotFoundError") {
        throw err;
      }
    }
  }
  const list = (await readCatalog(backend)).filter((e) => e.id !== docId);
  await writeCatalog(list, backend);
  return list;
}

export async function readDocState(
  docId: string,
  backend: StorageBackendKind,
): Promise<LocalDocState | null> {
  let bytes: Uint8Array | null = null;
  if (backend === "memory") {
    bytes = memoryFiles.get(`${LOCAL_DIR}/${docId}/state.json`) ?? null;
  } else {
    try {
      const dir = await getDocDir(docId, false);
      bytes = await readOpfsFileBytes(dir, "state.json");
    } catch (err) {
      if (errorName(err) === "NotFoundError") {
        return null;
      }
      throw err;
    }
  }
  if (!bytes) {
    return null;
  }
  const parsed = JSON.parse(decoder.decode(bytes)) as Partial<LocalDocState>;
  if (parsed.activeSlot !== "a" && parsed.activeSlot !== "b") {
    throw new Error("state.json has invalid activeSlot");
  }
  return {
    activeSlot: parsed.activeSlot,
    snapshotSeq: typeof parsed.snapshotSeq === "number" ? parsed.snapshotSeq : 0,
    lastFlushedSeq: typeof parsed.lastFlushedSeq === "number" ? parsed.lastFlushedSeq : 0,
    openingCount: typeof parsed.openingCount === "number" ? parsed.openingCount : 0,
  };
}

export async function writeDocState(
  docId: string,
  state: LocalDocState,
  backend: StorageBackendKind,
): Promise<void> {
  const bytes = encoder.encode(JSON.stringify(state));
  if (backend === "memory") {
    memoryFiles.set(`${LOCAL_DIR}/${docId}/state.json`, bytes);
    return;
  }
  const dir = await getDocDir(docId, true);
  await writeOpfsFileAtomic(dir, "state.json", bytes, backend);
}

export async function readSnapshotSlot(
  docId: string,
  slot: "a" | "b",
  backend: StorageBackendKind,
): Promise<Uint8Array | null> {
  const name = `snapshot-${slot}.bin`;
  if (backend === "memory") {
    const raw = memoryFiles.get(`${LOCAL_DIR}/${docId}/${name}`);
    return raw ? raw.slice() : null;
  }
  const dir = await getDocDir(docId, true);
  return readOpfsFileBytes(dir, name);
}

export async function writeSnapshotSlotFull(
  docId: string,
  slot: "a" | "b",
  bytes: Uint8Array,
  backend: StorageBackendKind,
): Promise<void> {
  const name = `snapshot-${slot}.bin`;
  if (backend === "memory") {
    memoryFiles.set(`${LOCAL_DIR}/${docId}/${name}`, bytes.slice());
    return;
  }
  const dir = await getDocDir(docId, true);
  await writeOpfsFileAtomic(dir, name, bytes, backend);
}

// --- Exclusive WAL Handle (`wal.log`) ---------------------------------------

export interface WalHandle {
  readonly byteLength: number;
  readAll(): Promise<Uint8Array>;
  append(frame: Uint8Array): Promise<void>;
  rewrite(bytes: Uint8Array): Promise<void>;
  truncate(validBytes: number): Promise<void>;
  close(): void;
}

/**
 * Opens `reticula-v1/local/<docId>/wal.log` exclusively.
 * On `"opfs-sync"`, retries while `NoModificationAllowedError` or Safari's
 * `InvalidStateError` indicates the previous tab has not yet closed its handle,
 * calling `onWaitingForRelease` on the first contention retry (`14-m1b.md` 7.2).
 */
export async function openWalHandle(
  docId: string,
  backend: StorageBackendKind,
  patienceMs: number,
  onWaitingForRelease?: () => void,
): Promise<WalHandle> {
  const memoryKey = `${LOCAL_DIR}/${docId}/wal.log`;
  const dir = backend === "memory" ? null : await getDocDir(docId, true);
  return openFileWalHandle(dir, "wal.log", memoryKey, backend, patienceMs, onWaitingForRelease);
}

/**
 * Opens an arbitrary append-only WAL file (`wal.log` or `pending.wal`)
 * exclusively, reusing the `opfs-sync` / `opfs-writable` / `memory` backends.
 */
export async function openFileWalHandle(
  dir: FileSystemDirectoryHandle | null,
  fileName: string,
  memoryKey: string,
  backend: StorageBackendKind,
  patienceMs: number,
  onWaitingForRelease?: () => void,
): Promise<WalHandle> {
  if (backend === "memory" || dir === null) {
    let buf = memoryFiles.get(memoryKey) ?? new Uint8Array(0);
    return {
      get byteLength() {
        return buf.byteLength;
      },
      async readAll() {
        return buf.slice();
      },
      async append(frame) {
        const next = new Uint8Array(buf.byteLength + frame.byteLength);
        next.set(buf, 0);
        next.set(frame, buf.byteLength);
        buf = next;
        memoryFiles.set(memoryKey, buf);
      },
      async rewrite(bytes) {
        buf = bytes.slice();
        memoryFiles.set(memoryKey, buf);
      },
      async truncate(validBytes) {
        buf = buf.slice(0, validBytes);
        memoryFiles.set(memoryKey, buf);
      },
      close() {},
    };
  }

  const fh = (await dir.getFileHandle(fileName, { create: true })) as WorkerFileHandle;

  if (backend === "opfs-sync" && typeof fh.createSyncAccessHandle === "function") {
    const started = performance.now();
    let notifiedWaiting = false;
    let sync: SyncAccessHandleLike;
    for (;;) {
      try {
        sync = await fh.createSyncAccessHandle();
        break;
      } catch (err) {
        const errName = errorName(err);
        if (!FILE_IN_USE_ERRORS.has(errName) || performance.now() - started >= patienceMs) {
          throw err;
        }
        if (!notifiedWaiting) {
          notifiedWaiting = true;
          onWaitingForRelease?.();
        }
        await sleep(25);
      }
    }

    let size = sync.getSize();
    let closed = false;
    return {
      get byteLength() {
        return size;
      },
      async readAll() {
        const data = readAllSync(sync);
        size = data.byteLength;
        return data;
      },
      async append(frame) {
        const view = toArrayBufferBacked(frame);
        let written = 0;
        while (written < view.byteLength) {
          const n = sync.write(view.subarray(written), { at: size + written });
          if (n <= 0) {
            throw new Error("WAL SyncAccessHandle write returned 0");
          }
          written += n;
        }
        size += view.byteLength;
        sync.flush();
      },
      async rewrite(bytes) {
        writeAllSync(sync, bytes, true);
        size = bytes.byteLength;
      },
      async truncate(validBytes) {
        sync.truncate(validBytes);
        sync.flush();
        size = validBytes;
      },
      close() {
        if (!closed) {
          closed = true;
          try {
            sync.flush();
          } catch {
            // Ignore flush error on closing evicted handle.
          }
          sync.close();
        }
      },
    };
  }

  // Async OPFS writable fallback (used only on platforms without SyncAccessHandle, e.g. WinCairo WebKit).
  const initialFile = await fh.getFile();
  let mirror = new Uint8Array(await initialFile.arrayBuffer());
  let writeChain: Promise<void> = Promise.resolve();
  return {
    get byteLength() {
      return mirror.byteLength;
    },
    async readAll() {
      await writeChain;
      const f = await fh.getFile();
      mirror = new Uint8Array(await f.arrayBuffer());
      return mirror.slice();
    },
    async append(frame) {
      const next = new Uint8Array(mirror.byteLength + frame.byteLength);
      next.set(mirror, 0);
      next.set(frame, mirror.byteLength);
      mirror = next;
      const snapshot = toArrayBufferBacked(mirror);
      writeChain = writeChain.then(async () => {
        const w = await fh.createWritable({ keepExistingData: false });
        await w.write(snapshot);
        await w.close();
      });
      await writeChain;
    },
    async rewrite(bytes) {
      mirror = bytes.slice();
      const snapshot = toArrayBufferBacked(mirror);
      writeChain = writeChain.then(async () => {
        const w = await fh.createWritable({ keepExistingData: false });
        await w.write(snapshot);
        await w.close();
      });
      await writeChain;
    },
    async truncate(validBytes) {
      mirror = mirror.slice(0, validBytes);
      const snapshot = toArrayBufferBacked(mirror);
      writeChain = writeChain.then(async () => {
        const w = await fh.createWritable({ keepExistingData: false });
        await w.write(snapshot);
        await w.close();
      });
      await writeChain;
    },
    close() {},
  };
}

// --- Streaming Inactive Snapshot Writer (`snapshot-a/b.bin`) ----------------

export interface SnapshotStreamSink {
  writeAt(chunk: Uint8Array, offset: number): Promise<void>;
  finish(totalLen: number): Promise<void>;
  abort(): void;
}

export async function openSnapshotStreamSink(
  docId: string,
  slot: "a" | "b",
  backend: StorageBackendKind,
): Promise<SnapshotStreamSink> {
  const name = `snapshot-${slot}.bin`;
  if (backend === "memory") {
    const chunks: { offset: number; bytes: Uint8Array }[] = [];
    return {
      async writeAt(chunk, offset) {
        chunks.push({ offset, bytes: chunk.slice() });
      },
      async finish(totalLen) {
        const out = new Uint8Array(totalLen);
        for (const c of chunks) {
          out.set(c.bytes, c.offset);
        }
        memoryFiles.set(`${LOCAL_DIR}/${docId}/${name}`, out);
      },
      abort() {
        chunks.length = 0;
      },
    };
  }

  const dir = await getDocDir(docId, true);
  const fh = (await dir.getFileHandle(name, { create: true })) as WorkerFileHandle;

  if (backend === "opfs-sync" && typeof fh.createSyncAccessHandle === "function") {
    const sync = await fh.createSyncAccessHandle();
    sync.truncate(0);
    let closed = false;
    return {
      async writeAt(chunk, offset) {
        const view = toArrayBufferBacked(chunk);
        let pos = 0;
        while (pos < view.byteLength) {
          const n = sync.write(view.subarray(pos), { at: offset + pos });
          if (n <= 0) {
            throw new Error("Snapshot SyncAccessHandle write returned 0");
          }
          pos += n;
        }
      },
      async finish(totalLen) {
        if (!closed) {
          if (sync.getSize() !== totalLen) {
            sync.truncate(totalLen);
          }
          sync.flush();
          closed = true;
          sync.close();
        }
      },
      abort() {
        if (!closed) {
          closed = true;
          try {
            sync.close();
          } catch {
            // Ignore close errors on abort.
          }
        }
      },
    };
  }

  const chunks: { offset: number; bytes: Uint8Array }[] = [];
  let aborted = false;
  return {
    async writeAt(chunk, offset) {
      if (!aborted) {
        chunks.push({ offset, bytes: chunk.slice() });
      }
    },
    async finish(totalLen) {
      if (aborted) {
        return;
      }
      const out = new Uint8Array(totalLen);
      for (const c of chunks) {
        out.set(c.bytes, c.offset);
      }
      const w = await fh.createWritable({ keepExistingData: false });
      await w.write(out);
      await w.close();
    },
    abort() {
      aborted = true;
      chunks.length = 0;
    },
  };
}

/**
 * Pure main-thread OPFS `getFile()` escape hatch (`14-m1b.md` 7.3): reads raw
 * `state.json`, `snapshot-a.bin`, `snapshot-b.bin`, and `wal.log` directly from
 * OPFS without touching WASM or the Worker, so user data can always be rescued.
 */
export async function readRawDocFilesFromOpfs(
  docId: string,
): Promise<{ readonly filename: string; readonly bytes: Uint8Array }[]> {
  if (typeof navigator === "undefined" || !navigator.storage?.getDirectory) {
    return [];
  }
  const out: { readonly filename: string; readonly bytes: Uint8Array }[] = [];
  try {
    const dir = await getDocDir(docId, false);
    for (const name of ["state.json", "snapshot-a.bin", "snapshot-b.bin", "wal.log"]) {
      const bytes = await readOpfsFileBytes(dir, name);
      if (bytes && bytes.byteLength > 0) {
        out.push({ filename: `${docId}-${name}`, bytes });
      }
    }
  } catch {
    // Directory does not exist or cannot be read.
  }
  return out;
}
