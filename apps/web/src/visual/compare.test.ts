import type { FrameMasks, Pixels } from "@reticula/grid/internals";
import { describe, expect, it } from "vitest";
import { changedPixels, comparePictures, withPatchOf } from "./compare.ts";

const WIDTH = 40;
const HEIGHT = 10;

/** A grey picture, with some pixels given another level. */
function picture(levels: Readonly<Record<number, number>> = {}): Pixels {
  const rgba = new Uint8ClampedArray(WIDTH * HEIGHT * 4).fill(128);
  for (const [pixel, level] of Object.entries(levels)) {
    rgba.set([level, level, level, 128], Number(pixel) * 4);
  }
  return { width: WIDTH, height: HEIGHT, rgba };
}

function masks(text: readonly number[] = [], translucent: Record<number, number> = {}): FrameMasks {
  const result = {
    text: new Uint8Array(WIDTH * HEIGHT),
    translucent: new Uint8Array(WIDTH * HEIGHT),
  };
  for (const pixel of text) {
    result.text[pixel] = 1;
  }
  for (const [pixel, layers] of Object.entries(translucent)) {
    result.translucent[Number(pixel)] = layers;
  }
  return result;
}

const NO_INK = new Uint8Array(WIDTH * HEIGHT);
const INK = new Uint8Array(WIDTH * HEIGHT).fill(1);
const ALL = Array.from({ length: WIDTH * HEIGHT }, (_, pixel) => pixel);

describe("comparePictures", () => {
  it("accepts a picture that is the same", () => {
    const result = comparePictures(picture(), picture(), masks(), NO_INK, false);
    expect(result).toMatchObject({ ok: true, differing: 0, worst: null });
  });

  it("allows one level outside text", () => {
    expect(comparePictures(picture(), picture({ 5: 129 }), masks(), NO_INK, false).ok).toBe(true);
    const result = comparePictures(picture(), picture({ 45: 130 }), masks(), NO_INK, false);
    expect(result.ok).toBe(false);
    expect(result.outside).toEqual({ over: 1, most: 2 });
    expect(result.worst).toMatchObject({ x: 5, y: 1, difference: 2, allowed: 1, inText: false });
  });

  it("allows a level for each translucent layer, up to five", () => {
    const compared = (layers: number, difference: number) =>
      comparePictures(
        picture(),
        picture({ 7: 128 + difference }),
        masks([], { 7: layers }),
        NO_INK,
        false,
      ).ok;
    expect(compared(1, 1)).toBe(true);
    expect(compared(1, 2)).toBe(false);
    expect(compared(3, 3)).toBe(true);
    expect(compared(3, 4)).toBe(false);
    expect(compared(9, 5)).toBe(true);
    expect(compared(9, 6)).toBe(false);
  });

  it("allows four levels in text, however many layers are over it", () => {
    const compared = (difference: number) =>
      comparePictures(picture(), picture({ 7: 128 + difference }), masks([7], { 7: 5 }), INK, false)
        .ok;
    expect(compared(4)).toBe(true);
    expect(compared(5)).toBe(false);
  });

  it("allows small text to be far off in half a percent of its ink", () => {
    const far = (pixels: number, level: number) =>
      comparePictures(
        picture(),
        picture(Object.fromEntries(ALL.slice(0, pixels).map((pixel) => [pixel, level]))),
        masks(ALL),
        INK,
        true,
      );
    // Two of 400 are exactly half a percent.
    expect(far(2, 168).ok).toBe(true);
    expect(far(3, 168).ok).toBe(false);
    expect(far(400, 144).ok).toBe(true);
    expect(far(1, 224).ok).toBe(true);
    expect(far(1, 225).ok).toBe(false);
  });

  it("counts only the ink of small text", () => {
    const ink = new Uint8Array(WIDTH * HEIGHT);
    ink.fill(1, 0, 100);
    const right = picture({ 0: 168 });
    expect(comparePictures(picture(), right, masks(ALL), INK, true).ok).toBe(true);
    const result = comparePictures(picture(), right, masks(ALL), ink, true);
    expect(result.ok).toBe(false);
    expect(result.inkedPixels).toBe(100);
  });

  it("points at small text when there is too much of it off", () => {
    const right = picture({ 0: 129, 20: 160, 41: 168, 60: 160 });
    const result = comparePictures(picture(), right, masks(ALL.slice(20)), INK, true);
    expect(result.ok).toBe(false);
    expect(result.worst).toMatchObject({ x: 1, y: 1, difference: 40, inText: true });
  });

  it("notices transparency", () => {
    const right = picture();
    right.rgba[3] = 0;
    expect(comparePictures(picture(), right, masks(), NO_INK, false).ok).toBe(false);
  });

  it("refuses pictures and masks of different sizes", () => {
    const small = { width: 1, height: 1, rgba: new Uint8ClampedArray(4) };
    expect(comparePictures(picture(), small, masks(), NO_INK, false).ok).toBe(false);
    expect(comparePictures(picture(), picture(), masks(), new Uint8Array(1), false).ok).toBe(false);
  });
});

describe("withPatchOf", () => {
  it("moves one patch and leaves the others", () => {
    const levels = { 0: 1, 1: 1, 10: 1, 11: 1, 12: 1, 20: 1, 21: 1, 22: 1, 23: 1 };
    const patched = withPatchOf(picture(), picture(levels));
    const changed = changedPixels(patched, picture(levels));
    expect(ALL.filter((pixel) => changed[pixel] === 1)).toEqual([10, 11, 12]);
  });

  it("says when there is nothing to choose from", () => {
    expect(() => withPatchOf(picture(), picture())).toThrow("0 patches");
  });
});
