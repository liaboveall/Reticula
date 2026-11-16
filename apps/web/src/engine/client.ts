import type { EngineEvent } from "@reticula/protocol";

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

/**
 * Starts the engine in a dedicated worker, so that calculation never blocks
 * input or rendering. Returns a function that stops it.
 */
export function startEngine(onMessage: (message: EngineMessage) => void): () => void {
  const worker = new Worker(new URL("./worker.ts", import.meta.url), {
    type: "module",
    name: "reticula-engine",
  });
  worker.onmessage = (event: MessageEvent<EngineMessage>) => onMessage(event.data);
  worker.onerror = (event) => {
    onMessage({ kind: "failed", message: event.message || "The engine worker failed." });
  };
  return () => worker.terminate();
}
