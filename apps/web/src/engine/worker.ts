import { boot, init } from "@reticula/protocol";
import wasmUrl from "@reticula/protocol/wasm?url";
import type { EngineMessage } from "./client.ts";

/**
 * The part of the dedicated worker scope this file uses. The DOM typings that
 * the rest of the app is checked against describe a window, not a worker.
 */
interface WorkerScope {
  postMessage(message: EngineMessage): void;
}

const scope = self as unknown as WorkerScope;

async function main(): Promise<void> {
  try {
    // Passing the URL lets the browser compile the module while it downloads.
    await init({ module_or_path: wasmUrl });
    scope.postMessage(boot());
  } catch (error) {
    scope.postMessage({
      kind: "failed",
      message: error instanceof Error ? error.message : String(error),
    });
  }
}

void main();
