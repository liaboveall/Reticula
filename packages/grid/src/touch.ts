// What fingers mean: pans, flings, pinches and taps. Pointer events go in and
// intents come out. Nothing here needs a browser.

/** A finger, or where it was: in CSS pixels, and in milliseconds. */
export interface Finger {
  readonly id: number;
  readonly x: number;
  readonly y: number;
  readonly time: number;
}

export type TouchIntent =
  /** Scrolls by so many pixels. */
  | { readonly kind: "pan"; readonly dx: number; readonly dy: number }
  /** Lets the sheet go at so many pixels per millisecond. */
  | { readonly kind: "fling"; readonly vx: number; readonly vy: number }
  | { readonly kind: "pinchStart" }
  /** Enlarges to so many times the zoom of the start, with what is at a point staying there. */
  | { readonly kind: "pinch"; readonly scale: number; readonly x: number; readonly y: number }
  | { readonly kind: "pinchEnd" }
  | { readonly kind: "tap"; readonly x: number; readonly y: number }
  | { readonly kind: "doubleTap"; readonly x: number; readonly y: number };

/** How far a finger may stray and still press, in pixels. */
export const SLOP = 8;
/** How far two fingers may move apart or together before that is a pinch. */
export const SPAN_SLOP = 12;
/** A press that takes longer is no tap. */
export const LONG_PRESS_MS = 500;
/** How soon after a tap, and how near, a finger has to come down for the two to be one. */
export const DOUBLE_TAP_MS = 300;
export const DOUBLE_TAP_REACH = 40;
/** A pan that starts this much steeper than flat, or flatter than steep, keeps to its axis. */
const LOCK_RATIO = 2;
/** It does until the finger moves this far across before it moves as far along. */
export const LOCK_CHANNEL = 24;
/** The last stretch of a pan, which its speed is taken from. */
const SPEED_MS = 100;
/** A finger that has rested this long lets go of a sheet that is not moving. */
const REST_MS = 50;
/** Speeds in pixels per millisecond: the least that lets a sheet go on, and the most. */
const LEAST_FLING = 0.15;
const MOST_FLING = 8;

type Phase = "idle" | "press" | "pan" | "pinch" | "over";

interface Pinch {
  readonly other: number;
  /** The span that counts as a scale of 1, or `null` while the fingers only move together. */
  base: number | null;
  readonly span: number;
  x: number;
  y: number;
}

/** Tells what fingers do, from the first that comes down to the last that is lifted. */
export class TouchGesture {
  #fingers = new Map<number, Finger>();
  #phase: Phase = "idle";
  /** The finger that presses or pans, or the first of two that pinch. */
  #main = 0;
  /** Where it came down. */
  #origin: Finger = { id: 0, x: 0, y: 0, time: 0 };
  /** Where it was when the other finger of a pinch was lifted. */
  #leftBehind: Finger | null = null;
  #lock: "x" | "y" | null = null;
  #along = 0;
  #across = 0;
  #samples: Finger[] = [];
  #pinch: Pinch | null = null;
  #lastTap: Finger | null = null;
  #isSecond = false;
  /** Whether the finger has stopped a fling, which is all it has done if it is lifted again. */
  #hasCaught = false;

  down(finger: Finger, hasCaught = false): TouchIntent[] {
    // The release of a finger with this number was never heard of.
    const intents = this.#fingers.has(finger.id) ? this.reset() : [];
    this.#fingers.set(finger.id, finger);
    if (this.#phase === "idle") {
      const last = this.#lastTap;
      this.#phase = "press";
      this.#main = finger.id;
      this.#origin = finger;
      this.#samples = [finger];
      this.#leftBehind = null;
      this.#hasCaught = hasCaught;
      this.#isSecond =
        last !== null &&
        finger.time - last.time <= DOUBLE_TAP_MS &&
        distance(finger, last) <= DOUBLE_TAP_REACH;
      this.#lastTap = null;
    } else if (this.#phase === "press" || this.#phase === "pan") {
      this.#startPinch(finger);
    }
    return intents;
  }

  move(finger: Finger): TouchIntent[] {
    const before = this.#fingers.get(finger.id);
    if (!before || distance(before, finger) === 0) {
      return [];
    }
    this.#fingers.set(finger.id, finger);
    if (this.#phase === "pinch") {
      return this.#movePinch();
    }
    if (finger.id !== this.#main) {
      return [];
    }
    if (this.#phase === "press") {
      return this.#startPan(finger);
    }
    return this.#phase === "pan" ? this.#movePan(before, finger) : [];
  }

