// Everything that draws the grid with WebGPU, loaded once it is wanted.

import type { RendererHost } from "../upgrade.ts";
import { acquire, type Lease, Refusal } from "./device.ts";
import { WebGpuRenderer } from "./renderer.ts";

/** Milliseconds until a lost device is asked for again. */
const RETRY_DELAY = 1000;
/** Milliseconds until a grid that something is dragged on is asked again. */
const DRAG_DELAY = 100;
/** How often the atlas is filled in idle time before the grid is drawn from it anyway. */
const MOST_WARMUPS = 120;

const sleep = (delay: number) => new Promise((resolve) => setTimeout(resolve, delay));

const idle = () =>
  new Promise<unknown>((resolve) => {
    (window.requestIdleCallback ?? window.setTimeout)(resolve, { timeout: DRAG_DELAY });
  });

/** Draws a grid with WebGPU as long as that is possible. Returns a function that ends that. */
export function attach(host: RendererHost, software: boolean): () => void {
  const forcedColors = matchMedia("(forced-colors: active)");
  let stopped = false;
  /** Whatever was begun under an earlier number is called off. */
  let attempt = 0;
  let timer = 0;
  /** The renderer, until it has been handed to the grid. */
  let pending: WebGpuRenderer | null = null;
  let shown = false;

  const start = async () => {
    const current = ++attempt;
    const called = () => stopped || current !== attempt;
    let lease: Lease | null = null;
    try {
      lease = await acquire(software, () => {
        if (!called()) {
          end("device-lost");
          timer = window.setTimeout(start, RETRY_DELAY);
        }
      });
      if (called()) {
        lease.release();
        return;
      }
      const renderer = new WebGpuRenderer(lease, {
        measure: host.measure,
        onIncomplete: host.invalidate,
      });
      pending = renderer;
      for (let round = 0; round < MOST_WARMUPS; round++) {
        await idle();
        if (called()) {
          return;
        }
        const frame = host.frame();
        if (frame && renderer.warm(frame)) {
          break;
        }
      }
      while (!called() && !host.swap(renderer)) {
        await sleep(DRAG_DELAY);
      }
      if (!called()) {
        pending = null;
        shown = true;
        host.report(null, lease.gpu.adapter);
      }
    } catch (error) {
      if (!called()) {
        pending?.destroy();
        pending = null;
        lease?.release();
        host.report(error instanceof Refusal ? error.reason : "failed");
      }
    }
  };

  const end = (reason: "device-lost" | "forced-colors") => {
    attempt++;
    clearTimeout(timer);
    pending?.destroy();
    pending = null;
    if (shown) {
      shown = false;
      host.swap(null);
    }
    host.report(reason);
  };

  const followColors = () => {
    if (forcedColors.matches) {
      end("forced-colors");
    } else {
      void start();
    }
  };

  forcedColors.addEventListener("change", followColors);
  followColors();

  return () => {
    stopped = true;
    forcedColors.removeEventListener("change", followColors);
    clearTimeout(timer);
    pending?.destroy();
    pending = null;
  };
}
