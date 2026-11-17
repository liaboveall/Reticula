import { Engine, type EngineCommand, init } from "@reticula/protocol";
import wasmUrl from "@reticula/protocol/wasm?url";
import type { EngineMessage } from "./client.ts";

/**
 * The part of the dedicated worker scope this file uses. The DOM typings that
 * the rest of the app is checked against describe a window, not a worker.
 */
interface WorkerScope {
  onmessage: ((event: MessageEvent<EngineCommand>) => void) | null;
  postMessage(message: EngineMessage | ArrayBuffer, transfer?: Transferable[]): void;
}

const scope = self as unknown as WorkerScope;

/** The document every visitor starts with: generated here, never downloaded. */
const SAMPLE: EngineCommand = { kind: "openSample", seed: 20261117, rows: 1_000_000 };

let engine: Engine | null = null;
let stopped = false;
/** Commands that arrived while the engine was still starting. */
const waiting: EngineCommand[] = [];

function run(engine: Engine, command: EngineCommand): void {
  if (stopped) {
    return;
  }
  try {
    const event = engine.handle(command);
    if (event) {
      scope.postMessage(event);
    }
    // Each page is handed over rather than copied: the buffer moves to the
    // main thread and is gone from this one.
    for (let page = engine.takePage(); page; page = engine.takePage()) {
      const buffer = page.buffer as ArrayBuffer;
      scope.postMessage(buffer, [buffer]);
    }
  } catch (error) {
    fail(error);
  }
}

/** After a failure the engine's state cannot be trusted, so nothing more is run. */
function fail(error: unknown): void {
  stopped = true;
  scope.postMessage({
    kind: "failed",
    message: error instanceof Error ? error.message : String(error),
  });
}

scope.onmessage = (event) => {
  if (engine) {
    run(engine, event.data);
  } else {
    waiting.push(event.data);
  }
};

async function main(): Promise<void> {
  try {
    // Passing the URL lets the browser compile the module while it downloads.
    await init({ module_or_path: wasmUrl });
    performance.mark("reticula:wasm-ready");
    engine = new Engine();
    run(engine, SAMPLE);
    performance.mark("reticula:sample-ready");
    for (const command of waiting.splice(0)) {
      run(engine, command);
    }
  } catch (error) {
    fail(error);
  }
}

void main();
