import { describe, expect, it } from "vitest";
import { rovingTarget, TYPE_AHEAD_MS, TypeAhead } from "./roving.ts";

describe("a list", () => {
  it("is walked up and down, and ends at its ends", () => {
    expect(rovingTarget("ArrowDown", 0, 3)).toBe(1);
    expect(rovingTarget("ArrowDown", 2, 3)).toBe(2);
    expect(rovingTarget("ArrowUp", 0, 3)).toBe(0);
    expect(rovingTarget("Home", 2, 3)).toBe(0);
    expect(rovingTarget("End", 0, 3)).toBe(2);
  });

  it("leaves the other keys alone", () => {
    for (const key of ["ArrowLeft", "ArrowRight", "Enter", "a", "PageDown"]) {
      expect(rovingTarget(key, 1, 3), key).toBeNull();
    }
    expect(rovingTarget("ArrowDown", 0, 0)).toBeNull();
  });

  it("goes around where that is wanted", () => {
    expect(rovingTarget("ArrowDown", 2, 3, { wrap: true })).toBe(0);
    expect(rovingTarget("ArrowUp", 0, 3, { wrap: true })).toBe(2);
  });
});

describe("a row of tabs", () => {
  const tabs = { orientation: "horizontal", wrap: true } as const;

  it("is walked left and right, around its ends", () => {
    expect(rovingTarget("ArrowRight", 1, 2, tabs)).toBe(0);
    expect(rovingTarget("ArrowLeft", 0, 2, tabs)).toBe(1);
    expect(rovingTarget("ArrowDown", 0, 2, tabs)).toBeNull();
  });
});

describe("a palette", () => {
  // 0 1 2 3
  // 4 5 6 7
  // 8 9
  const palette = { columns: 4 };

  it("is walked in both directions", () => {
    expect(rovingTarget("ArrowDown", 1, 10, palette)).toBe(5);
    expect(rovingTarget("ArrowUp", 5, 10, palette)).toBe(1);
    expect(rovingTarget("ArrowRight", 3, 10, palette)).toBe(4);
    expect(rovingTarget("ArrowLeft", 4, 10, palette)).toBe(3);
  });

  it("has nothing below its last row and above its first", () => {
    expect(rovingTarget("ArrowDown", 6, 10, palette)).toBe(6);
    expect(rovingTarget("ArrowUp", 2, 10, palette)).toBe(2);
    expect(rovingTarget("ArrowRight", 9, 10, palette)).toBe(9);
  });
});

describe("typing ahead", () => {
  const labels = ["General", "Percent (0%)", "Plain Text (@)", "Date", "DateTime"];

  function typeAhead() {
    const clock = { now: 1_000 };
    return { clock, typed: new TypeAhead(() => clock.now) };
  }

  it("finds the next label with the letter, also when it comes before", () => {
    const { typed } = typeAhead();
    expect(typed.find("p", labels, 0)).toBe(1);
    expect(typeAhead().typed.find("g", labels, 3)).toBe(0);
    expect(typeAhead().typed.find("x", labels, 3)).toBeNull();
  });

  it("goes through the labels when the letter is repeated", () => {
    const { typed } = typeAhead();
    expect(typed.find("d", labels, 0)).toBe(3);
    expect(typed.find("d", labels, 3)).toBe(4);
    expect(typed.find("d", labels, 4)).toBe(3);
  });

  it("puts letters together, and may stay where it is", () => {
    const { typed } = typeAhead();
    expect(typed.find("P", labels, 0)).toBe(1);
    expect(typed.active).toBe(true);
    expect(typed.find("l", labels, 1)).toBe(2);
    expect(typed.find("a", labels, 2)).toBe(2);
  });

  it("begins anew after a pause", () => {
    const { clock, typed } = typeAhead();
    typed.find("p", labels, 0);
    clock.now += TYPE_AHEAD_MS;
    expect(typed.active).toBe(false);
    expect(typed.find("g", labels, 1)).toBe(0);
  });
});
