import {
  type CollabEvent,
  Engine,
  type EngineCommand,
  encodePendingWalInit,
  encodeWalInit,
  HistoryPlayer,
  hasEnginePanicked,
  init,
  inspectWal,
  type SheetInfo,
  type SyncCommand,
} from "@reticula/protocol";
import wasmUrl from "@reticula/protocol/wasm?url";
import type {
  EngineMessage,
  HostCommand,
  SafeModeInfo,
  WorkerEnvelope,
  WorkerOutput,
} from "./client.ts";
import type { CollabReplicaSession } from "./collab-storage.ts";
import {
  detectStorageBackend,
  isQuotaExceededError,
  type LocalDocState,
  openSnapshotStreamSink,
  openWalHandle,
  readCatalog,
  readDocState,
  readSnapshotSlot,
  removeDocStorage,
  type SnapshotStreamSink,
  type StorageBackendKind,
  upsertCatalogEntry,
  type WalHandle,
  writeDocState,
  writeSnapshotSlotFull,
} from "./storage.ts";

interface WorkerScope {
  readonly name: string;
  onmessage: ((event: MessageEvent<WorkerEnvelope | EngineCommand>) => void) | null;
  postMessage(message: WorkerOutput, transfer?: Transferable[]): void;
  __reticulaOnWasmPanic?: (message: string) => void;
}

const scope = self as unknown as WorkerScope;
const TAB_ID = scope.name || Math.random().toString(36).slice(2, 10);

const SAMPLE_DOC_ID = "sample-1m";
const SAMPLE_SEED = 20261117;
const SAMPLE_ROWS = 1_000_000;
const WAL_COMPACT_TXNS = 500;
const WAL_COMPACT_BYTES = 8 * 1024 * 1024;
const TAKEOVER_LOCK_PATIENCE_MS = 2_500;
const TAKEOVER_FILE_PATIENCE_MS = 30_000;
const SNAPSHOT_SLICE_BUDGET_MS = 3.5;
const CSV_STEP_ROWS = 65_536;
const MAX_CSV_RAW_BYTES = 256 * 1024 * 1024;

interface ActiveDoc {
  docId: string;
  title: string;
  createdAt: number;
  state: LocalDocState;
  wal: WalHandle;
  currentSeq: number;
  txnsSinceSnapshot: number;
  awaitingInitialSettle: boolean;
}

interface ActiveSnapshotJob {
  docId: string;
  targetSlot: "a" | "b";
  snapSeq: number;
  sink: SnapshotStreamSink;
  startedAt: number;
  postSnapFrames: Uint8Array[];
}

let engine: Engine | null = null;
let backend: StorageBackendKind = "memory";
let stopped = false;
let activeDoc: ActiveDoc | null = null;
let activeSnap: ActiveSnapshotJob | null = null;
let collaborative = false;
let activeCollab: CollabReplicaSession | null = null;
let collabCheckpointTimer = 0;
let collabRemoteCommitsSinceCheckpoint = 0;
let lastSubscription: Extract<EngineCommand, { kind: "subscribe" }> | null = null;
let activeSheetId = 0;
let historyPlayer: HistoryPlayer | null = null;
let historyPreviewActive = false;

// Web Locks held by this worker (`lockName -> release callback`).
const heldLocks = new Map<string, () => void>();

// Cross-tab lock coordination channel (`14-m1b.md` 7.2).
const locksChannel =
  typeof BroadcastChannel !== "undefined" ? new BroadcastChannel("reticula:locks") : null;

// Zero-delay macrotask yielding channel (`MessageChannel` avoids 4 ms nested timer clamping).
const sliceChannel = new MessageChannel();
let sliceScheduled = false;

function scheduleWorkSlice(): void {
  if (stopped || sliceScheduled) {
    return;
  }
  if (!engine?.hasPendingRecalc() && !activeSnap) {
    return;
  }
  sliceScheduled = true;
  sliceChannel.port2.postMessage(0);
}

sliceChannel.port1.onmessage = () => {
  sliceScheduled = false;
  enqueueTask(async () => {
    if (stopped || !engine) {
      return;
    }
    if (engine.hasPendingRecalc()) {
      await runEngineCommand({ kind: "continueRecalc" });
    } else if (activeSnap) {
      await stepBackgroundSnapshot();
    }
    scheduleWorkSlice();
  });
};

// Serialized async task queue so OPFS/Lock operations and engine mutations never race.
let taskChain: Promise<void> = Promise.resolve();

function enqueueTask(fn: () => Promise<void>): void {
  taskChain = taskChain.then(fn).catch((err) => {
    fail(err);
  });
}

function lockNameOf(docId: string): string {
  return `reticula:local:${docId}`;
}

function acquireExclusiveLock(
  lockName: string,
  opts: { ifAvailable?: boolean; steal?: boolean; patienceMs?: number } = {},
): Promise<boolean> {
  if (typeof navigator === "undefined" || !navigator.locks?.request) {
    heldLocks.set(lockName, () => {});
    return Promise.resolve(true);
  }
  return new Promise((resolve, reject) => {
    let timer = 0;
    const reqOpts: LockOptions = { mode: "exclusive" };
    if (opts.ifAvailable) {
      reqOpts.ifAvailable = true;
    } else if (opts.steal) {
      reqOpts.steal = true;
    } else if (opts.patienceMs !== undefined && Number.isFinite(opts.patienceMs)) {
      const controller = new AbortController();
      reqOpts.signal = controller.signal;
      timer = setTimeout(() => controller.abort(), opts.patienceMs) as unknown as number;
    }

    navigator.locks
      .request(lockName, reqOpts, (lock) => {
        if (timer !== 0) {
          clearTimeout(timer);
          timer = 0;
        }
        if (!lock) {
          resolve(false);
          return undefined;
        }
        return new Promise<void>((release) => {
          heldLocks.set(lockName, release);
          resolve(true);
        });
      })
      .catch((err: unknown) => {
        if (timer !== 0) {
          clearTimeout(timer);
          timer = 0;
        }
        if (heldLocks.delete(lockName)) {
          // Lock was stolen after grant by another tab (`{ steal: true }`).
          onLockEvicted(lockName);
        } else {
          reject(err);
        }
      });
  });
}

function releaseLock(lockName: string): void {
  const release = heldLocks.get(lockName);
  heldLocks.delete(lockName);
  release?.();
}

function abortActiveSnapshot(): void {
  if (activeSnap) {
    try {
      engine?.abortSnapshotStream();
    } catch {
      // Ignore if engine is unavailable.
    }
    activeSnap.sink.abort();
    activeSnap = null;
  }
}

let watermarkTimer = 0;

function clearWatermarkTimer(): void {
  if (watermarkTimer !== 0) {
    clearTimeout(watermarkTimer);
    watermarkTimer = 0;
  }
}

