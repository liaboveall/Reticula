import { describe, expect, test } from "vitest";
import { alphaOf, Colors, cssColor, packColor, parseSerialized } from "./color.ts";

describe("colours as a 2D context writes them", () => {
  test("are packed with red in the lowest byte and alpha not multiplied in", () => {
    expect(parseSerialized("#102030")).toBe(0xff302010);
    expect(parseSerialized("rgba(16, 32, 48, 0.5)")).toBe(0x80302010);
    expect(parseSerialized("rgba(0, 0, 0, 0)")).toBe(0);
    expect(parseSerialized("rgb(255 0 0)")).toBe(0xff0000ff);
    expect(parseSerialized("color(srgb 1 0.5 0 / 0.2)")).toBe(packColor(255, 128, 0, 51));
    expect(parseSerialized("color(srgb 1.2 -0.1 0)")).toBe(packColor(255, 0, 0));
  });

  test("are not guessed at", () => {
    expect(parseSerialized("oklch(0.5 0.1 200)")).toBeNull();
    expect(parseSerialized("color(display-p3 1 0 0)")).toBeNull();
    expect(parseSerialized("rebeccapurple")).toBeNull();
  });

  test("are written back as CSS of the same bytes", () => {
    expect(cssColor(packColor(1, 2, 255))).toBe("#0102ff");
    expect(cssColor(packColor(1, 2, 255, 16))).toBe("#0102ff10");
    expect(alphaOf(packColor(1, 2, 255, 16))).toBe(16);
  });

  test("are serialised once, through sRGB when they are outside it", () => {
    const asked: string[] = [];
    const colors = new Colors((css) => {
      asked.push(css);
      return css.startsWith("color-mix") ? "color(srgb 0 0 1)" : css;
    });
    expect(colors.parse("oklch(0.45 0.31 264)")).toBe(packColor(0, 0, 255));
    expect(colors.parse("oklch(0.45 0.31 264)")).toBe(packColor(0, 0, 255));
    expect(asked).toEqual([
      "oklch(0.45 0.31 264)",
      "color-mix(in srgb, oklch(0.45 0.31 264), oklch(0.45 0.31 264))",
    ]);
  });
});
