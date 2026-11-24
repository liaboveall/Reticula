import { describe, expect, test } from "vitest";
import { type NavigationTarget, navigate } from "./navigation.ts";

function fakeGrid(active = { row: 4, col: 2 }, focus = active) {
  const calls: unknown[] = [];
  const called =
    (name: string) =>
    (...args: unknown[]) => {
      calls.push([name, ...args]);
    };
  const grid: NavigationTarget = {
    selection: active,
    selectionFocus: focus,
    sheetSize: { rows: 100, cols: 20 },
    selectAll: called("selectAll"),
    moveSelection: called("move"),
    extendSelection: called("extend"),
    pageVertical: called("page"),
    jumpTo: called("jump"),
  };
  return { grid, calls };
}

function press(keys: string, target = fakeGrid(), jumps = {}) {
  const parts = keys.split("+");
  const event = {
    key: parts.at(-1) ?? "",
    shiftKey: parts.includes("Shift"),
    ctrlKey: parts.includes("Control"),
    metaKey: parts.includes("Meta"),
    altKey: parts.includes("Alt"),
  };
  return { handled: navigate(target.grid, event, jumps), calls: target.calls };
}

describe("a key", () => {
  test.each([
    ["ArrowUp", ["move", -1, 0]],
    ["ArrowDown", ["move", 1, 0]],
    ["ArrowLeft", ["move", 0, -1]],
    ["ArrowRight", ["move", 0, 1]],
    ["Shift+ArrowRight", ["extend", 0, 1]],
    ["PageDown", ["page", 1, false]],
    ["Shift+PageUp", ["page", -1, true]],
    ["Home", ["jump", 4, 0, false]],
    ["End", ["jump", 4, 19, false]],
    ["Control+Home", ["jump", 0, 0, false]],
    ["Control+End", ["jump", 99, 19, false]],
    ["Control+ArrowDown", ["jump", 99, 2, false]],
    ["Control+ArrowRight", ["jump", 4, 19, false]],
    ["Meta+ArrowUp", ["jump", 0, 2, false]],
    ["Control+a", ["selectAll"]],
  ] as const)("%s moves the selection", (keys, call) => {
    expect(press(keys)).toEqual({ handled: true, calls: [call] });
  });

  test.each([
    "Alt+ArrowDown",
    "Control+Alt+ArrowDown",
    "Alt+PageDown",
    "Control+Shift+a",
    "Tab",
    "Enter",
    " ",
    "F6",
    "a",
  ])("%s is for others", (keys) => {
    expect(press(keys)).toEqual({ handled: false, calls: [] });
  });

  test("with Shift goes on from where the selection ends", () => {
    const target = () => fakeGrid({ row: 4, col: 2 }, { row: 9, col: 5 });
    expect(press("Shift+Home", target()).calls).toEqual([["jump", 9, 0, true]]);
    expect(press("Control+Shift+ArrowDown", target()).calls).toEqual([["jump", 99, 5, true]]);
    expect(press("Control+ArrowDown", target()).calls).toEqual([["jump", 99, 2, false]]);
  });

  test("goes where the data end, if somebody knows", () => {
    const jumps: unknown[] = [];
    const toEdge = (...args: unknown[]) => jumps.push(args);
    const toUsedEnd = (...args: unknown[]) => jumps.push(args);
    expect(press("Control+ArrowLeft", undefined, { toEdge, toUsedEnd }).calls).toEqual([]);
    press("Control+Shift+End", undefined, { toEdge, toUsedEnd });
    expect(jumps).toEqual([["left", false], [true]]);
  });
});

test("without a selection a key moves nothing", () => {
  const { grid, calls } = fakeGrid();
  const empty = { ...grid, selection: null, selectionFocus: null };
  const key = (key: string, ctrlKey = false) =>
    navigate(empty, { key, ctrlKey, shiftKey: false, metaKey: false, altKey: false });
  expect(key("Home")).toBe(false);
  expect(key("ArrowDown", true)).toBe(true);
  expect(calls).toEqual([]);
});
