import { describe, expect, test } from "vitest";
import {
  DOUBLE_TAP_MS,
  DOUBLE_TAP_REACH,
  Fling,
  LOCK_CHANNEL,
  LONG_PRESS_MS,
  SLOP,
  SPAN_SLOP,
  TouchGesture,
  type TouchIntent,
} from "./touch.ts";

/** Fingers on a touch screen whose time goes on by 8 ms from event to event. */
function hand(gesture = new TouchGesture()) {
  let time = 1_000;
  const at = (id: number, x: number, y: number) => {
    time += 8;
    return { id, x, y, time };
  };
  return {
    gesture,
    down: (id: number, x: number, y: number, hasCaught = false) =>
      gesture.down(at(id, x, y), hasCaught),
    move: (id: number, x: number, y: number) => gesture.move(at(id, x, y)),
    up: (id: number, x: number, y: number) => gesture.up(at(id, x, y)),
    cancel: (id: number) => gesture.cancel(id),
    wait: (ms: number) => {
      time += ms;
    },
  };
}

const kinds = (intents: readonly TouchIntent[]) => intents.map(({ kind }) => kind);

/** How far the sheet is scrolled by all of it. */
function panned(...intents: readonly TouchIntent[][]) {
  let dx = 0;
  let dy = 0;
  for (const intent of intents.flat()) {
    if (intent.kind === "pan") {
      dx += intent.dx;
      dy += intent.dy;
    }
  }
  return { dx, dy };
}

function scaleOf(intents: readonly TouchIntent[]): number | undefined {
  return intents.find((intent) => intent.kind === "pinch")?.scale;
}

describe("a tap", () => {
  test("is a finger that comes down and is lifted where it was", () => {
    const { down, move, up } = hand();
    expect(down(1, 50, 60)).toEqual([]);
    expect(move(1, 53, 62)).toEqual([]);
    expect(up(1, 53, 62)).toEqual([{ kind: "tap", x: 53, y: 62 }]);
  });

  test("is none if the finger has strayed, however far it comes back", () => {
    const { down, move, up } = hand();
    down(1, 50, 60);
    move(1, 50 + SLOP + 1, 60);
    move(1, 50, 60);
    expect(kinds(up(1, 50, 60))).not.toContain("tap");
  });

  test("is none if the finger stays for long", () => {
    const { down, up, wait } = hand();
    down(1, 50, 60);
    wait(LONG_PRESS_MS + 1);
    expect(up(1, 50, 60)).toEqual([]);
    // And is not the first half of a double tap either.
    down(1, 50, 60);
    expect(kinds(up(1, 50, 60))).toEqual(["tap"]);
  });

  test("is none if it is the finger that has stopped the sheet", () => {
    const { down, up } = hand();
    down(1, 50, 60, true);
    expect(up(1, 50, 60)).toEqual([]);
    down(1, 50, 60);
    expect(kinds(up(1, 50, 60))).toEqual(["tap"]);
  });

  test("is none if the finger is cancelled, or another one has been there", () => {
    const { down, up, cancel } = hand();
    down(1, 50, 60);
    expect(cancel(1)).toEqual([]);
    expect(up(1, 50, 60)).toEqual([]);
    down(1, 50, 60);
    down(2, 90, 60);
    expect(up(2, 90, 60)).toEqual([]);
    expect(up(1, 50, 60)).toEqual([]);
  });

  test("counts where the finger is lifted, even if nothing was said of it moving", () => {
    const { down, up } = hand();
    down(1, 50, 60);
    expect(kinds(up(1, 50, 90))).toEqual(["pan", "fling"]);
  });
});

