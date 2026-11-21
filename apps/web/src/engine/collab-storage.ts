import type { SyncCheckpoint, SyncCheckpointInfo } from "@reticula/protocol";
import {
  deleteMemoryPrefix,
  errorName,
  getRootDir,
  listMemoryKeys,
  openFileWalHandle,
  readMemoryFile,
  readOpfsFileBytes,
  type StorageBackendKind,
  upsertCatalogEntry,
  type WalHandle,
  type WorkbookCatalogEntry,
  writeDocState,
  writeMemoryFile,
  writeOpfsFileAtomic,
  writeSnapshotSlotFull,
} from "./storage.ts";

const COLLAB_DIR = "collab";
const LOCAL_DIR = "local";
const COLLAB_FORMAT_VERSION = 1;
export const PENDING_WAL_HEADER_LEN = 16;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export interface CollabReplicaMeta {
  formatVersion: number;
  userId: string;
  docId: string;
  branchId: string;
  title: string;
  activeSlot: "a" | "b";
  info: SyncCheckpointInfo | null;
  updatedAt: number;
}

export interface CollabLockAdapter {
  acquire(
    lockName: string,
    opts?: { ifAvailable?: boolean; steal?: boolean; patienceMs?: number },
  ): Promise<boolean>;
  release(lockName: string): void;
}

export class CollabOfflineBlockedError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "CollabOfflineBlockedError";
  }
}

export function collabPickLockName(userId: string, docId: string): string {
  return `reticula:collab-pick:${userId}:${docId}`;
}

export function collabSlotLockName(userId: string, docId: string, slot: string): string {
  return `reticula:collab:${userId}:${docId}:${slot}`;
}

function slotMemoryPrefix(userId: string, docId: string, slot: string): string {
  return `${COLLAB_DIR}/${userId}/${docId}/replica-${slot}`;
}

function parseSlotOrder(a: string, b: string): number {
  const na = /^\d+$/.test(a) ? Number(a) : Number.NaN;
  const nb = /^\d+$/.test(b) ? Number(b) : Number.NaN;
  if (!Number.isNaN(na) && !Number.isNaN(nb)) {
    return na - nb;
  }
  return a.localeCompare(b);
}

async function getCollabDocDir(
  userId: string,
  docId: string,
  create: boolean,
): Promise<FileSystemDirectoryHandle | null> {
  try {
    const root = await getRootDir(create);
    const collab = await root.getDirectoryHandle(COLLAB_DIR, { create });
    const userDir = await collab.getDirectoryHandle(userId, { create });
    return await userDir.getDirectoryHandle(docId, { create });
  } catch (err) {
    if (!create && errorName(err) === "NotFoundError") {
      return null;
    }
    throw err;
  }
}

async function readSlotMeta(
  slotDir: FileSystemDirectoryHandle | null,
  prefix: string,
  backend: StorageBackendKind,
): Promise<CollabReplicaMeta | null> {
  const bytes =
    backend === "memory" || slotDir === null
      ? readMemoryFile(`${prefix}/meta.json`)
      : await readOpfsFileBytes(slotDir, "meta.json");
  if (!bytes) {
    return null;
  }
  try {
    const parsed = JSON.parse(decoder.decode(bytes)) as Partial<CollabReplicaMeta>;
    if (
      typeof parsed.formatVersion !== "number" ||
      (parsed.activeSlot !== "a" && parsed.activeSlot !== "b")
    ) {
      return null;
    }
    return {
      formatVersion: parsed.formatVersion,
      userId: typeof parsed.userId === "string" ? parsed.userId : "",
      docId: typeof parsed.docId === "string" ? parsed.docId : "",
      branchId: typeof parsed.branchId === "string" ? parsed.branchId : "",
      title: typeof parsed.title === "string" ? parsed.title : "Untitled",
      activeSlot: parsed.activeSlot,
      info: parsed.info ?? null,
      updatedAt: typeof parsed.updatedAt === "number" ? parsed.updatedAt : 0,
    };
  } catch {
    return null;
  }
}

async function writeSlotMeta(
  slotDir: FileSystemDirectoryHandle | null,
  prefix: string,
  meta: CollabReplicaMeta,
  backend: StorageBackendKind,
): Promise<void> {
  const bytes = encoder.encode(JSON.stringify(meta));
  if (backend === "memory" || slotDir === null) {
    writeMemoryFile(`${prefix}/meta.json`, bytes);
    return;
  }
  await writeOpfsFileAtomic(slotDir, "meta.json", bytes, backend);
}