function scheduleWatermarkFlush(docId: string): void {
  if (watermarkTimer !== 0) {
    return;
  }
  watermarkTimer = setTimeout(() => {
    watermarkTimer = 0;
    enqueueTask(async () => {
      const doc = activeDoc;
      if (!doc || doc.docId !== docId || doc.state.lastFlushedSeq >= doc.currentSeq) {
        return;
      }
      doc.state.lastFlushedSeq = doc.currentSeq;
      try {
        await writeDocState(doc.docId, doc.state, backend);
      } catch {
        // Ignore quota errors on watermark flush.
      }
    });
  }, 50) as unknown as number;
}

function clearCollabCheckpointTimer(): void {
  if (collabCheckpointTimer !== 0) {
    clearTimeout(collabCheckpointTimer);
    collabCheckpointTimer = 0;
  }
}

async function flushCollabCheckpointNow(eng: Engine): Promise<void> {
  clearCollabCheckpointTimer();
  const session = activeCollab;
  if (!session) {
    return;
  }
  collabRemoteCommitsSinceCheckpoint = 0;
  eng.handleSync({ kind: "checkpoint" });
  const cp = eng.takeCollabEvent();
  if (cp?.kind === "checkpoint") {
    try {
      await session.writeCheckpoint(cp.checkpoint);
    } catch (err) {
      if (isQuotaExceededError(err)) {
        scope.postMessage({
          kind: "quotaExceeded",
          message:
            "Browser storage quota was reached while saving collaborative checkpoint. Pending edits remain in memory.",
        });
        return;
      }
      throw err;
    }
  }
}

function scheduleCollabCheckpoint(): void {
  if (collabCheckpointTimer !== 0 || !activeCollab) {
    return;
  }
  collabCheckpointTimer = setTimeout(() => {
    collabCheckpointTimer = 0;
    enqueueTask(async () => {
      if (!stopped && engine && activeCollab) {
        await flushCollabCheckpointNow(engine);
      }
    });
  }, 1_500) as unknown as number;
}

async function closeActiveDoc(persistFinalState: boolean): Promise<void> {
  clearWatermarkTimer();
  clearCollabCheckpointTimer();
  abortActiveSnapshot();
  historyPreviewActive = false;
  historyPlayer?.free();
  historyPlayer = null;
  const collab = activeCollab;
  if (collab) {
    if (persistFinalState && engine) {
      try {
        await flushCollabCheckpointNow(engine);
      } catch {
        // Ignore checkpoint write errors during shutdown/switch.
      }
    }
    activeCollab = null;
    collaborative = false;
    collabRemoteCommitsSinceCheckpoint = 0;
    collab.close();
  }
  const doc = activeDoc;
  activeDoc = null;
  if (!doc) {
    return;
  }
  if (persistFinalState) {
    try {
      doc.state.lastFlushedSeq = doc.currentSeq;
      doc.state.openingCount = 0;
      await writeDocState(doc.docId, doc.state, backend);
    } catch {
      // Ignore write errors during shutdown/yield.
    }
  }
  doc.wal.close();
  releaseLock(lockNameOf(doc.docId));
}

function onLockEvicted(lockName: string): void {
  if (activeDoc && lockNameOf(activeDoc.docId) === lockName) {
    const docId = activeDoc.docId;
    abortActiveSnapshot();
    activeDoc.wal.close();
    activeDoc = null;
    scope.postMessage({ kind: "docYielded", docId, evicted: true });
  }
}

if (locksChannel) {
  locksChannel.onmessage = (
    event: MessageEvent<{ kind?: string; docId?: string; fromTabId?: string }>,
  ) => {
    const data = event.data;
    if (
      data?.kind === "yield" &&
      data.fromTabId !== TAB_ID &&
      data.docId &&
      activeDoc?.docId === data.docId
    ) {
      const targetDocId = data.docId;
      enqueueTask(async () => {
        if (activeDoc?.docId !== targetDocId) {
          return;
        }
        await closeActiveDoc(true);
        locksChannel.postMessage({ kind: "yielded", docId: targetDocId, fromTabId: TAB_ID });
        scope.postMessage({ kind: "docYielded", docId: targetDocId, evicted: false });
      });
    }
  };
}

// Register WASM panic hook (`14-m1b.md` 7.3) so OPFS writes halt immediately on trap.
scope.__reticulaOnWasmPanic = (message: string) => {
  stopped = true;
  clearCollabCheckpointTimer();
  if (activeSnap) {
    activeSnap.sink.abort();
    activeSnap = null;
  }
  if (activeCollab) {
    activeCollab.close();
    activeCollab = null;
    collaborative = false;
  }
  if (activeDoc) {
    // Do NOT clear `openingCount` so a startup crash increments the crash sentinel!
    activeDoc.wal.close();
    releaseLock(lockNameOf(activeDoc.docId));
    activeDoc = null;
  }
  scope.postMessage({ kind: "failed", message, panicked: true });
};

function flushHistoryPages(player: HistoryPlayer): void {
  const pages: ArrayBuffer[] = [];
  for (let page = player.takePage(); page; page = player.takePage()) {
    pages.push(page.buffer as ArrayBuffer);
  }
  for (const buffer of pages) {
    scope.postMessage(buffer, [buffer]);
  }
}

function flushDuePagesAndAux(eng: Engine): void {
  const pages: ArrayBuffer[] = [];
  for (let page = eng.takePage(); page; page = eng.takePage()) {
    pages.push(page.buffer as ArrayBuffer);
  }
  for (let aux = eng.takeAuxEvent(); aux; aux = eng.takeAuxEvent()) {
    scope.postMessage(aux);
  }
  for (const buffer of pages) {
    scope.postMessage(buffer, [buffer]);
  }
}

function collabTransfers(event: CollabEvent): ArrayBuffer[] {
  const buffers =
    event.kind === "checkpoint"
      ? [event.checkpoint.snapshot.buffer]
      : event.kind === "saveUnsynced"
        ? [event.snapshot.buffer, event.pendingWal.buffer]
        : "bytes" in event
          ? [event.bytes.buffer]
          : [];
  return buffers.filter((buffer): buffer is ArrayBuffer => buffer instanceof ArrayBuffer);
}

