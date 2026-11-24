// How far the sheet is enlarged, and what the wheel and gestures make of it.
// Nothing here needs a browser.

export const MIN_ZOOM = 0.25;
export const MAX_ZOOM = 2;

/** The zooms that are offered, and that a turn of the wheel goes through. */
export const ZOOM_STEPS: readonly number[] = [
  0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2,
];

/** A gesture stops at multiples of this, which spares measuring text at every size in between. */
const GESTURE_STEP = 0.05;
/** How much a pinch on a trackpad enlarges per pixel that the browser says it scrolls. */
const PINCH_RATE = 0.01;
/** More than this in one event is a wheel whose key nobody has seen go down. */
const MOST_PINCHED = 25;
/** Pixels a wheel has to turn for a step. */
const NOTCH = 40;

/** A zoom within the limits; `fallback` for what is none. */
export function clampZoom(zoom: number, fallback = 1): number {
  return Number.isFinite(zoom) ? Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, zoom)) : fallback;
}

/** The next of the zooms that are offered. */
export function nextZoom(zoom: number, direction: 1 | -1): number {
  // A zoom that is nearly a step counts as that step.
  return direction > 0
    ? (ZOOM_STEPS.find((step) => step > zoom + 0.001) ?? MAX_ZOOM)
    : (ZOOM_STEPS.findLast((step) => step < zoom - 0.001) ?? MIN_ZOOM);
}

/**
 * One turn of the wheel or one pinch, from its first event to its last.
 * What it comes to is kept unrounded: rounded, many small movements would
 * come to nothing, and a movement there and back would not end where it began.
 */
export class ZoomGesture {
  #value: number | null = null;
  /** The zoom the fingers began at, or `null` if that has been forgotten since. */
  #start: number | null = null;
  #turned = 0;

  /** The zoom after the wheel was turned by so many pixels, or `null` if there is no other yet. */
  turn(delta: number, zoom: number): number | null {
    this.#value = null;
    this.#turned = delta * this.#turned < 0 ? delta : this.#turned + delta;
    if (Math.abs(this.#turned) < NOTCH) {
      return null;
    }
    const next = nextZoom(zoom, this.#turned < 0 ? 1 : -1);
    this.#turned = 0;
    return next === zoom ? null : next;
  }

  /** The zoom after a pinch that the browser reports as scrolling by so many pixels. */
  pinch(delta: number, zoom: number): number | null {
    const moved = Math.min(MOST_PINCHED, Math.max(-MOST_PINCHED, delta));
    return this.#reach((this.#value ?? zoom) * Math.exp(-moved * PINCH_RATE), zoom);
  }

  /** Begins a pinch that the browser reports by how far the fingers have moved apart. */
  begin(zoom: number): void {
    this.end();
    this.#start = zoom;
  }

  spread(scale: number, zoom: number): number | null {
    if (!(scale > 0)) {
      return null;
    }
    // Zoomed by something else meanwhile, it goes on from there.
    this.#start ??= zoom / scale;
    return this.#reach(this.#start * scale, zoom);
  }

  end(): void {
    this.#value = null;
    this.#start = null;
    this.#turned = 0;
  }

  #reach(value: number, zoom: number): number | null {
    this.#turned = 0;
    this.#value = clampZoom(value, zoom);
    // From a zoom between two steps, the nearest one may be the wrong way.
    if (Math.abs(this.#value - zoom) < GESTURE_STEP / 2) {
      return null;
    }
    const rounded = Math.round(Math.round(this.#value / GESTURE_STEP) * GESTURE_STEP * 100) / 100;
    return rounded === zoom ? null : clampZoom(rounded);
  }
}