async function readPendingWalSize(
  slotDir: FileSystemDirectoryHandle | null,
  prefix: string,
  backend: StorageBackendKind,
): Promise<number> {
  if (backend === "memory" || slotDir === null) {
    return readMemoryFile(`${prefix}/pending.wal`)?.byteLength ?? 0;
  }
  try {
    const fh = await slotDir.getFileHandle("pending.wal", { create: false });
    const file = await fh.getFile();
    return file.size;
  } catch (err) {
    if (errorName(err) === "NotFoundError") {
      return 0;
    }
    throw err;
  }
}

async function removeReplicaSlotDir(
  docDir: FileSystemDirectoryHandle | null,
  prefix: string,
  slot: string,
  backend: StorageBackendKind,
): Promise<void> {
  if (backend === "memory" || docDir === null) {
    deleteMemoryPrefix(`${prefix}/`);
    return;
  }
  try {
    await docDir.removeEntry(`replica-${slot}`, { recursive: true });
  } catch (err) {
    if (errorName(err) !== "NotFoundError") {
      throw err;
    }
  }
}

export interface CollabReplicaSession {
  readonly userId: string;
  readonly docId: string;
  readonly branchId: string;
  readonly slot: string;
  readonly lockName: string;
  readonly initialInfo: SyncCheckpointInfo | null;
  readonly initialSnapshot: Uint8Array;
  readonly initialPendingWal: Uint8Array;
  title(): string;
  persistedInfo(): SyncCheckpointInfo | null;
  checkpointNeeded(info: SyncCheckpointInfo): boolean;
  writeCheckpoint(checkpoint: SyncCheckpoint): Promise<void>;
  appendPending(bytes: Uint8Array, pendingWalInit: Uint8Array): Promise<void>;
  rewritePending(bytes: Uint8Array, checkpoint?: SyncCheckpoint): Promise<void>;
  saveUnsyncedRescue(
    snapshot: Uint8Array,
    pendingWal: Uint8Array,
    walInit: Uint8Array,
  ): Promise<{
    readonly localDocId: string;
    readonly title: string;
    readonly docs: readonly WorkbookCatalogEntry[];
  }>;
  close(): void;
}

export interface OpenCollabReplicaOptions {
  readonly userId: string;
  readonly docId: string;
  readonly branchId: string;
  readonly title: string;
  readonly online: boolean;
  readonly backend: StorageBackendKind;
  readonly locks: CollabLockAdapter;
}

/**
 * Selects or initializes an exclusive per-tab collaborative replica under
 * `reticula-v1/collab/<userId>/<docId>/replica-<slot>/` (`15-m2a.md` 2.6 &
 * `03-architecture.md` "多标签页与副本").
 */