  up(finger: Finger): TouchIntent[] {
    if (!this.#fingers.has(finger.id)) {
      return [];
    }
    const intents = this.move(finger);
    this.#fingers.delete(finger.id);
    if (this.#phase === "pinch") {
      intents.push(...this.#liftFromPinch(finger));
    } else if (finger.id === this.#main && this.#fingers.size === 0) {
      intents.push(...this.#release(finger));
    } else if (finger.id === this.#main) {
      this.#phase = "over";
    }
    if (this.#fingers.size === 0) {
      this.#phase = "idle";
    }
    return intents;
  }

  /** A finger is gone without having been lifted: the gesture ends where it is. */
  cancel(id: number): TouchIntent[] {
    const isKnown = this.#fingers.delete(id);
    return isKnown ? this.abort() : [];
  }

  /** Ends the gesture where it is. What fingers are still down count for nothing until all are lifted. */
  abort(): TouchIntent[] {
    const wasPinched = this.#pinch !== null && this.#pinch.base !== null;
    this.#pinch = null;
    this.#lastTap = null;
    this.#phase = this.#fingers.size > 0 ? "over" : "idle";
    return wasPinched ? [{ kind: "pinchEnd" }] : [];
  }

  /** Forgets every finger: those whose release was never heard of would be in the way for good. */
  reset(): TouchIntent[] {
    if (this.#fingers.size === 0) {
      return [];
    }
    this.#fingers.clear();
    return this.abort();
  }

  #startPan(finger: Finger): TouchIntent[] {
    const { x, y } = this.#origin;
    const dx = x - finger.x;
    const dy = y - finger.y;
    const length = Math.hypot(dx, dy);
    if (length <= SLOP) {
      return [];
    }
    this.#phase = "pan";
    this.#sample(finger);
    this.#lock =
      Math.abs(dx) > LOCK_RATIO * Math.abs(dy)
        ? "x"
        : Math.abs(dy) > LOCK_RATIO * Math.abs(dx)
          ? "y"
          : null;
    this.#along = 0;
    this.#across = 0;
    // What it has strayed while it only pressed is no part of it.
    const rest = 1 - SLOP / length;
    return this.#pan(dx * rest, dy * rest);
  }

  #movePan(before: Finger, finger: Finger): TouchIntent[] {
    this.#sample(finger);
    const dx = before.x - finger.x;
    const dy = before.y - finger.y;
    if (this.#lock) {
      this.#along += this.#lock === "x" ? dx : dy;
      this.#across += this.#lock === "x" ? dy : dx;
      if (Math.abs(this.#along) >= LOCK_CHANNEL) {
        this.#along = 0;
        this.#across = 0;
      } else if (Math.abs(this.#across) >= LOCK_CHANNEL) {
        this.#lock = null;
      }
    }
    return this.#pan(dx, dy);
  }

  #pan(dx: number, dy: number): TouchIntent[] {
    const pan = {
      kind: "pan",
      dx: this.#lock === "y" ? 0 : dx,
      dy: this.#lock === "x" ? 0 : dy,
    } as const;
    return pan.dx === 0 && pan.dy === 0 ? [] : [pan];
  }

  #sample(finger: Finger): void {
    this.#samples = this.#samples.filter(({ time }) => time >= finger.time - SPEED_MS);
    this.#samples.push(finger);
  }