describe("a double tap", () => {
  const tap = ({ down, up }: ReturnType<typeof hand>, x: number, y: number) => {
    down(1, x, y);
    return kinds(up(1, x, y));
  };

  test("is the second of two taps in short order, and only that one", () => {
    const one = hand();
    expect(tap(one, 50, 60)).toEqual(["tap"]);
    one.wait(100);
    expect(tap(one, 60, 60)).toEqual(["doubleTap"]);
    one.wait(100);
    expect(tap(one, 60, 60)).toEqual(["tap"]);
    one.wait(100);
    expect(tap(one, 60, 60)).toEqual(["doubleTap"]);
  });

  test("has the second one near the first, and soon after it", () => {
    const one = hand();
    tap(one, 50, 60);
    one.wait(DOUBLE_TAP_MS + 1);
    expect(tap(one, 50, 60)).toEqual(["tap"]);
    expect(tap(one, 50 + DOUBLE_TAP_REACH + 1, 60)).toEqual(["tap"]);
  });

  test("is one though fingers are forgotten in between of which there are none", () => {
    const one = hand();
    tap(one, 50, 60);
    expect(one.gesture.reset()).toEqual([]);
    expect(tap(one, 50, 60)).toEqual(["doubleTap"]);
  });

  test("is none with a pan, a pinch or a cancelled finger in between", () => {
    for (const between of ["pan", "pinch", "cancel", "long press"] as const) {
      const one = hand();
      tap(one, 50, 60);
      one.down(1, 50, 60);
      if (between === "pan") {
        one.move(1, 50, 120);
        one.wait(80);
        one.move(1, 50, 60);
        one.up(1, 50, 60);
      } else if (between === "pinch") {
        one.down(2, 90, 60);
        one.up(2, 90, 60);
        one.up(1, 50, 60);
      } else if (between === "cancel") {
        one.cancel(1);
      } else {
        one.wait(LONG_PRESS_MS + 1);
        expect(one.up(1, 50, 60)).toEqual([]);
      }
      expect(tap(one, 50, 60), between).toEqual(["tap"]);
    }
  });

  test("may be the start of a pan instead", () => {
    const one = hand();
    tap(one, 50, 60);
    one.down(1, 50, 60);
    expect(kinds(one.move(1, 50, 100))).toEqual(["pan"]);
    one.wait(200);
    expect(one.up(1, 50, 100)).toEqual([]);
  });
});

describe("a pan", () => {
  test("follows the finger in both directions, but for what it has strayed before", () => {
    const { down, move } = hand();
    down(1, 100, 100);
    const all = [move(1, 94, 94), move(1, 80, 80), move(1, 60, 70), move(1, 75, 130)];
    const { dx, dy } = panned(...all);
    const rest = 1 - SLOP / Math.hypot(20, 20);
    expect(dx).toBeCloseTo(20 * rest + 5, 9);
    expect(dy).toBeCloseTo(20 * rest - 50, 9);
  });

  test("keeps to the axis it starts along", () => {
    for (const [axis, across] of [
      ["x", "y"],
      ["y", "x"],
    ] as const) {
      const { down, move } = hand();
      const point = (along: number, off: number): [number, number] =>
        axis === "x" ? [along, off] : [off, along];
      down(1, ...point(300, 300));
      const all = [
        move(1, ...point(280, 296)),
        move(1, ...point(240, 290)),
        move(1, ...point(200, 310)),
        move(1, ...point(160, 296)),
      ];
      expect(panned(...all)[axis === "x" ? "dx" : "dy"]).toBeGreaterThan(130);
      expect(panned(...all)[across === "x" ? "dx" : "dy"]).toBe(0);
    }
  });

  test("leaves the axis when the finger turns, and without a jump", () => {
    const { down, move } = hand();
    down(1, 300, 300);
    move(1, 300, 260);
    move(1, 300, 220);
    expect(panned(move(1, 300 - LOCK_CHANNEL + 2, 218)).dx).toBe(0);
    expect(panned(move(1, 300 - LOCK_CHANNEL - 1, 217))).toEqual({ dx: 3, dy: 1 });
    expect(panned(move(1, 240, 200))).toEqual({ dx: 300 - LOCK_CHANNEL - 1 - 240, dy: 17 });
  });

  test("is not taken off its axis by a finger that drifts while it goes on", () => {
    const { down, move } = hand();
    down(1, 300, 600);
    for (let step = 1; step <= 25; step++) {
      expect(panned(move(1, 300 - step * 4, 600 - step * 20)).dx).toBe(0);
    }
  });

  test("of no length is none", () => {
    const { down, move } = hand();
    down(1, 100, 100);
    move(1, 100, 130);
    expect(move(1, 100, 130)).toEqual([]);
    // Across the axis it keeps to.
    expect(move(1, 103, 130)).toEqual([]);
  });

  test("ends where it is when the finger is cancelled, and says nothing of a speed", () => {
    const { down, move, up, cancel } = hand();
    down(1, 100, 100);
    move(1, 100, 80);
    move(1, 100, 20);
    expect(cancel(1)).toEqual([]);
    expect(move(1, 100, 0)).toEqual([]);
    expect(up(1, 100, 0)).toEqual([]);
    down(1, 100, 100);
    expect(kinds(up(1, 100, 100))).toEqual(["tap"]);
  });
});

