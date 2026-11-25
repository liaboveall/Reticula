import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { Point } from "./layout.ts";
import { TouchInput } from "./touch-input.ts";

// What becomes of the fingers' intents, on a grid that only takes them down.

const HEADERS = { headerWidth: 50, headerHeight: 20 };
const LIMITS = { top: 1_000, left: 400 };

let frames = new Map<number, (time: number) => void>();
let wantsLessMotion = false;
let now = 0;
let page = new EventTarget();

beforeEach(() => {
  frames = new Map();
  wantsLessMotion = false;
  now = 1_000;
  page = new EventTarget();
  vi.stubGlobal("window", page);
  vi.stubGlobal("performance", { now: () => now });
  vi.stubGlobal("matchMedia", () => ({ matches: wantsLessMotion }));
  let requests = 0;
  vi.stubGlobal("requestAnimationFrame", (callback: (time: number) => void) => {
    frames.set(++requests, callback);
    return requests;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function open({ zooms = true, capture = () => {}, filter = false } = {}) {
  const log: string[] = [];
  let scroll = { top: 500, left: 200 };
  const grid = {
    zoom: 1,
    setZoom(zoom: number, point?: Point) {
      grid.zoom = zoom;
      log.push(`zoom ${zoom} at ${point?.x},${point?.y}`);
      return true;
    },
    focus: () => void log.push("focus"),
    select: (cell: { row: number; col: number } | null) =>
      void log.push(`cell ${cell?.row},${cell?.col}`),
    selectRows: (first: number, last: number) => void log.push(`rows ${first}-${last}`),
    selectCols: (first: number, last: number) => void log.push(`cols ${first}-${last}`),
    selectAll: () => void log.push("all"),
  };
  const host = new EventTarget();
  const input = {
    getBoundingClientRect: () => ({ left: 10, top: 30 }),
    setPointerCapture: capture,
  };
  const touch = new TouchInput({
    grid,
    host: host as HTMLElement,
    input: input as unknown as HTMLElement,
    zooms,
    metrics: () => ({ ...HEADERS, rowHeight: 20, colWidth: 100, scrollbarSize: 12 }),
    scroll: () => scroll,
    scrollBy: (dx, dy) => {
      scroll = {
        top: Math.max(0, Math.min(LIMITS.top, scroll.top + dy)),
        left: Math.max(0, Math.min(LIMITS.left, scroll.left + dx)),
      };
    },
    cellAt: (x, y) =>
      x >= 1_000
        ? null
        : {
            row: Math.floor((y - HEADERS.headerHeight + scroll.top) / 20),
            col: Math.floor((x - HEADERS.headerWidth + scroll.left) / 100),
          },
    beginPinch: () => void log.push("begin"),
    pinch: (scale, { x, y }) => void log.push(`pinch ${scale.toFixed(2)} at ${x},${y}`),
    endPinch: () => void log.push("end"),
    pressButton: () => {
      if (filter) {
        log.push("filter");
      }
      return filter;
    },
  });
  const send = (type: string, pointerId: number, x: number, y: number, isPrimary = false) => {
    now += 8;
    if (type === "pointerdown") {
      press("touch");
    }
    touch.handle({
      type,
      pointerId,
      isPrimary,
      timeStamp: now,
      clientX: x + 10,
      clientY: y + 30,
    } as PointerEvent);
  };
  /** Any pointer, anywhere on the page. */
  const press = (pointerType: string) =>
    page.dispatchEvent(Object.assign(new Event("pointerdown"), { pointerType }));
  /** The next frame, if there is to be one. */
  const frame = () => {
    now += 16;
    for (const [id, callback] of [...frames]) {
      frames.delete(id);
      callback(now);
    }
  };
  const swipe = (dx: number, dy: number) => {
    send("pointerdown", 1, 300, 300, true);
    for (let step = 1; step <= 5; step++) {
      send("pointermove", 1, 300 + dx * step, 300 + dy * step);
    }
    send("pointerup", 1, 300 + dx * 5, 300 + dy * 5);
  };
  const tap = (x: number, y: number) => {
    send("pointerdown", 1, x, y, true);
    send("pointerup", 1, x, y);
    send("lostpointercapture", 1, x, y);
    now += 400;
  };
  return { touch, grid, host, log, send, press, frame, swipe, tap, scroll: () => scroll };
}

describe("a pan", () => {
  test("scrolls, as far as there is anything", () => {
    const { send, scroll } = open();
    send("pointerdown", 1, 300, 300, true);
    send("pointermove", 1, 290, 290);
    const start = scroll();
    send("pointermove", 1, 250, 220);
    expect(scroll().top - start.top).toBeCloseTo(70, 9);
    expect(scroll().left - start.left).toBeCloseTo(40, 9);
    send("pointermove", 1, 2_000, 4_000);
    expect(scroll()).toEqual({ top: 0, left: 0 });
  });

  test("does without a capture that is refused", () => {
    const { send, scroll } = open({
      capture: () => {
        throw new DOMException("no such pointer", "NotFoundError");
      },
    });
    send("pointerdown", 1, 300, 300);
    send("pointermove", 1, 300, 200);
    expect(scroll().top).toBeGreaterThan(580);
  });

  test("ends with a capture that is lost, a cancelled pointer, and a mouse that is pressed", () => {
    for (const ending of ["lostpointercapture", "pointercancel", "mouse"]) {
      const { send, press, scroll, frame, log } = open();
      send("pointerdown", 1, 300, 300, true);
      send("pointermove", 1, 300, 280);
      send("pointermove", 1, 300, 240);
      const { top } = scroll();
      if (ending === "mouse") {
        press("mouse");
      } else {
        send(ending, 1, 300, 240);
      }
      send("pointermove", 1, 300, 100);
      send("pointerup", 1, 300, 100);
      frame();
      expect(scroll().top, ending).toBe(top);
      expect(log).toEqual([]);
    }
  });
});

describe("a sheet that was let go", () => {
  test("goes on with the frames, slower and slower, to rest", () => {
    const { swipe, frame, scroll } = open();
    swipe(0, -16);
    let { top } = scroll();
    let last = Number.POSITIVE_INFINITY;
    let count = 0;
    while (frames.size > 0) {
      frame();
      const moved = scroll().top - top;
      expect(moved).toBeLessThanOrEqual(last);
      last = moved;
      ({ top } = scroll());
      count++;
    }
    expect(count).toBeGreaterThan(10);
    expect(scroll().left).toBe(200);
    expect(top).toBeGreaterThan(600);
  });

  test("ends with the sheet, along each axis by itself", () => {
    const { swipe, frame, scroll } = open();
    swipe(-24, -18);
    let count = 0;
    while (frames.size > 0 && count < 1_000) {
      frame();
      count++;
    }
    expect(scroll()).toEqual(LIMITS);
    expect(count).toBeLessThan(100);
  });

  test("is stopped by a pointer, the wheel, a key, the grid, and when it is gone", () => {
    for (const by of ["touch", "mouse", "wheel", "keydown", "grid", "destroy"]) {
      const { touch, swipe, press, host, frame, scroll } = open();
      swipe(0, -16);
      frame();
      if (by === "grid") {
        touch.stop();
      } else if (by === "destroy") {
        touch.destroy();
      } else if (by === "wheel" || by === "keydown") {
        host.dispatchEvent(new Event(by));
      } else {
        press(by);
      }
      const { top } = scroll();
      frame();
      frame();
      expect(scroll().top, by).toBe(top);
      expect(frames.size, by).toBe(0);
    }
  });

  test("selects nothing when a finger stops it", () => {
    const { swipe, frame, tap, log } = open();
    swipe(0, -16);
    frame();
    tap(100, 100);
    expect(log).toEqual([]);
    tap(100, 100);
    expect(log).toHaveLength(2);
  });

  test("stays where less motion is wanted", () => {
    wantsLessMotion = true;
    const { swipe } = open();
    swipe(0, -16);
    expect(frames.size).toBe(0);
  });

  test("is not listened for any more when the grid is gone", () => {
    const { touch, swipe, press, host } = open();
    touch.destroy();
    swipe(0, -16);
    press("mouse");
    host.dispatchEvent(new Event("wheel"));
    host.dispatchEvent(new Event("keydown"));
    expect(frames.size).toBe(1);
  });
});

describe("a tap", () => {
  test("selects a cell, a row, a column or all of it, and takes the focus", () => {
    const { tap, log } = open();
    tap(100, 45);
    tap(20, 45);
    tap(149, 10);
    tap(10, 10);
    expect(log).toEqual([
      "focus",
      "cell 26,2",
      "focus",
      "rows 26-26",
      "focus",
      "cols 2-2",
      "focus",
      "all",
    ]);
  });

  test("does nothing where there is nothing", () => {
    const { tap, log } = open();
    tap(1_200, 100);
    expect(log).toEqual([]);
  });

  test("does nothing else if it opens a filter, or if an editor has it do nothing", () => {
    const filter = open({ filter: true });
    filter.tap(100, 100);
    expect(filter.log).toEqual(["filter"]);

    const editor = open({ filter: true });
    editor.host.addEventListener("reticula-tap", (event) => event.preventDefault());
    editor.tap(100, 100);
    expect(editor.log).toEqual([]);
  });

  test("opens what the button in a header is for, even if there are no rows below it", () => {
    const button = open({ filter: true });
    button.tap(1_200, 10);
    expect(button.log).toEqual(["filter"]);
    button.tap(1_200, 100);
    expect(button.log).toEqual(["filter"]);

    const none = open();
    const tapped = vi.fn();
    none.host.addEventListener("reticula-tap", tapped);
    none.tap(1_200, 10);
    expect(none.log).toEqual([]);
    expect(tapped).not.toHaveBeenCalled();
  });

  test("is none of a finger that comes to one whose release was never heard of, but by itself", () => {
    const { send, log } = open();
    send("pointerdown", 1, 100, 100, true);
    send("pointerdown", 2, 200, 100, true);
    send("pointerup", 2, 200, 100);
    expect(log).toEqual(["focus", "cell 29,3"]);
  });
});

describe("a double tap", () => {
  const twice = ({ send }: ReturnType<typeof open>) => {
    for (let count = 0; count < 2; count++) {
      send("pointerdown", 1, 100, 100, true);
      send("pointerup", 1, 100, 100);
    }
    now += 400;
  };

  test("changes between 100% and the zoom there was before", () => {
    const one = open();
    const zooms = () => one.log.filter((line) => line.startsWith("zoom"));
    twice(one);
    twice(one);
    expect(zooms()).toEqual(["zoom 1.5 at 100,100", "zoom 1 at 100,100"]);
    // Chosen elsewhere, and looked at since.
    one.grid.zoom = 0.5;
    one.tap(100, 100);
    one.grid.zoom = 1;
    twice(one);
    expect(one.grid.zoom).toBe(0.5);
    // Chosen elsewhere and left there, without a finger on the grid meanwhile.
    one.grid.zoom = 0.75;
    one.press("touch");
    one.grid.zoom = 1;
    twice(one);
    expect(one.grid.zoom).toBe(0.75);
  });

  test("is two taps where fingers may not zoom", () => {
    const one = open({ zooms: false });
    twice(one);
    expect(one.grid.zoom).toBe(1);
    expect(one.log).toEqual(["focus", "cell 29,2"]);
  });
});

describe("a pinch", () => {
  const pinch = ({ send }: ReturnType<typeof open>) => {
    send("pointerdown", 1, 100, 100, true);
    send("pointerdown", 2, 200, 100);
    send("pointermove", 2, 324, 100);
    send("pointerup", 2, 324, 100);
    send("pointerup", 1, 100, 100);
  };

  test("zooms at the middle of the fingers, after the sheet has gone there", () => {
    const one = open();
    pinch(one);
    expect(one.log).toEqual(["begin", "pinch 2.00 at 212,100", "end"]);
    expect(one.scroll().left).toBe(200 - 62);
  });

  test("only scrolls where fingers may not zoom", () => {
    const one = open({ zooms: false });
    pinch(one);
    expect(one.log).toEqual([]);
    expect(one.scroll().left).toBe(200 - 62);
  });

  test("ends when a mouse is pressed", () => {
    const { send, press, log } = open();
    send("pointerdown", 1, 100, 100, true);
    send("pointerdown", 2, 200, 100);
    send("pointermove", 2, 324, 100);
    press("pen");
    send("pointermove", 2, 400, 100);
    expect(log.at(-1)).toBe("end");
  });
});
