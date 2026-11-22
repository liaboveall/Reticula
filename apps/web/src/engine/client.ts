import type {
  CollabEvent,
  EngineCommand,
  EngineEvent,
  SheetInfo,
  SyncCheckpoint,
  SyncCommand,
  WireWalTxnSummary,
} from "@reticula/protocol";
import type { StorageBackendKind, WorkbookCatalogEntry } from "./storage.ts";

export type SafeModeReason = "explicit" | "crashLoop" | "corruptWal" | "corruptSnapshot";

export interface SafeModeInfo {
  readonly docId: string;
  readonly title: string;
  readonly reason: SafeModeReason;
  readonly detail: string;
  readonly openingCount: number;
  readonly salvageableMid: boolean;
  readonly tailTxns: readonly WireWalTxnSummary[];
}

export type HostEvent =
  | {
      readonly kind: "docOpened";
      readonly docId: string;
      readonly title: string;
      readonly storageBackend: StorageBackendKind;
      readonly walRecords: number;
      readonly activeSlot: "a" | "b";
    }
  | { readonly kind: "docBlocked"; readonly docId: string; readonly title: string }
  | {
      readonly kind: "docTakingOver";
      readonly docId: string;
      readonly waitingForFile: boolean;
    }
  | { readonly kind: "docYielded"; readonly docId: string; readonly evicted: boolean }
  | { readonly kind: "safeMode"; readonly info: SafeModeInfo }
  | { readonly kind: "catalogListed"; readonly docs: readonly WorkbookCatalogEntry[] }
  | {
      readonly kind: "docSaved";
      readonly docId: string;
      readonly slot: "a" | "b";
      readonly byteLen: number;
      readonly elapsedMs: number;
    }
  | { readonly kind: "quotaExceeded"; readonly message: string }
  | { readonly kind: "csvImportProgress"; readonly rowsParsed: number; readonly percent: number }
  | {
      readonly kind: "fileExported";
      readonly format: "rtcl" | "csv" | "xlsxSnapshot";
      readonly filename: string;
      readonly bytes: ArrayBuffer;
    }
  | { readonly kind: "closedForUpdate" }
  | {
      readonly kind: "collabOpened";
      readonly docId: string;
      readonly title: string;
      readonly slot: string;
      readonly replica: number | null;
      readonly storageBackend: StorageBackendKind;
    }
  | {
      readonly kind: "collabUnsyncedSaved";
      readonly localDocId: string;
      readonly title: string;
      readonly fromClientSeq: string;
    }
  | {
      readonly kind: "historyReady";
      readonly seq: string;
      readonly sheet: SheetInfo;
      readonly elapsedMs: number;
    }
  | { readonly kind: "historyExited"; readonly sheet: SheetInfo }
  | { readonly kind: "failed"; readonly message: string; readonly panicked?: boolean };

export type EngineMessage = EngineEvent | HostEvent;

export type HostCommand =
  | {
      readonly kind: "openDoc";
      readonly docId: string;
      readonly title?: string | undefined;
      readonly safeRequested?: boolean | undefined;
    }
  | {
      readonly kind: "openCollab";
      readonly userId: string;
      readonly docId: string;
      readonly branchId: string;
      readonly title: string;
      readonly online: boolean;
      readonly readOnly?: boolean | undefined;
    }
  | { readonly kind: "takeOverDoc"; readonly docId: string }
  | {
      readonly kind: "createDoc";
      readonly docId: string;
      readonly title: string;
      readonly template: "empty" | "sample";
    }
  | { readonly kind: "renameDoc"; readonly docId: string; readonly title: string }
  | { readonly kind: "deleteDoc"; readonly docId: string }
  | { readonly kind: "listDocs" }
  | {
      readonly kind: "safeModeRecover";
      readonly docId: string;
      readonly mode:
        | { readonly type: "openFrozen" }
        | { readonly type: "skipTail"; readonly skipCount: number; readonly salvageMid: boolean };
    }
  | { readonly kind: "exportRtcl" }
  | { readonly kind: "exportXlsxSnapshot" }
  | {
      readonly kind: "importRtcl";
      readonly docId: string;
      readonly title: string;
      readonly bytes: ArrayBuffer;
    }
  | { readonly kind: "exportCsv"; readonly sheet: number }
  | {
      readonly kind: "importCsv";
      readonly docId: string;
      readonly title: string;
      readonly sheetName: string;
      readonly bytes: ArrayBuffer;
    }
  | { readonly kind: "forceCompactSnapshot" }
  | { readonly kind: "flushAndCloseForUpdate" }
  | {
      readonly kind: "historySeek";
      readonly docId: string;
      readonly targetSeq: string;
      readonly snapshotSeq: string | null;
      readonly shareToken?: string | null | undefined;
    }
  | { readonly kind: "historyExit" }
  | { readonly kind: "debugSpinWorker"; readonly ms: number };

export type WorkerEnvelope =
  | { readonly channel: "engine"; readonly command: EngineCommand }
  | { readonly channel: "host"; readonly command: HostCommand }
  | { readonly channel: "collab"; readonly command: SyncCommand };

export type WorkerOutput =
  | EngineMessage
  | ArrayBuffer
  | { readonly channel: "collab"; readonly event: CollabEvent };

