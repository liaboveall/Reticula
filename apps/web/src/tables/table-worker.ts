import {
  crash,
  csvHeadBytes,
  csvSniff,
  init,
  lastTrouble,
  panicMessage,
  Table,
  TableJob,
  TableStep,
  TableWork,
} from "@reticula/protocol/table";
import wasmUrl from "@reticula/protocol/table-wasm?url";
import { TablesHost } from "./host.ts";
import type { TableCommand, WorkerName } from "./protocol.ts";

interface WorkerScope {
  readonly name: string;
  onmessage: ((event: MessageEvent<TableCommand>) => void) | null;
  postMessage(message: unknown, transfer?: Transferable[]): void;
}

const scope = self as unknown as WorkerScope;
const { docId, tabId } = JSON.parse(scope.name) as WorkerName;

let memory: WebAssembly.Memory | null = null;

const host = new TablesHost({
  docId,
  tabId,
  engine: {
    crash,
    csvHeadBytes,
    csvSniff,
    lastTrouble,
    panicMessage,
    Table,
    TableJob,
    TableStep,
    TableWork,
  },
  memoryBytes: () => memory?.buffer.byteLength ?? 0,
  post: (message, transfer = []) => scope.postMessage(message, transfer),
  locks: navigator.locks,
  channel: typeof BroadcastChannel === "undefined" ? null : new BroadcastChannel("reticula:locks"),
});

const early: TableCommand[] = [];
scope.onmessage = ({ data }) => {
  early.push(data);
};

init({ module_or_path: wasmUrl }).then(
  (exports) => {
    memory = exports.memory;
    scope.onmessage = ({ data }) => host.handle(data);
    for (const command of early) {
      host.handle(command);
    }
  },
  (error: unknown) => {
    scope.postMessage({
      kind: "failed",
      failure: { code: "load", detail: error instanceof Error ? error.message : String(error) },
    });
  },
);
