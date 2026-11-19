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
let recalcTimer = 0;
/** Commands that arrived while the engine was still starting. */
const waiting: EngineCommand[] = [];

function scheduleContinue(): void {
  if (stopped || recalcTimer !== 0 || !engine?.hasPendingRecalc()) {
    return;
  }
  // Yield to the worker macrotask queue between ~4 ms slices so incoming
  // `subscribe` (scroll) or newer `editCell` commands execute immediately.
  recalcTimer = setTimeout(() => {
    recalcTimer = 0;
    if (engine?.hasPendingRecalc()) {
      run(engine, { kind: "continueRecalc" });
    }
  }, 0) as unknown as number;
}

function run(engine: Engine, command: EngineCommand): void {
  if (stopped) {
    return;
  }
  try {
    const event = engine.handle(command);
    if (event) {
      scope.postMessage(event);
    }
    // Collect due pages first so any cross-axis styles lazily interned during
    // `takePage` are posted via `takeAuxEvent` BEFORE the page buffers arrive.
    const pages: ArrayBuffer[] = [];
    for (let page = engine.takePage(); page; page = engine.takePage()) {
      pages.push(page.buffer as ArrayBuffer);
    }
    for (let aux = engine.takeAuxEvent(); aux; aux = engine.takeAuxEvent()) {
      scope.postMessage(aux);
    }
    for (const buffer of pages) {
      scope.postMessage(buffer, [buffer]);
    }
    scheduleContinue();
  } catch (error) {
    fail(error);
  }
}

/** After a failure the engine's state cannot be trusted, so nothing more is run. */
function fail(error: unknown): void {
  stopped = true;
  if (recalcTimer !== 0) {
    clearTimeout(recalcTimer);
    recalcTimer = 0;
  }
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
