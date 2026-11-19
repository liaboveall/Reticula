import type { EngineCommand, EngineEvent, WireWalTxnSummary } from "@reticula/protocol";
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
      readonly format: "rtcl" | "csv";
      readonly filename: string;
      readonly bytes: ArrayBuffer;
    }
  | { readonly kind: "closedForUpdate" }
  | { readonly kind: "failed"; readonly message: string; readonly panicked?: boolean };

export type EngineMessage = EngineEvent | HostEvent;

export type HostCommand =
  | {
      readonly kind: "openDoc";
      readonly docId: string;
      readonly title?: string | undefined;
      readonly safeRequested?: boolean | undefined;
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
  | { readonly kind: "debugSpinWorker"; readonly ms: number };

export type WorkerEnvelope =
  | { readonly channel: "engine"; readonly command: EngineCommand }
  | { readonly channel: "host"; readonly command: HostCommand };

export interface EngineHandlers {
  /** Unique identifier of this browser tab (used on `reticula:locks`). */
  readonly tabId?: string | undefined;
  /** Initial local workbook ID (`"sample-1m"` by default). */
  readonly initialDocId?: string | undefined;
  /** Whether `?safe=1` was requested in the URL. */
  readonly initialSafeMode?: boolean | undefined;
  /** Explicit initial host command when re-mounting a terminated worker (`takeOverDoc` / `createDoc`). */
  readonly initialCommand?: HostCommand | undefined;
  /** A structured message from the engine or storage worker. */
  onMessage(message: EngineMessage): void;
  /** A viewport page. The buffer belongs to the receiver. */
  onPage(page: ArrayBuffer): void;
}

export interface EngineClient {
  send(command: EngineCommand): void;
  sendHost(command: HostCommand, transfer?: Transferable[]): void;
  /** Terminates the dedicated worker immediately (releasing Web Locks and OPFS handles). */
  stop(): void;
}

/**
 * Starts the engine & OPFS storage coordinator in a dedicated worker, so that
 * calculation, WAL appends, and CoW snapshot streaming never block input or rendering.
 */
export function startEngine(handlers: EngineHandlers): EngineClient {
  const tabId = handlers.tabId ?? Math.random().toString(36).slice(2, 10);
  const initialDocId = handlers.initialDocId ?? "sample-1m";
  const worker = new Worker(new URL("./worker.ts", import.meta.url), {
    type: "module",
    name: tabId,
  });
  worker.onmessage = (event: MessageEvent<EngineMessage | ArrayBuffer>) => {
    if (event.data instanceof ArrayBuffer) {
      handlers.onPage(event.data);
    } else {
      handlers.onMessage(event.data);
    }
  };
  worker.onerror = (event) => {
    handlers.onMessage({ kind: "failed", message: event.message || "The engine worker failed." });
  };

  worker.postMessage({
    channel: "host",
    command: handlers.initialCommand ?? {
      kind: "openDoc",
      docId: initialDocId,
      safeRequested: handlers.initialSafeMode,
    },
  } satisfies WorkerEnvelope);

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
    stop: () => worker.terminate(),
  };
}