describe("a sheet that is let go", () => {
  const swipe = (one: ReturnType<typeof hand>, step: number, steps = 6) => {
    one.down(1, 100, 400);
    for (let at = 1; at <= steps; at++) {
      one.move(1, 100, 400 - at * step);
    }
    return 400 - steps * step;
  };

  test("goes on at the speed of the finger", () => {
    const one = hand();
    const y = swipe(one, 16);
    const [fling, ...rest] = one.up(1, 100, y);
    expect(rest).toEqual([]);
    expect(fling).toMatchObject({ kind: "fling", vx: 0 });
    expect(fling?.kind === "fling" && fling.vy).toBeGreaterThan(1.4);
    expect(fling?.kind === "fling" && fling.vy).toBeLessThanOrEqual(2);
  });

  test("stays when the finger has rested or was slow", () => {
    const rested = hand();
    const y = swipe(rested, 16);
    rested.wait(60);
    expect(rested.up(1, 100, y)).toEqual([]);
    const slow = hand();
    expect(slow.up(1, 100, swipe(slow, 1, 14))).toEqual([]);
  });

  test("goes the way the finger went last, not all in all", () => {
    const one = hand();
    let y = swipe(one, 30);
    one.wait(150);
    for (let step = 0; step < 5; step++) {
      y += 10;
      one.move(1, 100, y);
    }
    const [fling] = one.up(1, 100, y);
    expect(fling?.kind === "fling" && fling.vy).toBeLessThan(-0.5);
  });

  test("is no faster than what can be followed", () => {
    const one = hand();
    const y = swipe(one, 200, 3);
    expect(one.up(1, 100, y)).toMatchObject([{ vy: 8 }]);
  });

  test("goes as far as a flick of two events says", () => {
    const one = hand();
    one.down(1, 100, 400);
    one.move(1, 100, 360);
    expect(kinds(one.up(1, 100, 360))).toEqual(["fling"]);
  });

  test("slows down, and comes to rest", () => {
    const fling = new Fling(0, 2, 100);
    let last = Number.POSITIVE_INFINITY;
    let total = 0;
    let time = 100;
    while (!fling.isOver) {
      time += 16;
      const { dx, dy } = fling.step(time);
      expect(dx).toBe(0);
      expect(dy).toBeLessThan(last);
      expect(dy).toBeGreaterThan(0);
      last = dy;
      total += dy;
      expect(time).toBeLessThan(5_000);
    }
    expect(total).toBeGreaterThan(700);
    expect(total).toBeLessThan(800);
  });

  test("goes as far at any frame rate, and does not jump after a pause", () => {
    const distance = (frame: number) => {
      const fling = new Fling(1.5, 0, 0);
      let total = 0;
      for (let time = frame; time <= 960; time += frame) {
        total += fling.step(time).dx;
      }
      return total;
    };
    expect(distance(4)).toBeCloseTo(distance(16), 6);
    expect(new Fling(1, 0, 0).step(5_000).dx).toBe(new Fling(1, 0, 0).step(64).dx);
    expect(new Fling(1, 0, 50).step(40)).toEqual({ dx: 0, dy: 0 });
  });

  test("is stopped along each axis by itself", () => {
    const fling = new Fling(1, 1, 0);
    fling.stopX();
    expect(fling.isOver).toBe(false);
    expect(fling.step(16).dx).toBe(0);
    fling.stopY();
    expect(fling.isOver).toBe(true);
  });
});