async function flushCollabEvents(eng: Engine): Promise<boolean> {
  const collab = activeCollab;
  if (!collab) {
    for (let event = eng.takeCollabEvent(); event; event = eng.takeCollabEvent()) {
      scope.postMessage({ channel: "collab", event }, collabTransfers(event));
    }
    return false;
  }

  const events: CollabEvent[] = [];
  for (let event = eng.takeCollabEvent(); event; event = eng.takeCollabEvent()) {
    events.push(event);
  }
  let wroteCheckpoint = false;
  for (const event of events) {
    switch (event.kind) {
      case "appendPending": {
        try {
          await collab.appendPending(event.bytes, encodePendingWalInit());
        } catch (err) {
          if (isQuotaExceededError(err)) {
            scope.postMessage({
              kind: "quotaExceeded",
              message:
                "Browser storage quota was reached while writing the pending edit log. Your workbook remains in memory.",
            });
            break;
          }
          throw err;
        }
        break;
      }
      case "saveUnsynced": {
        try {
          const rescued = await collab.saveUnsyncedRescue(
            event.snapshot,
            event.pendingWal,
            encodeWalInit(0),
          );
          scope.postMessage({ kind: "catalogListed", docs: rescued.docs });
          scope.postMessage({
            kind: "collabUnsyncedSaved",
            localDocId: rescued.localDocId,
            title: rescued.title,
            fromClientSeq: event.fromClientSeq,
          });
        } catch (err) {
          if (isQuotaExceededError(err)) {
            scope.postMessage({
              kind: "quotaExceeded",
              message:
                "Browser storage quota was reached while saving unsynced edits as a local workbook.",
            });
            break;
          }
          throw err;
        }
        break;
      }
      case "replacePending": {
        let checkpoint: Parameters<typeof collab.rewritePending>[1];
        if (collab.checkpointNeeded(event.info)) {
          eng.handleSync({ kind: "checkpoint" });
          const cp = eng.takeCollabEvent();
          if (cp?.kind === "checkpoint") {
            checkpoint = cp.checkpoint;
            wroteCheckpoint = true;
          }
        }
        try {
          await collab.rewritePending(event.bytes, checkpoint);
          if (checkpoint) {
            collabRemoteCommitsSinceCheckpoint = 0;
          }
        } catch (err) {
          if (isQuotaExceededError(err)) {
            scope.postMessage({
              kind: "quotaExceeded",
              message: "Browser storage quota was reached while updating collaborative storage.",
            });
            break;
          }
          throw err;
        }
        break;
      }
      case "state": {
        if (
          event.connection === "online" &&
          event.replica !== null &&
          collab.persistedInfo()?.replica !== event.replica
        ) {
          await flushCollabCheckpointNow(eng);
          wroteCheckpoint = true;
        }
        scope.postMessage({ channel: "collab", event });
        break;
      }
      case "epochChanged": {
        collabRemoteCommitsSinceCheckpoint = 0;
        scope.postMessage({ channel: "collab", event });
        break;
      }
      default: {
        scope.postMessage({ channel: "collab", event }, collabTransfers(event));
        break;
      }
    }
  }
  return wroteCheckpoint;
}

async function drainWalFrames(eng: Engine, latestSheet?: SheetInfo): Promise<void> {
  const doc = activeDoc;
  if (!doc) {
    for (let f = eng.takePendingWalFrame(); f; f = eng.takePendingWalFrame()) {
      // Discard if operating without an open local doc.
    }
    return;
  }

  let wroteAny = false;
  for (let frame = eng.takePendingWalFrame(); frame; frame = eng.takePendingWalFrame()) {
    try {
      await doc.wal.append(frame);
      if (activeSnap && activeSnap.docId === doc.docId) {
        activeSnap.postSnapFrames.push(frame.slice());
      }
      doc.txnsSinceSnapshot += 1;
      wroteAny = true;
    } catch (err) {
      if (isQuotaExceededError(err)) {
        scope.postMessage({
          kind: "quotaExceeded",
          message:
            "Browser storage quota was reached while writing the edit log. Your workbook is still safe in memory — export a .rtcl snapshot or delete unused local workbooks.",
        });
        return;
      }
      throw err;
    }
  }

  if (latestSheet) {
    const seqNum = Number(latestSheet.walSeq);
    if (Number.isFinite(seqNum) && seqNum > doc.currentSeq) {
      doc.currentSeq = seqNum;
    }
  }
  if (wroteAny && doc.currentSeq > doc.state.lastFlushedSeq) {
    scheduleWatermarkFlush(doc.docId);
  }

  if (
    wroteAny &&
    !activeSnap &&
    (doc.txnsSinceSnapshot >= WAL_COMPACT_TXNS || doc.wal.byteLength >= WAL_COMPACT_BYTES)
  ) {
    await startBackgroundSnapshot(eng, doc);
  }
}

async function settleOpeningSentinelIfReady(eng: Engine): Promise<void> {
  const doc = activeDoc;
  if (!doc?.awaitingInitialSettle || eng.hasPendingRecalc()) {
    return;
  }
  doc.awaitingInitialSettle = false;
  if (doc.state.openingCount !== 0) {
    doc.state.openingCount = 0;
    try {
      await writeDocState(doc.docId, doc.state, backend);
    } catch {
      // Ignore quota error on sentinel clear.
    }
  }
}

async function startBackgroundSnapshot(eng: Engine, doc: ActiveDoc): Promise<void> {
  if (activeSnap || stopped) {
    return;
  }
  const targetSlot: "a" | "b" = doc.state.activeSlot === "a" ? "b" : "a";
  try {
    const sink = await openSnapshotStreamSink(doc.docId, targetSlot, backend);
    const snapSeq = eng.startSnapshotStream();
    activeSnap = {
      docId: doc.docId,
      targetSlot,
      snapSeq,
      sink,
      startedAt: performance.now(),
      postSnapFrames: [],
    };
    scheduleWorkSlice();
  } catch (err) {
    if (isQuotaExceededError(err)) {
      scope.postMessage({
        kind: "quotaExceeded",
        message:
          "Browser storage quota was reached while compacting a snapshot. Your workbook remains intact in memory — export .rtcl to save to disk.",
      });
      return;
    }
    throw err;
  }
}

async function stepBackgroundSnapshot(): Promise<void> {
  const job = activeSnap;
  const eng = engine;
  if (!job || !eng) {
    return;
  }
  const doc = activeDoc;
  if (!doc || doc.docId !== job.docId) {
    abortActiveSnapshot();
    return;
  }

  const sliceStart = performance.now();
  try {
    while (performance.now() - sliceStart < SNAPSHOT_SLICE_BUDGET_MS) {
      const step = eng.stepSnapshotStream(4);
      if (step.kind === "chunk") {
        const chunk = eng.takeSnapshotChunk();
        if (chunk && chunk.byteLength > 0) {
          await job.sink.writeAt(chunk, step.offset);
        }
      } else if (step.kind === "finished") {
        // `SnapshotStep::Finished` returns the file header + directory at byte offset 0!
        const chunk = eng.takeSnapshotChunk();
        if (chunk && chunk.byteLength > 0) {
          await job.sink.writeAt(chunk, 0);
        }
        await job.sink.finish(step.totalLen);
        activeSnap = null;

        // 1. Switch `state.json` to `targetSlot` first (`scan_wal` handles prefix overlap!).
        doc.state = {
          activeSlot: job.targetSlot,
          snapshotSeq: job.snapSeq,
          lastFlushedSeq: Math.max(job.snapSeq, doc.currentSeq),
          openingCount: 0,
        };
        await writeDocState(doc.docId, doc.state, backend);

        // 2. Compact `wal.log` to `encodeWalInit(snapSeq)` + any frames committed during stream.
        const header = encodeWalInit(job.snapSeq);
        let totalWal = header.byteLength;
        for (const f of job.postSnapFrames) {
          totalWal += f.byteLength;
        }
        const nextWal = new Uint8Array(totalWal);
        nextWal.set(header, 0);
        let pos = header.byteLength;
        for (const f of job.postSnapFrames) {
          nextWal.set(f, pos);
          pos += f.byteLength;
        }
        await doc.wal.rewrite(nextWal);
        doc.txnsSinceSnapshot = job.postSnapFrames.length;

        const now = Date.now();
        await upsertCatalogEntry(
          {
            id: doc.docId,
            title: doc.title,
            createdAt: doc.createdAt,
            updatedAt: now,
            sizeBytes: step.totalLen + nextWal.byteLength,
          },
          backend,
        );

        scope.postMessage({
          kind: "docSaved",
          docId: doc.docId,
          slot: job.targetSlot,
          byteLen: step.totalLen,
          elapsedMs: performance.now() - job.startedAt,
        });
        return;
      }
    }
  } catch (err) {
    abortActiveSnapshot();
    if (isQuotaExceededError(err)) {
      scope.postMessage({
        kind: "quotaExceeded",
        message:
          "Browser storage quota was reached while writing snapshot. Previous snapshot and edit log remain intact — export .rtcl to save to disk.",
      });
      return;
    }
    throw err;
  }
}