  #startPinch(finger: Finger): void {
    const first = this.#fingers.get(this.#main) ?? finger;
    this.#phase = "pinch";
    this.#pinch = {
      other: finger.id,
      base: null,
      span: distance(first, finger),
      ...middle(first, finger),
    };
  }

  #movePinch(): TouchIntent[] {
    const pinch = this.#pinch;
    const first = this.#fingers.get(this.#main);
    const second = pinch && this.#fingers.get(pinch.other);
    if (!pinch || !first || !second) {
      return [];
    }
    const { x, y } = middle(first, second);
    const intents: TouchIntent[] = [];
    if (x !== pinch.x || y !== pinch.y) {
      intents.push({ kind: "pan", dx: pinch.x - x, dy: pinch.y - y });
      pinch.x = x;
      pinch.y = y;
    }
    const span = distance(first, second);
    const grown = span - pinch.span;
    if (pinch.base === null && Math.abs(grown) > SPAN_SLOP) {
      // It starts from a scale of 1, not from that of all the fingers have moved.
      pinch.base = Math.max(1, pinch.span + Math.sign(grown) * SPAN_SLOP);
      intents.push({ kind: "pinchStart" });
    }
    if (pinch.base !== null) {
      intents.push({ kind: "pinch", scale: span / pinch.base, x, y });
    }
    return intents;
  }

  /** One of two fingers is lifted: the other goes on with a pan from where it is. */
  #liftFromPinch(lifted: Finger): TouchIntent[] {
    const pinch = this.#pinch;
    if (!pinch || (lifted.id !== this.#main && lifted.id !== pinch.other)) {
      return [];
    }
    const other = this.#fingers.get(lifted.id === this.#main ? pinch.other : this.#main);
    const intents = this.abort();
    if (other) {
      this.#phase = "pan";
      this.#main = other.id;
      this.#lock = null;
      this.#leftBehind = other;
      this.#samples = [{ ...other, time: lifted.time }];
    }
    return intents;
  }

  #release(finger: Finger): TouchIntent[] {
    const { x, y } = finger;
    if (this.#phase === "press") {
      if (this.#hasCaught || finger.time - this.#origin.time > LONG_PRESS_MS) {
        return [];
      }
      this.#lastTap = this.#isSecond ? null : finger;
      return [{ kind: this.#isSecond ? "doubleTap" : "tap", x, y }];
    }
    // The two fingers of a pinch are seldom lifted at the very same time.
    const isRest = this.#leftBehind !== null && distance(this.#leftBehind, finger) <= SLOP;
    return this.#phase === "pan" && !isRest ? this.#fling(finger) : [];
  }

  #fling(finger: Finger): TouchIntent[] {
    const moving = this.#samples.findLastIndex(({ x, y }) => x !== finger.x || y !== finger.y);
    const [first] = this.#samples;
    const arrived = this.#samples[moving + 1];
    const elapsed = first ? finger.time - first.time : 0;
    if (!first || !arrived || moving < 0 || elapsed <= 0 || finger.time - arrived.time > REST_MS) {
      return [];
    }
    const speed = (from: number, to: number) =>
      Math.max(-MOST_FLING, Math.min(MOST_FLING, (from - to) / elapsed));
    const vx = this.#lock === "y" ? 0 : speed(first.x, finger.x);
    const vy = this.#lock === "x" ? 0 : speed(first.y, finger.y);
    return Math.hypot(vx, vy) < LEAST_FLING ? [] : [{ kind: "fling", vx, vy }];
  }
}

interface Place {
  readonly x: number;
  readonly y: number;
}

function distance(one: Place, other: Place): number {
  return Math.hypot(one.x - other.x, one.y - other.y);
}

function middle(one: Place, other: Place): Place {
  return { x: (one.x + other.x) / 2, y: (one.y + other.y) / 2 };
}

/** How long a sheet that was let go takes to lose all but a third of its speed. */
const FRICTION_MS = 400;
/** It stands still below this speed, in pixels per millisecond. */
const STILL = 0.03;
/** A frame that comes later than this was held up: the sheet does not jump to make up for it. */
const LONGEST_STEP_MS = 64;

/** A sheet that was let go: how far it moves from one frame to the next. */
export class Fling {
  #vx: number;
  #vy: number;
  #time: number;

  constructor(vx: number, vy: number, time: number) {
    this.#vx = vx;
    this.#vy = vy;
    this.#time = time;
  }

  get isOver(): boolean {
    return Math.hypot(this.#vx, this.#vy) < STILL;
  }

  /** How far it has moved since the last step. */
  step(time: number): { readonly dx: number; readonly dy: number } {
    // A frame may have begun before the event that came to be handled in it.
    const elapsed = Math.min(LONGEST_STEP_MS, Math.max(0, time - this.#time));
    this.#time = Math.max(time, this.#time);
    const left = Math.exp(-elapsed / FRICTION_MS);
    const dx = this.#vx * FRICTION_MS * (1 - left);
    const dy = this.#vy * FRICTION_MS * (1 - left);
    this.#vx *= left;
    this.#vy *= left;
    return { dx, dy };
  }

  /** It has come to the end of the sheet. */
  stopX(): void {
    this.#vx = 0;
  }

  stopY(): void {
    this.#vy = 0;
  }
}