describe("a pinch", () => {
  test("enlarges by how far the fingers move apart, and starts from nothing", () => {
    const { down, move } = hand();
    down(1, 100, 100);
    expect(down(2, 200, 100)).toEqual([]);
    expect(kinds(move(2, 200 + SPAN_SLOP, 100))).toEqual(["pan"]);
    const first = move(2, 200 + SPAN_SLOP + 1, 100);
    expect(kinds(first)).toEqual(["pan", "pinchStart", "pinch"]);
    const base = 100 + SPAN_SLOP;
    expect(scaleOf(first)).toBeCloseTo((base + 1) / base, 9);
    expect(scaleOf(move(2, 100 + 2 * base, 100))).toBeCloseTo(2, 9);
    expect(scaleOf(move(1, 100 + base, 100))).toBeCloseTo(1, 9);
  });

  test("makes smaller when they move together, which may be all the way", () => {
    const { down, move } = hand();
    down(1, 100, 100);
    down(2, 200, 100);
    const base = 100 - SPAN_SLOP;
    expect(scaleOf(move(2, 100 + base / 2, 100))).toBeCloseTo(0.5, 9);
    expect(scaleOf(move(2, 100, 100))).toBe(0);
    const { down: press, move: spread } = hand();
    press(1, 100, 100);
    press(2, 100, 100);
    expect(scaleOf(spread(2, 160, 100))).toBeCloseTo(60 / SPAN_SLOP, 9);
  });

  test("is at the middle of the fingers, with which the sheet goes", () => {
    const { down, move } = hand();
    down(1, 100, 100);
    down(2, 200, 100);
    const all = [move(1, 80, 60), move(2, 240, 80)];
    expect(all[1]?.findLast(({ kind }) => kind === "pinch")).toMatchObject({ x: 160, y: 70 });
    expect(panned(...all)).toEqual({ dx: -10, dy: 30 });
    // What the sheet is moved by comes before what it is enlarged by at the new place.
    expect(kinds(move(1, 60, 60))).toEqual(["pan", "pinch"]);
  });

  test("is none while the fingers only move together", () => {
    const { down, move, up } = hand();
    down(1, 100, 100);
    down(2, 200, 100);
    const all: TouchIntent[][] = [];
    for (let step = 1; step <= 10; step++) {
      all.push(move(1, 100 - step * 5, 100 - step * 5), move(2, 200 - step * 5, 100 - step * 5));
    }
    expect(new Set(all.flatMap(kinds))).toEqual(new Set(["pan"]));
    expect(panned(...all)).toEqual({ dx: 50, dy: 50 });
    expect(up(2, 150, 50)).toEqual([]);
  });

  test("may begin in the middle of a pan", () => {
    const { down, move } = hand();
    down(1, 100, 100);
    move(1, 100, 160);
    down(2, 200, 160);
    expect(kinds(move(2, 260, 160))).toEqual(["pan", "pinchStart", "pinch"]);
  });

  test("goes on as a pan with the finger that stays, without a jump", () => {
    for (const lifted of [1, 2]) {
      const { down, move, up } = hand();
      down(1, 100, 100);
      down(2, 200, 100);
      move(2, 300, 100);
      const [x, stays] = lifted === 1 ? [300, 2] : [100, 1];
      expect(kinds(up(lifted, lifted === 1 ? 100 : 300, 100))).toEqual(["pinchEnd"]);
      // At once, in any direction, and all of it.
      expect(move(stays, x - 3, 98)).toEqual([{ kind: "pan", dx: 3, dy: 2 }]);
      expect(move(stays, x - 4, 60)).toEqual([{ kind: "pan", dx: 1, dy: 38 }]);
      expect(kinds(up(stays, x - 4, 60))).toEqual(["fling"]);
    }
  });

  test("does not let the sheet go when the fingers are lifted one after the other", () => {
    const { down, move, up } = hand();
    down(1, 100, 100);
    down(2, 200, 100);
    move(2, 300, 100);
    up(2, 310, 100);
    move(1, 96, 100);
    expect(up(1, 94, 100).filter(({ kind }) => kind !== "pan")).toEqual([]);
  });

  test("is taken up again by another finger", () => {
    const { down, move, up } = hand();
    down(1, 100, 100);
    down(2, 200, 100);
    move(2, 300, 100);
    up(2, 300, 100);
    down(3, 200, 100);
    expect(kinds(move(3, 260, 100))).toEqual(["pan", "pinchStart", "pinch"]);
    expect(kinds(up(1, 100, 100))).toEqual(["pinchEnd"]);
    up(3, 260, 100);
    down(1, 100, 100);
    expect(kinds(up(1, 100, 100))).toEqual(["tap"]);
  });

  test("is of two fingers, whatever a third one does", () => {
    const { down, move, up } = hand();
    down(1, 100, 100);
    down(2, 200, 100);
    move(2, 300, 100);
    expect(down(3, 150, 300)).toEqual([]);
    expect(panned(move(3, 150, 400))).toEqual({ dx: 0, dy: 0 });
    expect(kinds(up(3, 150, 400))).not.toContain("pinchEnd");
    down(3, 150, 300);
    expect(kinds(up(1, 100, 100))).toEqual(["pinchEnd"]);
    expect(move(3, 150, 350)).toEqual([]);
    expect(move(2, 290, 100)).toEqual([{ kind: "pan", dx: 10, dy: 0 }]);
    // The third one is the last, and has nothing to do until it is lifted.
    up(2, 200, 100);
    expect(move(3, 150, 500)).toEqual([]);
    expect(up(3, 150, 500)).toEqual([]);
    down(1, 100, 100);
    expect(kinds(up(1, 100, 100))).toEqual(["tap"]);
  });

  test("ends when one of its fingers is cancelled, and the other is of no account until it is lifted", () => {
    const { down, move, up, cancel } = hand();
    down(1, 100, 100);
    down(2, 200, 100);
    move(2, 300, 100);
    expect(kinds(cancel(2))).toEqual(["pinchEnd"]);
    expect(move(1, 100, 200)).toEqual([]);
    expect(up(1, 100, 200)).toEqual([]);
    down(1, 100, 100);
    expect(kinds(up(1, 100, 100))).toEqual(["tap"]);
  });
});