function sheetFromEvent(event: EngineMessage | undefined): SheetInfo | undefined {
  if (!event) {
    return undefined;
  }
  switch (event.kind) {
    case "ready":
    case "sheetChanged":
    case "clipboardPasted":
    case "replaceCompleted":
    case "recalcDone":
      return event.sheet;
    default:
      return undefined;
  }
}

async function runEngineCommand(command: EngineCommand): Promise<void> {
  if (stopped || !engine) {
    return;
  }
  if (command.kind === "subscribe") {
    lastSubscription = command;
    activeSheetId = command.sheet ?? activeSheetId;
  }
  if (historyPreviewActive && historyPlayer) {
    if (command.kind === "subscribe") {
      historyPlayer.subscribePages(
        command.sheet ?? activeSheetId,
        command.epoch,
        command.firstRow,
        command.rowCount,
        command.firstCol,
        command.colCount,
      );
      flushHistoryPages(historyPlayer);
      return;
    }
    if (command.kind === "switchSheet") {
      try {
        const sheet = historyPlayer.setSheet(command.sheet);
        activeSheetId = sheet.sheet;
        scope.postMessage({
          kind: "historyReady",
          seq: String(historyPlayer.currentSeq()),
          sheet,
          elapsedMs: 0,
        });
        flushHistoryPages(historyPlayer);
      } catch (err) {
        scope.postMessage({
          kind: "rejected",
          reason: err instanceof Error ? err.message : String(err),
        });
      }
      return;
    }
    if (command.kind === "inspectCell") {
      try {
        const ev = historyPlayer.inspectCell(command.sheet, command.rowId, command.colId);
        if (ev) {
          scope.postMessage(ev);
        }
      } catch {
        // Ignore invalid cell inspection while scrubbing history.
      }
      return;
    }
    if (command.kind !== "setEvalEnv") {
      scope.postMessage({
        kind: "rejected",
        reason: "Return to the live workbook before editing.",
      });
    }
    return;
  }
  if (collaborative && (command.kind === "openEmpty" || command.kind === "openSample")) {
    scope.postMessage({ kind: "rejected", reason: "Open a local workbook in a separate worker." });
    return;
  }
  const event = engine.handle(command);
  if (event) {
    scope.postMessage(event);
  }
  const updatedSheet = sheetFromEvent(event);
  if (updatedSheet) {
    activeSheetId = updatedSheet.sheet;
  }
  await drainWalFrames(engine, updatedSheet);
  if (collaborative) {
    await flushCollabEvents(engine);
  }
  flushDuePagesAndAux(engine);
  await settleOpeningSentinelIfReady(engine);
  scheduleWorkSlice();
}

async function runSyncCommand(command: SyncCommand): Promise<void> {
  if (stopped || !engine) return;
  if (command.kind === "open" && (activeDoc || collaborative)) {
    scope.postMessage({
      kind: "rejected",
      reason: "Open a collaborative workbook in a fresh worker.",
    });
    return;
  }
  const event = engine.handleSync(command);
  if (event?.kind === "ready") collaborative = true;
  if (event) {
    if (!historyPreviewActive || (event.kind !== "ready" && event.kind !== "sheetChanged")) {
      scope.postMessage(event);
    }
  }
  const wroteCheckpoint = await flushCollabEvents(engine);
  if (
    activeCollab &&
    !wroteCheckpoint &&
    command.kind === "receive" &&
    event?.kind === "sheetChanged"
  ) {
    collabRemoteCommitsSinceCheckpoint += 1;
    if (collabRemoteCommitsSinceCheckpoint >= 20) {
      await flushCollabCheckpointNow(engine);
    } else {
      scheduleCollabCheckpoint();
    }
  }
  if (!historyPreviewActive) {
    flushDuePagesAndAux(engine);
  }
  scheduleWorkSlice();
}

async function initializeFreshDoc(
  eng: Engine,
  docId: string,
  title: string,
  wal: WalHandle,
  template: "sample" | "empty",
): Promise<void> {
  const cmd: EngineCommand =
    template === "sample"
      ? { kind: "openSample", seed: SAMPLE_SEED, rows: SAMPLE_ROWS }
      : { kind: "openEmpty" };
  const event = eng.handle(cmd);
  if (event?.kind !== "ready") {
    throw new Error("Engine did not emit ready on fresh workbook initialization");
  }
  performance.mark("reticula:sample-ready");

  const now = Date.now();
  const state: LocalDocState = {
    activeSlot: "a",
    snapshotSeq: 0,
    lastFlushedSeq: 0,
    openingCount: 0,
  };

  activeDoc = {
    docId,
    title,
    createdAt: now,
    state,
    wal,
    currentSeq: 0,
    txnsSinceSnapshot: 0,
    awaitingInitialSettle: false,
  };

  // Emit `ready` and push initial viewport pages BEFORE initial OPFS stub writes so
  // cold-start first-screen paint has zero storage latency on the critical path!
  scope.postMessage({
    kind: "docOpened",
    docId,
    title,
    storageBackend: backend,
    walRecords: 0,
    activeSlot: "a",
  });
  scope.postMessage(event);

  // Default 1M sample writes a ~120B SeedOrigin stub (`14-m1b.md` 7.1).
  const snapBytes =
    template === "sample"
      ? (eng.encodeSeedStub() ?? eng.encodeSnapshotFull(false))
      : eng.encodeSnapshotFull(false);
  const walInit = encodeWalInit(0);

  try {
    await writeSnapshotSlotFull(docId, "a", snapBytes, backend);
    await wal.rewrite(walInit);
    await writeDocState(docId, state, backend);
    await upsertCatalogEntry(
      {
        id: docId,
        title,
        createdAt: now,
        updatedAt: now,
        sizeBytes: snapBytes.byteLength + walInit.byteLength,
      },
      backend,
    );
  } catch (err) {
    if (isQuotaExceededError(err)) {
      scope.postMessage({
        kind: "quotaExceeded",
        message:
          "Browser storage quota was reached while initializing local storage. Your workbook is running in memory — export .rtcl to save.",
      });
    } else {
      throw err;
    }
  }

  scheduleWorkSlice();
}

