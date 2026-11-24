import type { Grid } from "@reticula/grid";
import { afterEach, expect, test, vi } from "vitest";
import { rendererPreference, setRendererPreference } from "./renderer.ts";

afterEach(() => {
  vi.unstubAllGlobals();
});

function visit(search: string, kept = new Map<string, string>()): Map<string, string> {
  vi.stubGlobal("window", { location: { search } });
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => kept.get(key) ?? null,
    setItem: (key: string, value: string) => kept.set(key, value),
  });
  return kept;
}

test("the address says what grids are drawn with, or else the user, or else the browser", () => {
  visit("");
  expect(rendererPreference()).toBe("auto");
  visit("?hooks&renderer=webgpu");
  expect(rendererPreference()).toBe("webgpu");
  visit("?renderer=svg");
  expect(rendererPreference()).toBe("auto");
});

test("a choice is followed at once, and kept", () => {
  const kept = visit("");
  const grid = { setRenderer: vi.fn() };
  setRendererPreference(grid as unknown as Grid, "canvas2d");
  expect(grid.setRenderer).toHaveBeenCalledWith("canvas2d");
  visit("", kept);
  expect(rendererPreference()).toBe("canvas2d");
  visit("?renderer=webgpu", kept);
  expect(rendererPreference()).toBe("webgpu");
  setRendererPreference(null, "auto");
  visit("", kept);
  expect(rendererPreference()).toBe("auto");
});

test("without storage a choice holds for the visit", () => {
  visit("");
  vi.stubGlobal("localStorage", undefined);
  const grid = { setRenderer: vi.fn() };
  setRendererPreference(grid as unknown as Grid, "canvas2d");
  expect(grid.setRenderer).toHaveBeenCalledWith("canvas2d");
  expect(rendererPreference()).toBe("auto");
});
