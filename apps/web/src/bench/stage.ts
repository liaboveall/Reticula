import type { Metrics } from "@reticula/grid";
import type { SheetInfo } from "@reticula/protocol";
import { type EngineClient, startEngine } from "../engine/client.ts";

/**
 * The sizes every measurement uses. In a window of 1676 by 1036 they leave
 * room for 20 columns and 50 rows: a thousand cells, the number the targets
 * are stated for.
 */
export const METRICS: Metrics = {
  rowHeight: 20,
  colWidth: 80,
  headerHeight: 24,
  headerWidth: 64,
  scrollbarSize: 12,
};

/** The element that fills the window, into which whatever is measured is put. */
export function stage(): HTMLElement {
  const element = document.getElementById("stage");
  if (!element) {
    throw new Error("bench.html has no #stage element");
  }
  return element;
}

/** A running engine with the sample open. */
export interface OpenEngine {
  readonly client: EngineClient;
  readonly sheet: SheetInfo;
  /** Where the engine's pages go; whoever is measuring sets it. */
  onPage: (page: ArrayBuffer) => void;
}

let opened: Promise<OpenEngine> | undefined;

/**
 * The engine, started once and shared by all measurements: the very worker
 * and module the app runs.
 */
export function openEngine(): Promise<OpenEngine> {
  opened ??= new Promise((resolve, reject) => {
    let engine: OpenEngine | undefined;
    const client = startEngine({
      onMessage(message) {
        if (message.kind === "ready") {
          engine = { client, sheet: message.sheet, onPage: () => {} };
          resolve(engine);
        } else if (message.kind === "failed") {
          reject(new Error(message.message));
        }
      },
      onPage(page) {
        engine?.onPage(page);
      },
    });
  });
  return opened;
}

/** Tells the engine that nothing holds any of its pages any more. */
export function release(engine: OpenEngine): void {
  engine.onPage = () => {};
  engine.client.send({
    kind: "subscribe",
    epoch: 0,
    firstRow: 0,
    rowCount: 0,
    firstCol: 0,
    colCount: 0,
  });
}
