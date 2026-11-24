import type { Frame } from "./frame.ts";
import type { TextMeasurer } from "./text-measure.ts";
import type { GridRenderer } from "./types.ts";

/** `webgpu` is for tests: it also takes a software adapter, and browsers nobody has tried. */
export type RendererPreference = "auto" | "canvas2d" | "webgpu";

/** Why a grid is not drawn with WebGPU. */
export type RendererReason =
  | "starting"
  /** It is meant to be drawn with Canvas 2D. */
  | "preference"
  | "unsupported"
  /** A browser engine whose pictures have not been verified. */
  | "engine"
  | "forced-colors"
  | "no-adapter"
  | "software-adapter"
  | "device-lost"
  | "failed";

/** What a renderer that is loaded later needs of the grid. */
export interface RendererHost {
  readonly measure: TextMeasurer;
  /** What was drawn last, if anything. */
  readonly frame: () => Frame | null;
  readonly invalidate: () => void;
  /** Has a renderer draw from now on, or one for a 2D canvas. Refuses while something is dragged. */
  readonly swap: (renderer: GridRenderer | null) => boolean;
  readonly report: (reason: RendererReason | null, adapter?: string | null) => void;
}

/**
 * Replaces the grid's renderer with one for WebGPU, once the browser has
 * nothing better to do. Returns a function that calls it off.
 */
export function startUpgrade(host: RendererHost, preference: RendererPreference): () => void {
  let stop: (() => void) | null = () => {};
  const forced = preference === "webgpu";
  const reason =
    preference === "canvas2d"
      ? "preference"
      : !navigator.gpu
        ? "unsupported"
        : // Chromium and Firefox, by what only they have.
          forced || "userAgentData" in navigator || "mozInnerScreenX" in window
          ? "starting"
          : "engine";
  host.report(reason);
  const load = async () => {
    try {
      const { attach } = await import("./webgpu/index.ts");
      stop &&= attach(host, forced);
    } catch {
      // Whoever has called it off has reported something since.
      if (stop) {
        host.report("failed");
      }
    }
  };
  if (reason === "starting") {
    // Safari has no requestIdleCallback.
    (window.requestIdleCallback ?? window.setTimeout)(() => stop && load());
  }
  return () => {
    stop?.();
    stop = null;
  };
}
