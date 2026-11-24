import { describe, expect, test } from "vitest";
import { clampZoom, MAX_ZOOM, MIN_ZOOM, nextZoom, ZOOM_STEPS, ZoomGesture } from "./zoom.ts";

describe("the zooms", () => {
  test("go from a quarter to twice the size, in order", () => {
    expect(ZOOM_STEPS[0]).toBe(MIN_ZOOM);
    expect(ZOOM_STEPS.at(-1)).toBe(MAX_ZOOM);
    expect([...ZOOM_STEPS].sort((a, b) => a - b)).toEqual(ZOOM_STEPS);
    expect(ZOOM_STEPS).toContain(1);
  });

  test("are kept within the limits", () => {
    expect([0, 0.1, 0.25, 0.85, 2, 7].map((zoom) => clampZoom(zoom))).toEqual([
      0.25, 0.25, 0.25, 0.85, 2, 2,
    ]);
    expect(clampZoom(Number.NaN)).toBe(1);
    expect(clampZoom(Number.POSITIVE_INFINITY, 0.5)).toBe(0.5);
  });

  test("lead from one to the next and back", () => {
    for (let step = 0; step + 1 < ZOOM_STEPS.length; step++) {
      const [lower = 0, upper = 0] = ZOOM_STEPS.slice(step, step + 2);
      expect(nextZoom(lower, 1)).toBe(upper);
      expect(nextZoom(upper, -1)).toBe(lower);
      expect(nextZoom((lower + upper) / 2, 1)).toBe(upper);
      expect(nextZoom((lower + upper) / 2, -1)).toBe(lower);
    }
    expect(nextZoom(MAX_ZOOM, 1)).toBe(MAX_ZOOM);
    expect(nextZoom(MIN_ZOOM, -1)).toBe(MIN_ZOOM);
    // Arithmetic that is a little off does not lead to the same zoom again.
    expect(nextZoom(0.1 + 1, 1)).toBe(1.25);
    expect(nextZoom(1.1 - 1e-12, 1)).toBe(1.25);
  });
});

describe("a turn of the wheel", () => {
  test("is a step, whichever way the wheel reports it", () => {
    for (const delta of [40, 100, 120, 125, 360]) {
      expect(new ZoomGesture().turn(-delta, 1)).toBe(1.1);
      expect(new ZoomGesture().turn(delta, 1)).toBe(0.9);
    }
    expect(new ZoomGesture().turn(100, MIN_ZOOM)).toBeNull();
    expect(new ZoomGesture().turn(-100, MAX_ZOOM)).toBeNull();
  });

  test("that comes in small pieces is one all the same", () => {
    const gesture = new ZoomGesture();
    let zoom = 1;
    const steps: number[] = [];
    for (let piece = 0; piece < 50; piece++) {
      const next = gesture.turn(-4, zoom);
      if (next !== null) {
        zoom = next;
        steps.push(next);
      }
    }
    expect(steps).toEqual([1.1, 1.25, 1.5, 1.75, 2]);
  });

  test("the other way forgets what was left of the one before", () => {
    const gesture = new ZoomGesture();
    expect(gesture.turn(-39, 1)).toBeNull();
    expect(gesture.turn(39, 1)).toBeNull();
    expect(gesture.turn(1, 1)).toBe(0.9);
    gesture.turn(-39, 1);
    gesture.end();
    expect(gesture.turn(-1, 1)).toBeNull();
  });
});

describe("a pinch", () => {
  /** The zooms that small movements lead through. */
  const pinch = (gesture: ZoomGesture, deltas: readonly number[], from: number): number[] => {
    const zooms: number[] = [];
    let zoom = from;
    for (const delta of deltas) {
      const next = gesture.pinch(delta, zoom);
      if (next !== null) {
        zoom = next;
        zooms.push(next);
      }
    }
    return zooms;
  };

  test("stops at multiples of five per cent, however small its movements", () => {
    const zooms = pinch(new ZoomGesture(), new Array(400).fill(-0.2), 1);
    expect(zooms[0]).toBe(1.05);
    for (const zoom of zooms) {
      expect(Math.abs(zoom * 20 - Math.round(zoom * 20))).toBeLessThan(1e-9);
    }
    expect(zooms.at(-1)).toBe(2);
    expect([...zooms].sort((a, b) => a - b)).toEqual(zooms);
  });

  test("there and back ends where it began", () => {
    const gesture = new ZoomGesture();
    const there = pinch(gesture, new Array(37).fill(-1.3), 1);
    const back = pinch(gesture, new Array(37).fill(1.3), there.at(-1) ?? 1);
    expect(there.length).toBeGreaterThan(5);
    expect(back.at(-1)).toBe(1);
  });

  test("beyond a limit turns round at once", () => {
    const gesture = new ZoomGesture();
    expect(pinch(gesture, new Array(100).fill(-20), 1).at(-1)).toBe(2);
    expect(gesture.pinch(3, 2)).toBe(1.95);
  });

  test("goes the way it is made from a zoom that is no multiple", () => {
    for (const from of [0.33, 0.67]) {
      expect(pinch(new ZoomGesture(), new Array(30).fill(-0.5), from)[0]).toBeGreaterThan(from);
      expect(pinch(new ZoomGesture(), new Array(30).fill(0.5), from)[0]).toBeLessThan(from);
      expect(new ZoomGesture().pinch(-0.1, from)).toBeNull();
    }
  });

  test("does not leap when it is a wheel after all", () => {
    expect(new ZoomGesture().pinch(120, 1)).toBeGreaterThan(0.7);
    expect(new ZoomGesture().pinch(-120, 1)).toBeLessThan(1.4);
  });

  test("by how far the fingers are apart is from where it began", () => {
    const gesture = new ZoomGesture();
    gesture.begin(0.8);
    expect(gesture.spread(1.01, 0.8)).toBeNull();
    expect(gesture.spread(1.25, 0.8)).toBe(1);
    expect(gesture.spread(1.25, 1)).toBeNull();
    expect(gesture.spread(1, 1)).toBe(0.8);
    expect(gesture.spread(100, 0.8)).toBe(2);
    expect(gesture.spread(0.001, 2)).toBe(0.25);
  });

  test("goes on from where something else has zoomed to meanwhile", () => {
    const gesture = new ZoomGesture();
    gesture.begin(1);
    expect(gesture.spread(0.5, 1)).toBe(0.5);
    gesture.end();
    expect(gesture.spread(0.5, 0.75)).toBeNull();
    expect(gesture.spread(0.6, 0.75)).toBe(0.9);
    // What is no scale leads nowhere, now and later.
    gesture.end();
    expect(gesture.spread(0, 0.5)).toBeNull();
    expect(gesture.spread(Number.NaN, 0.5)).toBeNull();
    expect(gesture.spread(1, 0.5)).toBeNull();
    expect(gesture.spread(1.2, 0.5)).toBe(0.6);
  });
});
