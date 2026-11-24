import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { difference } from "./golden.ts";
import { decodePng, encodePng, type Picture } from "./png.ts";

function gradient(width: number, height: number): Picture {
  const rgba = new Uint8Array(width * height * 4);
  for (let pixel = 0; pixel < width * height; pixel++) {
    const x = pixel % width;
    rgba.set([(x * 5) & 0xff, pixel & 0xff, x > 4 ? 200 : 10, 255], pixel * 4);
  }
  return { width, height, rgba };
}

describe("the font", () => {
  it("is what its generator makes", () => {
    execFileSync(process.execPath, [
      fileURLToPath(new URL("font.mjs", import.meta.url)),
      "--check",
    ]);
  });
});

describe("a golden image", () => {
  it("holds the pixels that were drawn", () => {
    const picture = gradient(37, 11);
    expect(decodePng(encodePng(picture))).toEqual(picture);
  });

  it("is of an opaque picture", () => {
    const picture = gradient(2, 2);
    picture.rgba[7] = 254;
    expect(() => encodePng(picture)).toThrow("pixel 1 is not opaque");
  });

  it("is refused if it was written by something else", () => {
    const file = encodePng(gradient(2, 2));
    file[25] = 6;
    expect(() => decodePng(file)).toThrow("not an RGB image");
    expect(() => decodePng(Buffer.from("GIF89a..."))).toThrow("not a PNG");
  });

  it("differs from a picture by more than one level", () => {
    const golden = gradient(9, 4);
    const actual = gradient(9, 4);
    actual.rgba[4 * 12] = (golden.rgba[4 * 12] ?? 0) + 1;
    expect(difference(actual, golden)).toMatchObject({ over: 0, most: 1, first: null });
    actual.rgba[4 * 12] = (golden.rgba[4 * 12] ?? 0) + 2;
    expect(difference(actual, golden)).toMatchObject({ over: 1, most: 2, first: { x: 3, y: 1 } });
  });

  it("differs from a picture of another size", () => {
    expect(difference(gradient(3, 3), gradient(3, 4)).over).toBe(9);
  });
});