async function finishOpeningWithLock(
  docId: string,
  requestedTitle: string | undefined,
  safeRequested: boolean,
  filePatienceMs: number,
): Promise<void> {
  if (!engine) {
    return;
  }
  const catalog = await readCatalog(backend);
  const existingEntry = catalog.find((c) => c.id === docId);
  const title =
    requestedTitle ??
    existingEntry?.title ??
    (docId === SAMPLE_DOC_ID ? "Sample sales data" : docId);
  const createdAt = existingEntry?.createdAt ?? Date.now();

  const wal = await openWalHandle(docId, backend, filePatienceMs, () => {
    scope.postMessage({ kind: "docTakingOver", docId, waitingForFile: true });
  });

  let state: LocalDocState | null = null;
  try {
    state = await readDocState(docId, backend);
  } catch (err) {
    // Corrupted `state.json` -> enter Safe Mode so user can export raw files!
    activeDoc = {
      docId,
      title,
      createdAt,
      state: { activeSlot: "a", snapshotSeq: 0, lastFlushedSeq: 0, openingCount: 1 },
      wal,
      currentSeq: 0,
      txnsSinceSnapshot: 0,
      awaitingInitialSettle: false,
    };
    const info: SafeModeInfo = {
      docId,
      title,
      reason: "corruptSnapshot",
      detail: err instanceof Error ? err.message : "state.json is unreadable.",
      openingCount: 1,
      salvageableMid: false,
      tailTxns: [],
    };
    scope.postMessage({ kind: "safeMode", info });
    return;
  }

  if (!state) {
    // Brand-new local workbook!
    await initializeFreshDoc(
      engine,
      docId,
      title,
      wal,
      docId === SAMPLE_DOC_ID ? "sample" : "empty",
    );
    return;
  }

  // Increment startup crash sentinel BEFORE decoding/recalculating (`14-m1b.md` 7.3).
  const nextOpeningCount = state.openingCount + 1;
  state.openingCount = nextOpeningCount;
  try {
    await writeDocState(docId, state, backend);
  } catch {
    // Ignore quota errors on sentinel bump.
  }

  const snapBytes = await readSnapshotSlot(docId, state.activeSlot, backend);
  const walBytes = await wal.readAll();

  if (!snapBytes || snapBytes.byteLength === 0) {
    activeDoc = {
      docId,
      title,
      createdAt,
      state,
      wal,
      currentSeq: state.snapshotSeq,
      txnsSinceSnapshot: 0,
      awaitingInitialSettle: false,
    };
    scope.postMessage({
      kind: "safeMode",
      info: {
        docId,
        title,
        reason: "corruptSnapshot",
        detail: `Snapshot slot '${state.activeSlot}' is missing or empty.`,
        openingCount: nextOpeningCount,
        salvageableMid: false,
        tailTxns: [],
      },
    });
    return;
  }

  const walCheck = inspectWal(walBytes, state.snapshotSeq, state.lastFlushedSeq);

  if (safeRequested || nextOpeningCount >= 2 || walCheck.status === "corrupted") {
    activeDoc = {
      docId,
      title,
      createdAt,
      state,
      wal,
      currentSeq: walCheck.status === "valid" ? Number(walCheck.lastSeq) : state.snapshotSeq,
      txnsSinceSnapshot: walCheck.status === "valid" ? walCheck.recordCount : 0,
      awaitingInitialSettle: false,
    };
    const reason =
      walCheck.status === "corrupted"
        ? "corruptWal"
        : nextOpeningCount >= 2
          ? "crashLoop"
          : "explicit";
    const detail =
      walCheck.status === "corrupted"
        ? walCheck.reason
        : nextOpeningCount >= 2
          ? `Previous open attempt terminated unexpectedly while loading or recalculating (sentinel = ${nextOpeningCount}).`
          : "Safe Mode was requested via ?safe=1.";
    const tailTxns = walCheck.status === "valid" ? walCheck.tailTxns : walCheck.salvageableTailTxns;

    scope.postMessage({
      kind: "safeMode",
      info: {
        docId,
        title,
        reason,
        detail,
        openingCount: nextOpeningCount,
        salvageableMid: walCheck.status === "corrupted" && walCheck.salvageableCount > 0,
        tailTxns,
      },
    });
    return;
  }

  const shouldCompactOnColdOpen =
    walCheck.recordCount >= WAL_COMPACT_TXNS || walBytes.byteLength >= WAL_COMPACT_BYTES;

  try {
    const outcome = engine.loadSnapshotAndWal(
      snapBytes,
      walBytes,
      state.snapshotSeq,
      state.lastFlushedSeq,
      0,
      true,
      false,
      shouldCompactOnColdOpen,
    );

    if (outcome.truncateWalToBytes !== undefined) {
      await wal.truncate(outcome.truncateWalToBytes);
    }

    const loadedSeq = Number(outcome.sheet.walSeq);
    activeDoc = {
      docId,
      title,
      createdAt,
      state,
      wal,
      currentSeq: Number.isFinite(loadedSeq) ? loadedSeq : state.snapshotSeq + outcome.replayedTxns,
      txnsSinceSnapshot: outcome.replayedTxns,
      awaitingInitialSettle: true,
    };

    scope.postMessage({
      kind: "docOpened",
      docId,
      title,
      storageBackend: backend,
      walRecords: outcome.replayedTxns,
      activeSlot: state.activeSlot,
    });
    scope.postMessage({ kind: "ready", info: outcome.info, sheet: outcome.sheet });
    await settleOpeningSentinelIfReady(engine);

    if (shouldCompactOnColdOpen) {
      await startBackgroundSnapshot(engine, activeDoc);
    }
    scheduleWorkSlice();
  } catch (err) {
    activeDoc = {
      docId,
      title,
      createdAt,
      state,
      wal,
      currentSeq: Number(walCheck.lastSeq),
      txnsSinceSnapshot: walCheck.recordCount,
      awaitingInitialSettle: false,
    };
    scope.postMessage({
      kind: "safeMode",
      info: {
        docId,
        title,
        reason: "corruptSnapshot",
        detail: err instanceof Error ? err.message : String(err),
        openingCount: nextOpeningCount,
        salvageableMid: false,
        tailTxns: walCheck.tailTxns,
      },
    });
  }
}

async function handleOpenDoc(
  docId: string,
  title: string | undefined,
  safeRequested: boolean,
): Promise<void> {
  await closeActiveDoc(true);
  const lockName = lockNameOf(docId);
  const granted = await acquireExclusiveLock(lockName, { ifAvailable: true });
  if (!granted) {
    const catalog = await readCatalog(backend);
    const entryTitle =
      title ??
      catalog.find((c) => c.id === docId)?.title ??
      (docId === SAMPLE_DOC_ID ? "Sample sales data" : docId);
    scope.postMessage({ kind: "docBlocked", docId, title: entryTitle });
    return;
  }
  await finishOpeningWithLock(docId, title, safeRequested, 500);
}

