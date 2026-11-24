import { describe, expect, test } from "vitest";
import { commonSize, mergeIntervals, sizeIn, withSizes } from "./sizes.ts";

const run = (first: number, end: number) => ({ first, end });

describe("intervals", () => {
  test("are each counted once, and in order", () => {
    expect(mergeIntervals([run(8, 10), run(0, 3), run(2, 5), run(5, 6), run(7, 7)])).toEqual([
      run(0, 6),
      run(8, 10),
    ]);
    expect(mergeIntervals([run(0, 9), run(2, 3)])).toEqual([run(0, 9)]);
    expect(mergeIntervals([])).toEqual([]);
  });
});

describe("sizes", () => {
  const SIZES = [
    [2, 40],
    [5, 30],
    [9, 30],
  ] as const;

  test("are given to whole runs, and the others keep theirs", () => {
    expect(withSizes(SIZES, [run(4, 6), run(1, 2)], 60, 24)).toEqual([
      [1, 60],
      [2, 40],
      [4, 60],
      [5, 60],
      [9, 30],
    ]);
    expect(sizeIn(withSizes(SIZES, [run(9, 10)], 60, 24), 9, 24)).toBe(60);
  });

  test("that are the standard one are none of their own", () => {
    expect(withSizes(SIZES, [run(0, 6)], 24, 24)).toEqual([[9, 30]]);
    expect(withSizes(SIZES, [], 24, 24)).toEqual(SIZES);
  });

  test("are told if they are the same", () => {
    expect(commonSize(SIZES, [run(2, 3)], 24)).toBe(40);
    expect(commonSize(SIZES, [run(5, 6), run(9, 10)], 24)).toBe(30);
    expect(commonSize(SIZES, [run(6, 9)], 24)).toBe(24);
    // A million rows are not looked at one by one.
    expect(commonSize(SIZES, [run(10, 1_000_000)], 24)).toBe(24);
    expect(commonSize(SIZES, [run(5, 7)], 24)).toBeNull();
    expect(commonSize(SIZES, [run(2, 3), run(5, 6)], 24)).toBeNull();
    expect(commonSize([[3, 24]], [run(3, 5)], 24)).toBe(24);
    expect(commonSize(SIZES, [], 24)).toBeNull();
  });
});
