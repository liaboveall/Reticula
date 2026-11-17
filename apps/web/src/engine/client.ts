import type { EngineCommand, EngineEvent } from "@reticula/protocol";

/**
 * The engine could not start or has stopped. This is the one message that is
 * not generated from the Rust sources: it reports that the engine itself is
 * unavailable, so it has to come from the glue around it.
 */
export interface EngineFailure {
  kind: "failed";
  message: string;
}

export type EngineMessage = EngineEvent | EngineFailure;

export interface EngineHandlers {
  /** A structured message from the engine. */
  onMessage(message: EngineMessage): void;
  /** A viewport page. The buffer belongs to the receiver. */
  onPage(page: ArrayBuffer): void;
}

export interface EngineClient {
  send(command: EngineCommand): void;
  stop(): void;
}

/**
 * Starts the engine in a dedicated worker, so that calculation never blocks
 * input or rendering.
 */
export function startEngine(handlers: EngineHandlers): EngineClient {
  const worker = new Worker(new URL("./worker.ts", import.meta.url), {
    type: "module",
    name: "reticula-engine",
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
  return {
    send: (command) => worker.postMessage(command),
    stop: () => worker.terminate(),
  };
}