async function handleTakeOverDoc(docId: string): Promise<void> {
  await closeActiveDoc(true);
  const lockName = lockNameOf(docId);
  scope.postMessage({ kind: "docTakingOver", docId, waitingForFile: false });
  locksChannel?.postMessage({ kind: "yield", docId, fromTabId: TAB_ID });

  let gotLock = false;
  try {
    gotLock = await acquireExclusiveLock(lockName, { patienceMs: TAKEOVER_LOCK_PATIENCE_MS });
  } catch (err) {
    const isAbort =
      err !== null &&
      typeof err === "object" &&
      "name" in err &&
      (err as { name: unknown }).name === "AbortError";
    if (!isAbort) {
      throw err;
    }
  }
  if (!gotLock) {
    await acquireExclusiveLock(lockName, { steal: true });
  }
  await finishOpeningWithLock(docId, undefined, false, TAKEOVER_FILE_PATIENCE_MS);
}

async function handleSafeModeRecover(
  docId: string,
  mode:
    | { readonly type: "openFrozen" }
    | { readonly type: "skipTail"; readonly skipCount: number; readonly salvageMid: boolean },
): Promise<void> {
  const doc = activeDoc;
  if (!engine || !doc || doc.docId !== docId) {
    return;
  }
  const snapBytes = await readSnapshotSlot(docId, doc.state.activeSlot, backend);
  if (!snapBytes) {
    throw new Error("Snapshot file is missing; use Export Raw Files instead.");
  }
  const walBytes = await doc.wal.readAll();

  const recalculate = mode.type !== "openFrozen";
  const skipTail = mode.type === "skipTail" ? mode.skipCount : 0;
  const salvageMid = mode.type === "skipTail" ? mode.salvageMid : true;

  const outcome = engine.loadSnapshotAndWal(
    snapBytes,
    walBytes,
    doc.state.snapshotSeq,
    0,
    skipTail,
    recalculate,
    salvageMid,
    false,
  );

  doc.currentSeq = Number(outcome.sheet.walSeq);
  doc.txnsSinceSnapshot = outcome.replayedTxns;
  doc.state.openingCount = 0;
  await writeDocState(docId, doc.state, backend);

  // If the user explicitly skipped corrupted/bad tail transactions, materialize a clean snapshot!
  if (mode.type === "skipTail") {
    await startBackgroundSnapshot(engine, doc);
    while (activeSnap && activeSnap.docId === doc.docId) {
      await stepBackgroundSnapshot();
    }
  }

  scope.postMessage({
    kind: "docOpened",
    docId,
    title: doc.title,
    storageBackend: backend,
    walRecords: outcome.replayedTxns,
    activeSlot: doc.state.activeSlot,
  });
  scope.postMessage({ kind: "ready", info: outcome.info, sheet: outcome.sheet });
  scheduleWorkSlice();
}

async function handleImportRtcl(docId: string, title: string, bytes: ArrayBuffer): Promise<void> {
  if (!engine) {
    return;
  }
  await closeActiveDoc(true);
  const lockName = lockNameOf(docId);
  const granted = await acquireExclusiveLock(lockName, { ifAvailable: true });
  if (!granted) {
    scope.postMessage({ kind: "docBlocked", docId, title });
    return;
  }
  const wal = await openWalHandle(docId, backend, 500);
  const snapView = new Uint8Array(bytes);
  const walInit = encodeWalInit(0);
  const outcome = engine.loadSnapshotAndWal(snapView, walInit, 0, 0, 0, true, false, false);

  const now = Date.now();
  const state: LocalDocState = {
    activeSlot: "a",
    snapshotSeq: 0,
    lastFlushedSeq: 0,
    openingCount: 0,
  };
  await writeSnapshotSlotFull(docId, "a", snapView, backend);
  await wal.rewrite(walInit);
  await writeDocState(docId, state, backend);
  const docs = await upsertCatalogEntry(
    {
      id: docId,
      title,
      createdAt: now,
      updatedAt: now,
      sizeBytes: snapView.byteLength + walInit.byteLength,
    },
    backend,
  );

  activeDoc = {
    docId,
    title,
    createdAt: now,
    state,
    wal,
    currentSeq: 0,
    txnsSinceSnapshot: 0,
    awaitingInitialSettle: false,
  };
  scope.postMessage({ kind: "catalogListed", docs });
  scope.postMessage({
    kind: "docOpened",
    docId,
    title,
    storageBackend: backend,
    walRecords: 0,
    activeSlot: "a",
  });
  scope.postMessage({ kind: "ready", info: outcome.info, sheet: outcome.sheet });
  scheduleWorkSlice();
}

async function handleImportCsv(
  docId: string,
  title: string,
  sheetName: string,
  bytes: ArrayBuffer,
): Promise<void> {
  if (!engine) {
    return;
  }
  if (bytes.byteLength > MAX_CSV_RAW_BYTES) {
    scope.postMessage({
      kind: "rejected",
      reason: "CSV exceeds the 256 MiB local workbook import limit.",
    });
    return;
  }
  await closeActiveDoc(true);
  const lockName = lockNameOf(docId);
  const granted = await acquireExclusiveLock(lockName, { ifAvailable: true });
  if (!granted) {
    scope.postMessage({ kind: "docBlocked", docId, title });
    return;
  }
  const wal = await openWalHandle(docId, backend, 500);
  try {
    engine.startCsvImport(sheetName, new Uint8Array(bytes));
  } catch (err) {
    wal.close();
    releaseLock(lockName);
    scope.postMessage({
      kind: "rejected",
      reason: err instanceof Error ? err.message : String(err),
    });
    return;
  }

  for (;;) {
    const step = engine.stepCsvImport(CSV_STEP_ROWS);
    if (step.kind === "progress") {
      scope.postMessage({
        kind: "csvImportProgress",
        rowsParsed: step.rowsParsed,
        percent: step.percent,
      });
      // Yield macrotask so progress updates paint smoothly on 1M-row imports.
      await new Promise<void>((r) => setTimeout(r, 0));
    } else {
      const walInit = encodeWalInit(0);
      const now = Date.now();
      const state: LocalDocState = {
        activeSlot: "a",
        snapshotSeq: 0,
        lastFlushedSeq: 0,
        openingCount: 0,
      };
      await wal.rewrite(walInit);
      await writeDocState(docId, state, backend);
      activeDoc = {
        docId,
        title,
        createdAt: now,
        state,
        wal,
        currentSeq: 0,
        txnsSinceSnapshot: 0,
        awaitingInitialSettle: false,
      };
      // Stream initial snapshot into slot "a" via CoW encoder so we never duplicate 1M-row memory!
      const sink = await openSnapshotStreamSink(docId, "a", backend);
      engine.startSnapshotStream();
      for (;;) {
        const s = engine.stepSnapshotStream(8);
        if (s.kind === "chunk") {
          const c = engine.takeSnapshotChunk();
          if (c && c.byteLength > 0) {
            await sink.writeAt(c, s.offset);
          }
        } else if (s.kind === "finished") {
          const c = engine.takeSnapshotChunk();
          if (c && c.byteLength > 0) {
            await sink.writeAt(c, 0);
          }
          await sink.finish(s.totalLen);
          const docs = await upsertCatalogEntry(
            {
              id: docId,
              title,
              createdAt: now,
              updatedAt: now,
              sizeBytes: s.totalLen + walInit.byteLength,
            },
            backend,
          );
          scope.postMessage({ kind: "catalogListed", docs });
          break;
        }
      }

      scope.postMessage({
        kind: "docOpened",
        docId,
        title,
        storageBackend: backend,
        walRecords: 0,
        activeSlot: "a",
      });
      scope.postMessage({ kind: "ready", info: step.info, sheet: step.sheet });
      scheduleWorkSlice();
      return;
    }
  }
}

