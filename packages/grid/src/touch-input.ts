import type { Grid } from "./grid.ts";
import type { Metrics, Point, Scroll } from "./layout.ts";
import { Fling, TouchGesture, type TouchIntent } from "./touch.ts";

// What fingers do to a grid. Loaded once there are any: most devices the grid
// is used on have none. What it shares with the grid it is handed: importing
// it would have the bundler take the grid apart, and compress the parts worse.

/** Sent to the grid's element before a tap selects anything. Whoever prevents it has the tap do nothing. */
export type TapEvent = "reticula-tap";
const TAP_EVENT: TapEvent = "reticula-tap";

/** A grid, as far as fingers have anything to do with it. */
export interface TouchTarget {
  readonly grid: Pick<
    Grid,
    "zoom" | "setZoom" | "focus" | "select" | "selectRows" | "selectCols" | "selectAll"
  >;
  readonly host: HTMLElement;
  readonly input: HTMLElement;
  /** Whether fingers may zoom it. */
  readonly zooms: boolean;
  readonly metrics: () => Metrics;
  readonly scroll: () => Scroll;
  readonly scrollBy: (dx: number, dy: number) => void;
  readonly cellAt: (x: number, y: number) => { readonly row: number; readonly col: number } | null;
  readonly beginPinch: () => void;
  /** Zooms with the next frame to so many times the zoom of the beginning. */
  readonly pinch: (scale: number, point: Point) => void;
  readonly endPinch: () => void;
  /** Opens what a filter button at a point is for. Returns whether there is one. */
  readonly pressFilterButton: (point: Point) => boolean;
}

/** Where a double tap goes at 100% while there is no other zoom to go back to. */
const CLOSER = 1.5;

export class TouchInput {
  #target: TouchTarget;
  #gesture = new TouchGesture();
  #fling: Fling | null = null;
  #frame = 0;
  #hasCaught = false;
  /** The last zoom there was other than 100%. */
  #zoomBefore = CLOSER;

  constructor(target: TouchTarget) {
    this.#target = target;
    window.addEventListener("pointerdown", this.#onAnyPointerDown, { capture: true });
    target.host.addEventListener("wheel", this.stop, { capture: true, passive: true });
    target.host.addEventListener("keydown", this.stop, { capture: true });
  }

  destroy(): void {
    this.stop();
    window.removeEventListener("pointerdown", this.#onAnyPointerDown, { capture: true });
    this.#target.host.removeEventListener("wheel", this.stop, { capture: true });
    this.#target.host.removeEventListener("keydown", this.stop, { capture: true });
  }

  /** Stops a sheet that was let go. */
  stop = (): void => {
    cancelAnimationFrame(this.#frame);
    this.#fling = null;
  };

  /** Takes an event of a finger on the grid. */
  handle(event: PointerEvent): void {
    const { input } = this.#target;
    const { pointerId: id, timeStamp: time } = event;
    const bounds = input.getBoundingClientRect();
    const finger = { id, x: event.clientX - bounds.left, y: event.clientY - bounds.top, time };
    switch (event.type) {
      case "pointerdown":
        this.#noteZoom();
        if (event.isPrimary) {
          // It is the only finger there is.
          this.#act(this.#gesture.reset());
        }
        try {
          input.setPointerCapture(id);
        } catch {
          // The pointer is gone by now, or was never there.
        }
        this.#act(this.#gesture.down(finger, this.#hasCaught));
        break;
      case "pointermove":
        this.#act(this.#gesture.move(finger));
        break;
      case "pointerup":
        this.#act(this.#gesture.up(finger));
        break;
      default:
        // Cancelled, or the capture is lost. The latter follows every release
        // too, by when the finger has been forgotten.
        this.#act(this.#gesture.cancel(id));
    }
  }

  #noteZoom(): void {
    const { zoom } = this.#target.grid;
    if (zoom !== 1) {
      this.#zoomBefore = zoom;
    }
  }

  #onAnyPointerDown = (event: PointerEvent): void => {
    // It may be on its way to what sets the zoom to 100%.
    this.#noteZoom();
    this.#hasCaught = this.#fling !== null;
    this.stop();
    if (event.pointerType !== "touch") {
      this.#act(this.#gesture.abort());
    }
  };

  #act(intents: readonly TouchIntent[]): void {
    const { zooms, beginPinch, pinch, endPinch } = this.#target;
    for (const intent of intents) {
      switch (intent.kind) {
        case "pan":
          this.#target.scrollBy(intent.dx, intent.dy);
          break;
        case "fling":
          this.#letGo(intent.vx, intent.vy);
          break;
        case "tap":
          this.#tap(intent);
          break;
        default:
          if (!zooms) {
            break;
          }
          if (intent.kind === "pinch") {
            pinch(intent.scale, { x: intent.x, y: intent.y });
          } else if (intent.kind === "doubleTap") {
            this.#toggleZoom({ x: intent.x, y: intent.y });
          } else if (intent.kind === "pinchStart") {
            beginPinch();
          } else {
            endPinch();
          }
      }
    }
  }

  #letGo(vx: number, vy: number): void {
    if (!matchMedia("(prefers-reduced-motion: reduce)").matches) {
      this.#fling = new Fling(vx, vy, performance.now());
      this.#frame = requestAnimationFrame(this.#step);
    }
  }

  #step = (time: number): void => {
    const fling = this.#fling;
    if (!fling) {
      return;
    }
    const { dx, dy } = fling.step(time);
    const { scroll, scrollBy } = this.#target;
    const before = scroll();
    scrollBy(dx, dy);
    const after = scroll();
    if (dx !== 0 && after.left === before.left) {
      fling.stopX();
    }
    if (dy !== 0 && after.top === before.top) {
      fling.stopY();
    }
    if (fling.isOver) {
      this.#fling = null;
    } else {
      this.#frame = requestAnimationFrame(this.#step);
    }
  };

  #tap({ x, y }: Point): void {
    const { grid, host, metrics, cellAt, pressFilterButton } = this.#target;
    const { headerWidth, headerHeight } = metrics();
    const isRow = x < headerWidth;
    const isCol = y < headerHeight;
    const cell = cellAt(Math.max(x, headerWidth), Math.max(y, headerHeight));
    if (!cell || !host.dispatchEvent(new Event(TAP_EVENT, { cancelable: true }))) {
      return;
    }
    if (pressFilterButton({ x, y })) {
      return;
    }
    grid.focus();
    if (isRow && isCol) {
      grid.selectAll();
    } else if (isRow) {
      grid.selectRows(cell.row, cell.row);
    } else if (isCol) {
      grid.selectCols(cell.col, cell.col);
    } else {
      grid.select(cell);
    }
  }

  #toggleZoom(point: Point): void {
    const { grid } = this.#target;
    grid.setZoom(grid.zoom === 1 ? this.#zoomBefore : 1, point);
  }
}
