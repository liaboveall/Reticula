import { describe, expect, it } from "vitest";
import { mayPrefetch } from "./preload.ts";

describe("mayPrefetch", () => {
  it("wants a fine pointer and no wish to save data", () => {
    expect(mayPrefetch({ finePointer: true, saveData: false })).toBe(true);
    expect(mayPrefetch({ finePointer: false, saveData: false })).toBe(false);
    expect(mayPrefetch({ finePointer: true, saveData: true })).toBe(false);
  });
});
