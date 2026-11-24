// The measurement page. It shows nothing by itself: a script drives it through
// `window.__bench` and records what the functions return.
//
// It is built with `vite build --mode spikes`, with the same settings as the
// app, and measures the same grid package and engine worker the app runs.

import "@reticula/grid/grid.css";
import "./bench.css";
import * as s1 from "./s1.ts";
import * as s2 from "./s2.ts";
import * as s11 from "./s11.ts";
import { stage } from "./stage.ts";
import { frameInterval, type Summary, timerResolution } from "./stats.ts";

export interface Environment {
  readonly userAgent: string;
  readonly devicePixelRatio: number;
  readonly viewport: { readonly width: number; readonly height: number };
  /** Whether the page is cross-origin isolated, which gives it a finer clock. */
  readonly crossOriginIsolated: boolean;
  /** The smallest step `performance.now()` was seen to take, in milliseconds. */
  readonly timerResolution: number;
  /** Milliseconds between frames with nothing being drawn. */
  readonly frameInterval: Summary;
  readonly hardwareConcurrency: number;
  readonly visibility: DocumentVisibilityState;
  /** The fonts the grid asks for, in order of preference. */
  readonly font: string;
}

async function environment(): Promise<Environment> {
  const host = stage();
  host.classList.add("reticula-grid");
  const style = getComputedStyle(host);
  const font = `${style.fontSize} ${style.fontFamily}`;
  host.classList.remove("reticula-grid");
  return {
    userAgent: navigator.userAgent,
    devicePixelRatio: window.devicePixelRatio,
    viewport: { width: window.innerWidth, height: window.innerHeight },
    crossOriginIsolated: window.crossOriginIsolated,
    timerResolution: timerResolution(),
    frameInterval: await frameInterval(),
    hardwareConcurrency: navigator.hardwareConcurrency,
    visibility: document.visibilityState,
    font,
  };
}

const bench = { environment, s1, s2, s11 };

declare global {
  interface Window {
    __bench?: typeof bench;
  }
}

window.__bench = bench;
