import type { SyncCheckpoint } from "@reticula/protocol";
import { beforeEach, describe, expect, it } from "vitest";
import {
  type CollabLockAdapter,
  CollabOfflineBlockedError,
  openCollabReplica,
  PENDING_WAL_HEADER_LEN,
} from "./collab-storage.ts";
import { listMemoryKeys, readCatalog, readDocState, readMemoryFile } from "./storage.ts";

function createTestLocks(): CollabLockAdapter {
  const held = new Set<string>();
  return {
    async acquire(lockName, opts) {
      if (held.has(lockName)) {
        if (opts?.ifAvailable) {
          return false;
        }
        throw new Error(`Unexpected blocking lock contention on ${lockName}`);
      }
      held.add(lockName);
      return true;
    },
    release(lockName) {
      held.delete(lockName);
    },
  };
}

function fakePendingHeader(): Uint8Array {
  const header = new Uint8Array(PENDING_WAL_HEADER_LEN);
  header.set([0x52, 0x54, 0x43, 0x4c, 0x50, 0x57, 0x41, 0x4c], 0); // "RTCLPWAL"
  return header;
}

function makeCheckpoint(
  replica: number,
  confirmedSeq = "10",
  confirmedClientSeq = "1",
): SyncCheckpoint {
  return {
    snapshot: new Uint8Array([1, 2, 3, 4]),
    info: {
      epoch: 1,
      replica,
      confirmedSeq,
      confirmedClientSeq,
      nextIdCounter: 0,
    },
  };
}

describe("collab-storage", () => {
  const docId = "00000000-0000-4000-8000-000000000001";
  const branchId = "00000000-0000-4000-8000-000000000002";

  beforeEach(() => {
    for (const key of listMemoryKeys("")) {
      // storage.ts keeps memory map in module scope; isolate per test using unique docIds or cleanup
      void key;
    }
  });

  it("allocates distinct slots for concurrent tabs of the same user and reuses after close", async () => {
    const locks = createTestLocks();
    const d1 = `${docId}-a`;
    const tab1 = await openCollabReplica({
      userId: "alice",
      docId: d1,
      branchId,
      title: "Budget",
      online: true,
      backend: "memory",
      locks,
    });
    expect(tab1.slot).toBe("1");

    const tab2 = await openCollabReplica({
      userId: "alice",
      docId: d1,
      branchId,
      title: "Budget",
      online: true,
      backend: "memory",
      locks,
    });
    expect(tab2.slot).toBe("2");

    tab1.close();
    const tab3 = await openCollabReplica({
      userId: "alice",
      docId: d1,
      branchId,
      title: "Budget",
      online: true,
      backend: "memory",
      locks,
    });
    expect(tab3.slot).toBe("1");
    tab2.close();
    tab3.close();
  });

  it("takes over slot with unsent pending WAL (>16 bytes) and prunes clean surplus slots only", async () => {
    const locks = createTestLocks();
    const d = `${docId}-b`;
    const header = fakePendingHeader();

    // Open 3 concurrent slots.
    const s1 = await openCollabReplica({
      userId: "alice",
      docId: d,
      branchId,
      title: "Sheet",
      online: true,
      backend: "memory",
      locks,
    });
    const s2 = await openCollabReplica({
      userId: "alice",
      docId: d,
      branchId,
      title: "Sheet",
      online: true,
      backend: "memory",
      locks,
    });
    const s3 = await openCollabReplica({
      userId: "alice",
      docId: d,
      branchId,
      title: "Sheet",
      online: true,
      backend: "memory",
      locks,
    });

    // Slot 1 is clean (`<= 16` bytes), slot 2 and slot 3 both have unsent records (`> 16` bytes).
    await s1.rewritePending(header, makeCheckpoint(10));
    await s2.appendPending(new Uint8Array([9, 9, 9]), header);
    await s3.appendPending(new Uint8Array([8, 8, 8]), header);
    s1.close();
    s2.close();
    s3.close();

    // Next open must pick slot 2 (lowest slot with unsent WAL), keep slot 3 (also unsent), and remove clean slot 1.
    const reopened = await openCollabReplica({
      userId: "alice",
      docId: d,
      branchId,
      title: "Sheet",
      online: true,
      backend: "memory",
      locks,
    });
    expect(reopened.slot).toBe("2");
    expect(readMemoryFile(`collab/alice/${d}/replica-1/meta.json`)).toBeNull();
    expect(readMemoryFile(`collab/alice/${d}/replica-3/pending.wal`)?.byteLength).toBeGreaterThan(
      PENDING_WAL_HEADER_LEN,
    );
    reopened.close();
  });

  it("isolates storage directories by userId", async () => {
    const locks = createTestLocks();
    const d = `${docId}-c`;
    const alice = await openCollabReplica({
      userId: "alice",
      docId: d,
      branchId,
      title: "Shared",
      online: true,
      backend: "memory",
      locks,
    });
    const bob = await openCollabReplica({
      userId: "bob",
      docId: d,
      branchId,
      title: "Shared",
      online: true,
      backend: "memory",
      locks,
    });
    expect(alice.slot).toBe("1");
    expect(bob.slot).toBe("1");
    expect(readMemoryFile(`collab/alice/${d}/replica-1/meta.json`)).not.toBeNull();
    expect(readMemoryFile(`collab/bob/${d}/replica-1/meta.json`)).not.toBeNull();
    alice.close();
    bob.close();
  });

  it("blocks opening a brand-new replica offline when existing slots are locked, but allows reopening idle slot offline", async () => {
    const locks = createTestLocks();
    const d = `${docId}-d`;
    const first = await openCollabReplica({
      userId: "alice",
      docId: d,
      branchId,
      title: "OfflineDoc",
      online: true,
      backend: "memory",
      locks,
    });

    await expect(
      openCollabReplica({
        userId: "alice",
        docId: d,
        branchId,
        title: "OfflineDoc",
        online: false,
        backend: "memory",
        locks,
      }),
    ).rejects.toBeInstanceOf(CollabOfflineBlockedError);

    first.close();
    const reopened = await openCollabReplica({
      userId: "alice",
      docId: d,
      branchId,
      title: "OfflineDoc",
      online: false,
      backend: "memory",
      locks,
    });
    expect(reopened.slot).toBe("1");
    reopened.close();
  });

  it("saves rejected unsynced edits into a local rescue workbook", async () => {
    const locks = createTestLocks();
    const d = `${docId}-e`;
    const session = await openCollabReplica({
      userId: "alice",
      docId: d,
      branchId,
      title: "Quarterly",
      online: true,
      backend: "memory",
      locks,
    });

    const rescued = await session.saveUnsyncedRescue(
      new Uint8Array([10, 20, 30]),
      new Uint8Array([40, 50]),
      new Uint8Array(16),
    );
    expect(rescued.title).toBe("Unsynced changes - Quarterly");
    expect(readMemoryFile(`local/${rescued.localDocId}/snapshot-a.bin`)).toEqual(
      new Uint8Array([10, 20, 30]),
    );
    expect(readMemoryFile(`local/${rescued.localDocId}/rejected-pending.wal`)).toEqual(
      new Uint8Array([40, 50]),
    );
    expect(await readDocState(rescued.localDocId, "memory")).toMatchObject({
      activeSlot: "a",
      snapshotSeq: 0,
      lastFlushedSeq: 0,
      openingCount: 0,
    });
    const catalog = await readCatalog("memory");
    expect(catalog.some((entry) => entry.id === rescued.localDocId)).toBe(true);
    session.close();
  });
});