async function handleHostCommand(cmd: HostCommand): Promise<void> {
  if (stopped || !engine) {
    return;
  }
  switch (cmd.kind) {
    case "openDoc":
      await handleOpenDoc(cmd.docId, cmd.title, cmd.safeRequested ?? false);
      break;
    case "openCollab": {
      await closeActiveDoc(true);
      try {
        if (cmd.readOnly) {
          const readyEv = engine.handleSync({
            kind: "open",
            docId: cmd.docId,
            branchId: cmd.branchId,
            info: undefined,
            snapshot: new Uint8Array(),
            pendingWal: new Uint8Array(),
          });
          if (readyEv?.kind !== "ready") {
            scope.postMessage(
              readyEv ?? {
                kind: "rejected",
                reason: "Could not open read-only collaborative view.",
              },
            );
            return;
          }
          activeCollab = null;
          collaborative = true;
          collabRemoteCommitsSinceCheckpoint = 0;
          scope.postMessage({
            kind: "collabOpened",
            docId: cmd.docId,
            title: cmd.title,
            slot: "memory",
            replica: null,
            storageBackend: "memory",
          });
          scope.postMessage(readyEv);
          await flushCollabEvents(engine);
          flushDuePagesAndAux(engine);
          scheduleWorkSlice();
          return;
        }
        const { openCollabReplica } = await import("./collab-storage.ts");
        const replicaSession = await openCollabReplica({
          userId: cmd.userId,
          docId: cmd.docId,
          branchId: cmd.branchId,
          title: cmd.title,
          online: cmd.online,
          backend,
          locks: { acquire: acquireExclusiveLock, release: releaseLock },
        });
        const readyEv = engine.handleSync({
          kind: "open",
          docId: cmd.docId,
          branchId: cmd.branchId,
          info: replicaSession.initialInfo ?? undefined,
          snapshot: replicaSession.initialSnapshot,
          pendingWal: replicaSession.initialPendingWal,
        });
        if (readyEv?.kind !== "ready") {
          replicaSession.close();
          scope.postMessage(
            readyEv ?? {
              kind: "rejected",
              reason: "Could not open collaborative replica.",
            },
          );
          return;
        }
        activeCollab = replicaSession;
        collaborative = true;
        collabRemoteCommitsSinceCheckpoint = 0;
        scope.postMessage({
          kind: "collabOpened",
          docId: cmd.docId,
          title: replicaSession.title(),
          slot: replicaSession.slot,
          replica: replicaSession.initialInfo?.replica ?? null,
          storageBackend: backend,
        });
        scope.postMessage(readyEv);
        await flushCollabEvents(engine);
        flushDuePagesAndAux(engine);
        scheduleWorkSlice();
      } catch (err) {
        scope.postMessage({
          kind: "rejected",
          reason: err instanceof Error ? err.message : String(err),
        });
      }
      break;
    }
    case "takeOverDoc":
      await handleTakeOverDoc(cmd.docId);
      break;
    case "createDoc": {
      await closeActiveDoc(true);
      const granted = await acquireExclusiveLock(lockNameOf(cmd.docId), { ifAvailable: true });
      if (!granted) {
        scope.postMessage({ kind: "docBlocked", docId: cmd.docId, title: cmd.title });
        return;
      }
      const wal = await openWalHandle(cmd.docId, backend, 500);
      await initializeFreshDoc(engine, cmd.docId, cmd.title, wal, cmd.template);
      scope.postMessage({ kind: "catalogListed", docs: await readCatalog(backend) });
      break;
    }
    case "renameDoc": {
      const catalog = await readCatalog(backend);
      const found = catalog.find((c) => c.id === cmd.docId);
      if (found) {
        const docs = await upsertCatalogEntry(
          { ...found, title: cmd.title, updatedAt: Date.now() },
          backend,
        );
        if (activeDoc?.docId === cmd.docId) {
          activeDoc.title = cmd.title;
        }
        scope.postMessage({ kind: "catalogListed", docs });
      }
      break;
    }
    case "deleteDoc": {
      const deletingActive = activeDoc?.docId === cmd.docId;
      if (deletingActive) {
        await closeActiveDoc(false);
      }
      const docs = await removeDocStorage(cmd.docId, backend);
      scope.postMessage({ kind: "catalogListed", docs });
      if (deletingActive) {
        const nextDoc = docs[0]?.id ?? SAMPLE_DOC_ID;
        await handleOpenDoc(nextDoc, docs[0]?.title, false);
      }
      break;
    }
    case "listDocs": {
      scope.postMessage({ kind: "catalogListed", docs: await readCatalog(backend) });
      break;
    }
    case "safeModeRecover": {
      try {
        await handleSafeModeRecover(cmd.docId, cmd.mode);
      } catch (err) {
        scope.postMessage({
          kind: "rejected",
          reason: err instanceof Error ? err.message : String(err),
        });
      }
      break;
    }
    case "exportRtcl":
    case "exportXlsxSnapshot": {
      const isXlsx = cmd.kind === "exportXlsxSnapshot";
      const snap = engine.encodeSnapshotFull(true);
      const safeName = (activeCollab?.title() ?? activeDoc?.title ?? "workbook")
        .replace(/[^a-zA-Z0-9._-]+/g, "-")
        .replace(/^-|-$/g, "");
      const copy = new Uint8Array(snap.byteLength);
      copy.set(snap);
      scope.postMessage(
        {
          kind: "fileExported",
          format: isXlsx ? "xlsxSnapshot" : "rtcl",
          filename: `${safeName || "workbook"}.${isXlsx ? "xlsx" : "rtcl"}`,
          bytes: copy.buffer,
        },
        [copy.buffer],
      );
      break;
    }
    case "importRtcl": {
      try {
        await handleImportRtcl(cmd.docId, cmd.title, cmd.bytes);
      } catch (err) {
        scope.postMessage({
          kind: "rejected",
          reason: err instanceof Error ? err.message : String(err),
        });
      }
      break;
    }
    case "exportCsv": {
      const csvBytes = engine.exportCsv(cmd.sheet);
      const safeName = (activeCollab?.title() ?? activeDoc?.title ?? "sheet")
        .replace(/[^a-zA-Z0-9._-]+/g, "-")
        .replace(/^-|-$/g, "");
      const copy = new Uint8Array(csvBytes.byteLength);
      copy.set(csvBytes);
      scope.postMessage(
        {
          kind: "fileExported",
          format: "csv",
          filename: `${safeName || "sheet"}.csv`,
          bytes: copy.buffer,
        },
        [copy.buffer],
      );
      break;
    }
    case "importCsv":
      await handleImportCsv(cmd.docId, cmd.title, cmd.sheetName, cmd.bytes);
      break;
    case "forceCompactSnapshot":
      if (activeDoc && !activeSnap) {
        await startBackgroundSnapshot(engine, activeDoc);
      }
      break;
    case "flushAndCloseForUpdate":
      await closeActiveDoc(true);
      scope.postMessage({ kind: "closedForUpdate" });
      break;
    case "historySeek": {
      if (!collaborative) {
        scope.postMessage({
          kind: "rejected",
          reason: "History playback requires an active collaborative workbook.",
        });
        return;
      }
      try {
        const started = performance.now();
        if (!historyPlayer) {
          historyPlayer = new HistoryPlayer();
        }
        const player = historyPlayer;
        const targetSeq = BigInt(cmd.targetSeq);
        const fetchInit: RequestInit = cmd.shareToken
          ? { credentials: "same-origin", headers: { "X-Reticula-Share": cmd.shareToken } }
          : { credentials: "same-origin" };
        if (cmd.snapshotSeq === null) {
          if (!player.hasSnapshot(0n)) {
            player.loadEmptyBase();
          } else if (player.baseSeq() !== 0n) {
            player.activateCachedSnapshot(0n);
          }
        } else {
          const snapSeq = BigInt(cmd.snapshotSeq);
          if (!player.hasSnapshot(snapSeq)) {
            const snapResp = await fetch(
              `/api/workbooks/${encodeURIComponent(cmd.docId)}/snapshots/${encodeURIComponent(cmd.snapshotSeq)}`,
              fetchInit,
            );
            if (!snapResp.ok) {
              throw new Error(
                `Could not load snapshot #${cmd.snapshotSeq} (HTTP ${snapResp.status})`,
              );
            }
            player.loadSnapshot(snapSeq, new Uint8Array(await snapResp.arrayBuffer()));
          } else if (player.baseSeq() !== snapSeq) {
            player.activateCachedSnapshot(snapSeq);
          }
        }
        while (player.maxLoadedSeq() < targetSeq) {
          const afterSeq = player.maxLoadedSeq();
          const txResp = await fetch(
            `/api/workbooks/${encodeURIComponent(cmd.docId)}/txns?after_seq=${afterSeq}&limit=500`,
            fetchInit,
          );
          if (!txResp.ok) {
            throw new Error(`Could not load history transactions (HTTP ${txResp.status})`);
          }
          const batch = new Uint8Array(await txResp.arrayBuffer());
          if (batch.byteLength === 0) {
            break;
          }
          const nextMax = player.appendTxns(batch);
          if (nextMax <= afterSeq) {
            break;
          }
        }
        const sheet = player.seekTo(targetSeq);
        historyPreviewActive = true;
        activeSheetId = sheet.sheet;
        scope.postMessage({
          kind: "historyReady",
          seq: String(player.currentSeq()),
          sheet,
          elapsedMs: performance.now() - started,
        });
        const sub = lastSubscription;
        player.subscribePages(
          sheet.sheet,
          sub?.epoch ?? 1,
          sub?.firstRow ?? 0,
          sub?.rowCount ?? 64,
          sub?.firstCol ?? 0,
          sub?.colCount ?? 16,
        );
        flushHistoryPages(player);
      } catch (err) {
        scope.postMessage({
          kind: "rejected",
          reason: err instanceof Error ? err.message : String(err),
        });
      }
      break;
    }
    case "historyExit": {
      historyPreviewActive = false;
      let ev = engine.handle({ kind: "switchSheet", sheet: activeSheetId });
      if (ev?.kind === "rejected") {
        ev = engine.handle({ kind: "switchSheet", sheet: 0 });
      }
      if (ev?.kind === "sheetChanged") {
        activeSheetId = ev.sheet.sheet;
        scope.postMessage({ kind: "historyExited", sheet: ev.sheet });
      }
      // Do not replay stale `lastSubscription`: `historyExited` calls `grid.setSheet(...)`,
      // which clears PageCache and issues a fresh `subscribe` at the new epoch.
      flushDuePagesAndAux(engine);
      break;
    }
    case "debugSpinWorker": {
      const end = performance.now() + cmd.ms;
      while (performance.now() < end) {
        // Busy-wait used only by automated tests exercising the 500 ms takeover watchdog.
      }
      break;
    }
  }
}