export async function openCollabReplica(
  opts: OpenCollabReplicaOptions,
): Promise<CollabReplicaSession> {
  const { userId, docId, branchId, title, online, backend, locks } = opts;
  const pickLock = collabPickLockName(userId, docId);
  await locks.acquire(pickLock);

  let chosenSlot = "";
  let chosenLockName = "";
  let chosenDir: FileSystemDirectoryHandle | null = null;
  let chosenMeta: CollabReplicaMeta | null = null;

  const acquiredLocks: string[] = [];
  const releaseAcquiredExcept = (keep?: string) => {
    for (const name of acquiredLocks) {
      if (name !== keep) {
        locks.release(name);
      }
    }
  };

  try {
    const docDir = backend === "memory" ? null : await getCollabDocDir(userId, docId, true);
    const existingSlots = new Set<string>();

    if (backend === "memory" || docDir === null) {
      const basePrefix = `${COLLAB_DIR}/${userId}/${docId}/replica-`;
      for (const key of listMemoryKeys(basePrefix)) {
        const rest = key.slice(basePrefix.length);
        const slash = rest.indexOf("/");
        if (slash > 0) {
          existingSlots.add(rest.slice(0, slash));
        }
      }
    } else {
      const iterable = docDir as FileSystemDirectoryHandle & {
        entries(): AsyncIterableIterator<[string, FileSystemHandle]>;
      };
      for await (const [name, handle] of iterable.entries()) {
        if (handle.kind === "directory" && name.startsWith("replica-")) {
          const slot = name.slice("replica-".length);
          if (slot.length > 0) {
            existingSlots.add(slot);
          }
        }
      }
    }

    const sortedSlots = [...existingSlots].sort(parseSlotOrder);
    interface Candidate {
      slot: string;
      lockName: string;
      dir: FileSystemDirectoryHandle | null;
      meta: CollabReplicaMeta | null;
      pendingBytes: number;
    }
    const candidates: Candidate[] = [];

    for (const slot of sortedSlots) {
      const lockName = collabSlotLockName(userId, docId, slot);
      const granted = await locks.acquire(lockName, { ifAvailable: true });
      if (!granted) {
        continue;
      }
      acquiredLocks.push(lockName);
      const prefix = slotMemoryPrefix(userId, docId, slot);
      const slotDir =
        backend === "memory" || docDir === null
          ? null
          : await docDir.getDirectoryHandle(`replica-${slot}`, { create: false });
      const meta = await readSlotMeta(slotDir, prefix, backend);
      if (meta && meta.formatVersion > COLLAB_FORMAT_VERSION) {
        throw new Error(
          "This collaborative workbook was saved by a newer version of Reticula. Please update and reload.",
        );
      }
      const pendingBytes = await readPendingWalSize(slotDir, prefix, backend);
      candidates.push({ slot, lockName, dir: slotDir, meta, pendingBytes });
    }

    const hasUnsent = (c: Candidate) => c.pendingBytes > PENDING_WAL_HEADER_LEN;
    const hasReplica = (c: Candidate) =>
      c.meta?.info !== null &&
      c.meta?.info !== undefined &&
      c.meta.info.replica !== null &&
      c.meta.info.replica !== undefined;

    const chosen =
      candidates.find((c) => hasUnsent(c) && hasReplica(c)) ??
      candidates.find((c) => hasUnsent(c)) ??
      candidates.find((c) => hasReplica(c)) ??
      candidates[0] ??
      null;

    if (chosen) {
      chosenSlot = chosen.slot;
      chosenLockName = chosen.lockName;
      chosenDir = chosen.dir;
      chosenMeta = chosen.meta;

      for (const other of candidates) {
        if (other === chosen) {
          continue;
        }
        // Clean up idle replica directories that have no unsent edits (`03-architecture.md`),
        // but preserve any unheld directories that still hold unsent pending transactions.
        if (!hasUnsent(other)) {
          await removeReplicaSlotDir(
            docDir,
            slotMemoryPrefix(userId, docId, other.slot),
            other.slot,
            backend,
          );
        }
        locks.release(other.lockName);
      }
    } else {
      if (!online) {
        throw new CollabOfflineBlockedError(
          "Offline: cannot open an additional window for this collaborative workbook without a network connection.",
        );
      }
      let nextNum = 1;
      while (existingSlots.has(String(nextNum))) {
        nextNum += 1;
      }
      for (;;) {
        const candidateSlot = String(nextNum);
        const lockName = collabSlotLockName(userId, docId, candidateSlot);
        const granted = await locks.acquire(lockName, { ifAvailable: true });
        if (granted) {
          acquiredLocks.push(lockName);
          chosenSlot = candidateSlot;
          chosenLockName = lockName;
          break;
        }
        nextNum += 1;
      }
      chosenDir =
        backend === "memory" || docDir === null
          ? null
          : await docDir.getDirectoryHandle(`replica-${chosenSlot}`, { create: true });
      chosenMeta = {
        formatVersion: COLLAB_FORMAT_VERSION,
        userId,
        docId,
        branchId,
        title,
        activeSlot: "a",
        info: null,
        updatedAt: Date.now(),
      };
      await writeSlotMeta(
        chosenDir,
        slotMemoryPrefix(userId, docId, chosenSlot),
        chosenMeta,
        backend,
      );
    }
  } catch (err) {
    releaseAcquiredExcept();
    throw err;
  } finally {
    locks.release(pickLock);
  }

  const slotPrefix = slotMemoryPrefix(userId, docId, chosenSlot);
  let meta: CollabReplicaMeta = chosenMeta ?? {
    formatVersion: COLLAB_FORMAT_VERSION,
    userId,
    docId,
    branchId,
    title,
    activeSlot: "a",
    info: null,
    updatedAt: Date.now(),
  };

  let pendingWal: WalHandle;
  try {
    pendingWal = await openFileWalHandle(
      chosenDir,
      "pending.wal",
      `${slotPrefix}/pending.wal`,
      backend,
      5_000,
    );
  } catch (err) {
    locks.release(chosenLockName);
    throw err;
  }

  let initialPendingWal: Uint8Array;
  let initialSnapshot = new Uint8Array(0);
  try {
    initialPendingWal = await pendingWal.readAll();
    if (meta.info !== null) {
      const snapName = `snapshot-${meta.activeSlot}.bin`;
      const loaded =
        backend === "memory" || chosenDir === null
          ? readMemoryFile(`${slotPrefix}/${snapName}`)
          : await readOpfsFileBytes(chosenDir, snapName);
      if (loaded) {
        initialSnapshot = new Uint8Array(loaded);
      }
    }
    if (meta.title !== title && title.trim().length > 0) {
      meta = { ...meta, title, updatedAt: Date.now() };
      await writeSlotMeta(chosenDir, slotPrefix, meta, backend);
    }
  } catch (err) {
    pendingWal.close();
    locks.release(chosenLockName);
    throw err;
  }

  const writeCheckpoint = async (checkpoint: SyncCheckpoint): Promise<void> => {
    const nextSlot: "a" | "b" = meta.activeSlot === "a" ? "b" : "a";
    const snapName = `snapshot-${nextSlot}.bin`;
    if (backend === "memory" || chosenDir === null) {
      writeMemoryFile(`${slotPrefix}/${snapName}`, checkpoint.snapshot);
    } else {
      await writeOpfsFileAtomic(chosenDir, snapName, checkpoint.snapshot, backend);
    }
    const nextMeta: CollabReplicaMeta = {
      ...meta,
      formatVersion: COLLAB_FORMAT_VERSION,
      userId,
      docId,
      branchId,
      activeSlot: nextSlot,
      info: checkpoint.info,
      updatedAt: Date.now(),
    };
    await writeSlotMeta(chosenDir, slotPrefix, nextMeta, backend);
    meta = nextMeta;
  };

  let closed = false;

  return {
    userId,
    docId,
    branchId,
    slot: chosenSlot,
    lockName: chosenLockName,
    initialInfo: meta.info,
    initialSnapshot,
    initialPendingWal,
    title() {
      return meta.title;
    },
    persistedInfo() {
      return meta.info;
    },
    checkpointNeeded(info: SyncCheckpointInfo) {
      const cur = meta.info;
      if (!cur) {
        return true;
      }
      return (
        cur.epoch !== info.epoch ||
        cur.replica !== info.replica ||
        cur.confirmedSeq !== info.confirmedSeq ||
        cur.confirmedClientSeq !== info.confirmedClientSeq
      );
    },
    writeCheckpoint,
    async appendPending(bytes: Uint8Array, pendingWalInit: Uint8Array) {
      if (pendingWal.byteLength < PENDING_WAL_HEADER_LEN) {
        const combined = new Uint8Array(pendingWalInit.byteLength + bytes.byteLength);
        combined.set(pendingWalInit, 0);
        combined.set(bytes, pendingWalInit.byteLength);
        await pendingWal.rewrite(combined);
      } else {
        await pendingWal.append(bytes);
      }
    },
    async rewritePending(bytes: Uint8Array, checkpoint?: SyncCheckpoint) {
      if (checkpoint) {
        await writeCheckpoint(checkpoint);
      }
      await pendingWal.rewrite(bytes);
    },
    async saveUnsyncedRescue(snapshot: Uint8Array, rejectedWal: Uint8Array, walInit: Uint8Array) {
      const now = Date.now();
      const localDocId = `unsynced-${now.toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
      const rescueTitle = `Unsynced changes - ${meta.title}`;
      await writeSnapshotSlotFull(localDocId, "a", snapshot, backend);
      if (backend === "memory") {
        writeMemoryFile(`${LOCAL_DIR}/${localDocId}/wal.log`, walInit);
        writeMemoryFile(`${LOCAL_DIR}/${localDocId}/rejected-pending.wal`, rejectedWal);
      } else {
        const root = await getRootDir(true);
        const localRoot = await root.getDirectoryHandle(LOCAL_DIR, { create: true });
        const rescueDir = await localRoot.getDirectoryHandle(localDocId, { create: true });
        await writeOpfsFileAtomic(rescueDir, "wal.log", walInit, backend);
        await writeOpfsFileAtomic(rescueDir, "rejected-pending.wal", rejectedWal, backend);
      }
      await writeDocState(
        localDocId,
        {
          activeSlot: "a",
          snapshotSeq: 0,
          lastFlushedSeq: 0,
          openingCount: 0,
        },
        backend,
      );
      const docs = await upsertCatalogEntry(
        {
          id: localDocId,
          title: rescueTitle,
          createdAt: now,
          updatedAt: now,
          sizeBytes: snapshot.byteLength + walInit.byteLength + rejectedWal.byteLength,
        },
        backend,
      );
      return { localDocId, title: rescueTitle, docs };
    },
    close() {
      if (!closed) {
        closed = true;
        pendingWal.close();
        locks.release(chosenLockName);
      }
    },
  };
}