describe("fingers", () => {
  test("that nobody has seen come down are none", () => {
    const { move, up, cancel } = hand();
    expect(move(7, 10, 10)).toEqual([]);
    expect(up(7, 10, 10)).toEqual([]);
    expect(cancel(7)).toEqual([]);
  });

  test("are of no account after the gesture was called off, until all of them are lifted", () => {
    const { gesture, down, move, up } = hand();
    down(1, 100, 100);
    down(2, 200, 100);
    move(2, 300, 100);
    expect(kinds(gesture.abort())).toEqual(["pinchEnd"]);
    expect(gesture.abort()).toEqual([]);
    expect(move(2, 400, 100)).toEqual([]);
    expect(down(3, 100, 300)).toEqual([]);
    for (const id of [1, 2, 3]) {
      expect(up(id, 100, 300)).toEqual([]);
    }
    down(1, 100, 100);
    expect(kinds(up(1, 100, 100))).toEqual(["tap"]);
  });

  test("whose release was never heard of are forgotten", () => {
    const { gesture, down, move, up } = hand();
    down(1, 100, 100);
    // The same finger comes down again.
    down(1, 100, 100);
    expect(kinds(up(1, 100, 100))).toEqual(["tap"]);
    down(1, 100, 100);
    down(2, 200, 100);
    move(2, 300, 100);
    expect(kinds(gesture.reset())).toEqual(["pinchEnd"]);
    down(3, 100, 100);
    expect(kinds(move(3, 100, 150))).toEqual(["pan"]);
    expect(move(2, 300, 300)).toEqual([]);
  });

  test("never do anything but what there is a name for, whatever they do", () => {
    // A linear congruential generator, so that a failure can be repeated.
    let seed = 7;
    const random = (below: number) => {
      seed = (seed * 1_103_515_245 + 12_345) % 2_147_483_648;
      return Math.floor((seed / 2_147_483_648) * below);
    };
    const { gesture, down, move, up, cancel, wait } = hand();
    let isPinching = false;
    for (let event = 0; event < 20_000; event++) {
      const id = 1 + random(4);
      const [x, y] = [random(400), random(800)];
      const intents = [
        () => down(id, x, y, random(5) === 0),
        () => move(id, x, y),
        () => move(id, x, y),
        () => up(id, x, y),
        () => cancel(id),
        () => (random(20) === 0 ? gesture.abort() : []),
        () => (random(20) === 0 ? gesture.reset() : []),
      ][random(7)]?.();
      wait(random(120));
      for (const intent of intents ?? []) {
        for (const value of Object.values(intent)) {
          expect(typeof value === "string" || Number.isFinite(value)).toBe(true);
        }
        if (intent.kind === "pinchStart" || intent.kind === "pinchEnd") {
          expect(isPinching).toBe(intent.kind === "pinchEnd");
          isPinching = !isPinching;
        } else if (intent.kind === "pinch") {
          expect(isPinching).toBe(true);
          expect(intent.scale).toBeGreaterThanOrEqual(0);
        } else if (intent.kind !== "pan") {
          expect(isPinching).toBe(false);
        }
      }
    }
  });
});
