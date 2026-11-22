import { expect, test } from "vitest";
import { FILTER_BUTTON_SIZE, FilterRowMap, filterButtonBox } from "./filter.ts";

test("FilterRowMap maps 1M rows with huge hidden gaps in O(log H)", () => {
  const map = new FilterRowMap(1_000_000, [
    [1, 499],
    [500, 999_995],
  ]);
  expect(map.totalRows).toBe(1_000_000);
  expect(map.hasHiddenRows).toBe(true);
  expect(map.visibleRows).toBe(5);

  expect(map.displayToPhysical(0)).toBe(0);
  expect(map.displayToPhysical(1)).toBe(999_996);
  expect(map.displayToPhysical(4)).toBe(999_999);

  expect(map.physicalToDisplay(0)).toBe(0);
  expect(map.physicalToDisplay(1)).toBeNull();
  expect(map.physicalToDisplay(999_995)).toBeNull();
  expect(map.physicalToDisplay(999_996)).toBe(1);
  expect(map.physicalToDisplay(999_999)).toBe(4);

  expect(map.isGapBefore(0)).toBe(false);
  expect(map.isGapAfter(0)).toBe(true);
  expect(map.isGapBefore(1)).toBe(true);
  expect(map.isGapAfter(1)).toBe(false);
});

test("filterButtonBox centers inside wide cells and returns null when too narrow", () => {
  expect(filterButtonBox(10, 20, 80, 24)).toEqual({
    x: 10 + 80 - FILTER_BUTTON_SIZE - 3,
    y: 20 + Math.floor((24 - FILTER_BUTTON_SIZE) / 2),
    size: FILTER_BUTTON_SIZE,
  });
  expect(filterButtonBox(10, 20, 14, 24)).toBeNull();
});
