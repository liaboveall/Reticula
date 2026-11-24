import { describe, expect, test } from "vitest";
import { addDashedOutline, addOutline, lineWidth, type PixelBox, RectList } from "./rects.ts";

const SCREEN: PixelBox = { left: 0, top: 0, right: 200, bottom: 120 };

/** How often each pixel of a screen is painted. */
function painted(rects: RectList, box: PixelBox = SCREEN): Map<string, number> {
  const pixels = new Map<string, number>();
  for (let index = 0; index < rects.length; index++) {
    const [left = 0, top = 0, width = 0, height = 0] = rects.geometry.subarray(index * 4);
    expect(left).toBeGreaterThanOrEqual(box.left);
    expect(top).toBeGreaterThanOrEqual(box.top);
    expect(left + width).toBeLessThanOrEqual(box.right);
    expect(top + height).toBeLessThanOrEqual(box.bottom);
    for (let y = top; y < top + height; y++) {
      for (let x = left; x < left + width; x++) {
        pixels.set(`${x},${y}`, (pixels.get(`${x},${y}`) ?? 0) + 1);
      }
    }
  }
  return pixels;
}

describe("a list of rectangles", () => {
  test("keeps what it has when it grows, and leaves out what is empty", () => {
    const rects = new RectList(1);
    for (let index = 0; index < 5; index++) {
      rects.push(index, index + 1, index, 3, index + 100);
    }
    expect(rects.length).toBe(4);
    expect([...rects.geometry.subarray(0, 8)]).toEqual([1, 2, 1, 3, 2, 3, 2, 3]);
    expect([...rects.colors.subarray(0, 4)]).toEqual([101, 102, 103, 104]);
  });

  test("cuts rectangles down to what is in view, however far they reach", () => {
    const rects = new RectList();
    rects.add(SCREEN, 10, -5_000_000_000, 20, 5_000_000_000, 1);
    rects.add(SCREEN, 300, 0, 400, 10, 2);
    expect(rects.length).toBe(1);
    expect([...rects.geometry.subarray(0, 4)]).toEqual([10, 0, 10, 120]);
  });
});

test("lines are whole device pixels and never vanish", () => {
  expect([0.25, 1, 1.25, 1.5, 2].map((scale) => lineWidth(1, scale))).toEqual([1, 1, 1, 2, 2]);
  expect([1, 1.25, 1.5, 2].map((scale) => lineWidth(2, scale))).toEqual([2, 3, 3, 4]);
});

describe("an outline", () => {
  test("is four rectangles that do not overlap", () => {
    const rects = new RectList();
    addOutline(rects, SCREEN, 10, 20, 60, 50, 3, 1);
    expect(rects.length).toBe(4);
    const pixels = painted(rects);
    expect(pixels.size).toBe(50 * 30 - 44 * 24);
    expect(Math.max(...pixels.values())).toBe(1);
  });

  test("is filled when there is no room inside it", () => {
    const rects = new RectList();
    addOutline(rects, SCREEN, 10, 20, 15, 50, 3, 1);
    expect(rects.length).toBe(1);
    expect(painted(rects).size).toBe(5 * 30);
  });

  test("shows the edges that are in view", () => {
    const rects = new RectList();
    addOutline(rects, SCREEN, 10, -24_000_000, 60, 24_000_000, 2, 1);
    expect(rects.length).toBe(2);
    expect([...rects.geometry.subarray(0, 8)]).toEqual([10, 0, 2, 120, 58, 0, 2, 120]);
  });
});

describe("a dashed outline", () => {
  const box = { left: 12, top: 9, right: 150, bottom: 101 };
  const dashed = (clip: PixelBox, x = 0, y = 0) => {
    const rects = new RectList();
    const { left, top, right, bottom } = box;
    addDashedOutline(rects, clip, left + x, top + y, right + x, bottom + y, 2, 4, 3, 1);
    return rects;
  };

  test("runs clockwise from the top left corner without a break in its rhythm", () => {
    const pixels = painted(dashed(SCREEN));
    expect(Math.max(...pixels.values())).toBe(1);
    const path: boolean[] = [];
    for (let x = 12; x < 148; x++) path.push(pixels.has(`${x},9`));
    for (let y = 9; y < 99; y++) path.push(pixels.has(`149,${y}`));
    for (let x = 149; x >= 14; x--) path.push(pixels.has(`${x},100`));
    for (let y = 100; y >= 11; y--) path.push(pixels.has(`12,${y}`));
    expect(path).toEqual(path.map((_, at) => at % 7 < 4));
  });

  test("paints the same pixels through any clip", () => {
    const whole = painted(dashed(SCREEN));
    for (const clip of [
      { left: 30, top: 0, right: 90, bottom: 120 },
      { left: 0, top: 40, right: 200, bottom: 101 },
      { left: 141, top: 93, right: 149, bottom: 101 },
      { left: 13, top: 10, right: 14, bottom: 11 },
    ]) {
      const inside = [...whole.keys()].filter((key) => {
        const [x = 0, y = 0] = key.split(",").map(Number);
        return x >= clip.left && x < clip.right && y >= clip.top && y < clip.bottom;
      });
      expect([...painted(dashed(clip), clip).keys()].sort()).toEqual(inside.sort());
    }
  });

  test("moves with what it is around instead of crawling along it", () => {
    const still = painted(dashed(SCREEN));
    const moved = painted(dashed(SCREEN, 1, -3));
    for (const key of still.keys()) {
      const [x = 0, y = 0] = key.split(",").map(Number);
      expect(moved.has(`${x + 1},${y - 3}`)).toBe(true);
    }
  });

  test("of a million rows is only the dashes in view, where they belong", () => {
    const height = 1_000_000 * 48;
    const rects = new RectList();
    addDashedOutline(rects, SCREEN, 20, -23_456_789, 100, height - 23_456_789, 2, 8, 6, 1);
    expect(rects.length).toBeLessThan(30);
    const pixels = painted(rects);
    // The right edge starts after the top one; the left one after all the others.
    const across = 78;
    const down = height - 2;
    for (let y = 0; y < 120; y++) {
      const right = (across + 23_456_789 + y) % 14 < 8;
      expect(pixels.has(`99,${y}`), `right edge at ${y}`).toBe(right);
      const left = (2 * across + down + (height - 23_456_789 - 1 - y)) % 14 < 8;
      expect(pixels.has(`20,${y}`), `left edge at ${y}`).toBe(left);
    }
  });

  test("of a box as thin as its line does not fall apart", () => {
    const rects = new RectList();
    addDashedOutline(rects, SCREEN, 10, 10, 60, 12, 2, 4, 3, 1);
    expect(Math.max(...painted(rects).values())).toBeLessThanOrEqual(2);
  });
});