export interface EngineHandlers {
  /** Unique identifier of this browser tab (used on `reticula:locks`). */
  readonly tabId?: string | undefined;
  /** Initial local workbook ID (`"sample-1m"` by default). */
  readonly initialDocId?: string | undefined;
  /** Whether `?safe=1` was requested in the URL. */
  readonly initialSafeMode?: boolean | undefined;
  /** Explicit initial host command when re-mounting a terminated worker (`takeOverDoc` / `createDoc`). */
  readonly initialCommand?: HostCommand | undefined;
  /** Opens a collaborative document instead of initializing a local workbook. */
  readonly initialSync?: Extract<SyncCommand, { kind: "open" }> | undefined;
  /** Ordered collaboration effects. Complete durable writes before each following send. */
  onCollab?(event: CollabEvent): void | Promise<void>;
  /** A structured message from the engine or storage worker. */
  onMessage(message: EngineMessage): void;
  /** A viewport page. The buffer belongs to the receiver. */
  onPage(page: ArrayBuffer): void;
}

export interface EngineClient {
  send(command: EngineCommand): void;
  sendHost(command: HostCommand, transfer?: Transferable[]): void;
  sendSync(command: SyncCommand, transfer?: Transferable[]): void;
  /** Snapshot read which can complete while a persistence effect is still being handled. */
  checkpoint(): Promise<SyncCheckpoint>;
  /** Terminates the dedicated worker immediately (releasing Web Locks and OPFS handles). */
  stop(): void;
}

/**
 * Starts the engine & OPFS storage coordinator in a dedicated worker, so that
 * calculation, WAL appends, and CoW snapshot streaming never block input or rendering.
 */
export function startEngine(handlers: EngineHandlers): EngineClient {
  if (handlers.initialSync && !handlers.onCollab) {
    throw new Error("A collaborative worker requires an effect handler.");
  }
  const tabId = handlers.tabId ?? Math.random().toString(36).slice(2, 10);
  const initialDocId = handlers.initialDocId ?? "sample-1m";
  const worker = new Worker(new URL("./worker.ts", import.meta.url), {
    type: "module",
    name: tabId,
  });
  let stopped = false;
  let syncFailed = false;
  let syncEffects = Promise.resolve();
  let checkpointRequest: PromiseWithResolvers<SyncCheckpoint> | undefined;
  worker.onmessage = (event: MessageEvent<WorkerOutput>) => {
    if (event.data instanceof ArrayBuffer) {
      handlers.onPage(event.data);
    } else if ("channel" in event.data) {
      const effect = event.data.event;
      if (
        checkpointRequest &&
        (effect.kind === "checkpoint" || effect.kind === "checkpointUnavailable")
      ) {
        const request = checkpointRequest;
        checkpointRequest = undefined;
        if (effect.kind === "checkpoint") request.resolve(effect.checkpoint);
        else request.reject(new Error(effect.message));
        return;
      }
      syncEffects = syncEffects
        .then(async () => {
          if (!stopped && !syncFailed) await handlers.onCollab?.(effect);
        })
        .catch((error: unknown) => {
          syncFailed = true;
          if (!stopped) {
            handlers.onMessage({
              kind: "failed",
              message:
                error instanceof Error ? error.message : "Could not apply a collaboration effect.",
            });
          }
        });
    } else {
      if (event.data.kind === "failed") {
        checkpointRequest?.reject(new Error(event.data.message));
        checkpointRequest = undefined;
      }
      handlers.onMessage(event.data);
    }
  };
  worker.onerror = (event) => {
    const message = event.message || "The engine worker failed.";
    checkpointRequest?.reject(new Error(message));
    checkpointRequest = undefined;
    handlers.onMessage({ kind: "failed", message });
  };

  const initial: WorkerEnvelope = handlers.initialSync
    ? { channel: "collab", command: handlers.initialSync }
    : {
        channel: "host",
        command: handlers.initialCommand ?? {
          kind: "openDoc",
          docId: initialDocId,
          safeRequested: handlers.initialSafeMode,
        },
      };
  worker.postMessage(initial);

  return {
    send: (command) => worker.postMessage({ channel: "engine", command } satisfies WorkerEnvelope),
    sendHost: (command, transfer) => {
      const env: WorkerEnvelope = { channel: "host", command };
      if (transfer && transfer.length > 0) {
        worker.postMessage(env, transfer);
      } else {
        worker.postMessage(env);
      }
    },
    sendSync: (command, transfer = []) => {
      worker.postMessage({ channel: "collab", command } satisfies WorkerEnvelope, transfer);
    },
    checkpoint: () => {
      if (stopped || !handlers.initialSync)
        return Promise.reject(new Error("No collaborative worker is open."));
      if (!checkpointRequest) {
        checkpointRequest = Promise.withResolvers<SyncCheckpoint>();
        worker.postMessage({
          channel: "collab",
          command: { kind: "checkpoint" },
        } satisfies WorkerEnvelope);
      }
      return checkpointRequest.promise;
    },
    stop: () => {
      stopped = true;
      checkpointRequest?.reject(new Error("The engine worker stopped."));
      checkpointRequest = undefined;
      worker.terminate();
    },
  };
}
