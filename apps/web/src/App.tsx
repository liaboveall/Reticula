import { Grid, type GridStats } from "@reticula/grid";
import type { EngineInfo, SheetInfo } from "@reticula/protocol";
import { useEffect, useRef, useState } from "react";
import { startEngine } from "./engine/client.ts";
import { hooksRequested, installHooks } from "./hooks.ts";

type EngineState =
  | { status: "starting" }
  | { status: "ready"; info: EngineInfo; sheet: SheetInfo }
  | { status: "failed"; message: string };

/** How often the status bar looks at what the grid is doing. */
const STATUS_INTERVAL_MS = 500;

/**
 * Moments of starting up, marked on the page's timeline where the browser's
 * own tools and the start-up measurements read them. The engine's worker
 * marks its own two, `reticula:wasm-ready` and `reticula:sample-ready`, on
 * its timeline.
 */
const MARKS = {
  /** The grid has drawn its first frame and takes input. */
  shell: "reticula:shell",
  /** The engine has said that the document is open. */
  engineReady: "reticula:engine-ready",
  /** A frame has been drawn in which every cell in view had its text. */
  firstScreen: "reticula:first-screen",
} as const;

const integer = new Intl.NumberFormat("en-US");
const milliseconds = new Intl.NumberFormat("en-US", {
  minimumFractionDigits: 1,
  maximumFractionDigits: 1,
});

export function App() {
  const host = useRef<HTMLDivElement>(null);
  const [engine, setEngine] = useState<EngineState>({ status: "starting" });
  const [stats, setStats] = useState<GridStats | null>(null);

  useEffect(() => {
    const element = host.current;
    if (!element) {
      return;
    }
    // Those waiting for the result of a determinism self-check.
    const selfChecks: ((digest: string) => void)[] = [];

    const client = startEngine({
      onMessage(message) {
        switch (message.kind) {
          case "ready":
            performance.mark(MARKS.engineReady);
            grid.setSheet(message.sheet);
            setEngine({ status: "ready", info: message.info, sheet: message.sheet });
            break;
          case "selfChecked":
            for (const resolve of selfChecks.splice(0)) {
              resolve(message.digest);
            }
            break;
          case "failed":
            setEngine({ status: "failed", message: message.message });
            break;
        }
      },
      onPage: (page) => grid.acceptPage(page),
    });
    // The grid is on screen and takes input before the engine has anything
    // to show; the sheet's size arrives with the engine's first message.
    const grid = new Grid(element, {
      subscribe: (subscription) => client.send({ kind: "subscribe", sheet: 0, ...subscription }),
    });

    let shellMarked = false;
    const stopMarking = grid.watchFrames((frame) => {
      if (!shellMarked) {
        shellMarked = true;
        performance.mark(MARKS.shell);
      }
      if (frame.cells > 0 && frame.missing === 0) {
        performance.mark(MARKS.firstScreen);
        stopMarking();
      }
    });

    const removeHooks = hooksRequested()
      ? installHooks(grid, () => {
          client.send({ kind: "selfCheck" });
          return new Promise<string>((resolve) => selfChecks.push(resolve));
        })
      : undefined;
    const timer = window.setInterval(() => setStats(grid.stats()), STATUS_INTERVAL_MS);

    return () => {
      window.clearInterval(timer);
      stopMarking();
      removeHooks?.();
      grid.destroy();
      client.stop();
    };
  }, []);

  return (
    <div className="app">
      <header className="titlebar">
        <h1>Reticula</h1>
        {engine.status === "ready" && (
          <span className="document" data-testid="document">
            Sample sales data · {integer.format(engine.sheet.rows)} rows × {engine.sheet.cols}{" "}
            columns
          </span>
        )}
      </header>

      <div className="sheet">
        <div ref={host} className="grid" data-testid="grid" />
        {engine.status === "failed" && (
          <div className="failure" role="alert">
            <p>The engine has stopped. Reload the page to start again.</p>
            <p className="detail" data-testid="engine-error">
              {engine.message}
            </p>
          </div>
        )}
      </div>

      <footer className="statusbar">
        <span data-testid="engine-status">{engine.status}</span>
        {engine.status === "starting" && <span>Generating the sample…</span>}
        {engine.status === "ready" && (
          <>
            {stats?.selection && (
              <span data-testid="selection">
                {stats.selection.name}
                {stats.selection.text ? ` · ${stats.selection.text}` : ""}
              </span>
            )}
            <span className="spacer" />
            <span title="The sample is computed from this seed, not downloaded">
              seed {engine.sheet.seed}
            </span>
            <span data-testid="generation" title="Time the engine took to generate the sample">
              generated in {milliseconds.format(engine.sheet.generationMs)} ms
            </span>
            {stats && stats.frameWork.samples > 0 && (
              <span
                data-testid="frame-work"
                title="Main-thread work per frame: input, receiving pages and issuing draw calls"
              >
                frame {milliseconds.format(stats.frameWork.p50)} ms · p99{" "}
                {milliseconds.format(stats.frameWork.p99)} ms
              </span>
            )}
            <span data-testid="kernel-version">kernel {engine.info.kernelVersion}</span>
          </>
        )}
      </footer>
    </div>
  );
}