/** After a failure the engine's state cannot be trusted, so nothing more is run. */
function fail(error: unknown): void {
  if (stopped) {
    return;
  }
  stopped = true;
  clearCollabCheckpointTimer();
  abortActiveSnapshot();
  if (activeCollab) {
    activeCollab.close();
    activeCollab = null;
    collaborative = false;
  }
  if (activeDoc) {
    activeDoc.wal.close();
    releaseLock(lockNameOf(activeDoc.docId));
    activeDoc = null;
  }
  scope.postMessage({
    kind: "failed",
    message: error instanceof Error ? error.message : String(error),
    panicked: hasEnginePanicked(),
  });
}

const startupQueue: (WorkerEnvelope | EngineCommand)[] = [];

const COLLAB_ALLOWED_HOST_COMMANDS = new Set<HostCommand["kind"]>([
  "openDoc",
  "openCollab",
  "createDoc",
  "listDocs",
  "exportRtcl",
  "exportXlsxSnapshot",
  "exportCsv",
  "flushAndCloseForUpdate",
  "historySeek",
  "historyExit",
]);

function dispatchEnvelope(item: WorkerEnvelope | EngineCommand): void {
  enqueueTask(async () => {
    if ("channel" in item) {
      if (item.channel === "collab") {
        await runSyncCommand(item.command);
      } else if (item.channel === "host") {
        if (collaborative && !COLLAB_ALLOWED_HOST_COMMANDS.has(item.command.kind)) {
          scope.postMessage({
            kind: "rejected",
            reason: "This worker holds a collaborative workbook.",
          });
          return;
        }
        await handleHostCommand(item.command);
      } else {
        await runEngineCommand(item.command);
      }
    } else {
      await runEngineCommand(item);
    }
  });
}

scope.onmessage = (event) => {
  if (engine) {
    dispatchEnvelope(event.data);
  } else {
    startupQueue.push(event.data);
  }
};

async function main(): Promise<void> {
  try {
    const [, detected] = await Promise.all([
      init({ module_or_path: wasmUrl }),
      detectStorageBackend(),
    ]);
    backend = detected;
    performance.mark("reticula:wasm-ready");
    engine = new Engine();
    for (const item of startupQueue.splice(0)) {
      dispatchEnvelope(item);
    }
  } catch (error) {
    fail(error);
  }
}

void main();
