// The golden images. They are made and compared in one environment only, the
// runner of the CI job: another system draws the same text a little differently.

import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { decodePng, encodePng, type Picture } from "./png.ts";

/** Whether this is the environment the golden images belong to. */
export const GOLDEN = process.env.RETICULA_GOLDEN === "1";
export const UPDATE = process.env.RETICULA_GOLDEN_UPDATE === "1";

const DIRECTORY = fileURLToPath(new URL("golden", import.meta.url));
/** Where images go that are to become golden images. The CI job keeps them. */
const CANDIDATES = fileURLToPath(new URL("../../test-results/visual-golden", import.meta.url));

// The same build of the same browser on the same system: what is left is how
// a processor rounds.
const ALLOWED = 1;

/** The names of the golden images there are. */
export function goldenNames(): string[] {
  return existsSync(DIRECTORY)
    ? readdirSync(DIRECTORY)
        .filter((file) => file.endsWith(".png"))
        .map((file) => file.slice(0, -".png".length))
    : [];
}

export function readGolden(name: string): Picture | null {
  const path = join(DIRECTORY, `${name}.png`);
  return existsSync(path) ? decodePng(readFileSync(path)) : null;
}

/** Writes an image to become a golden one, and returns where it is. */
export function writeGolden(name: string, picture: Picture): string {
  const directory = UPDATE ? DIRECTORY : CANDIDATES;
  mkdirSync(directory, { recursive: true });
  const path = join(directory, `${name}.png`);
  writeFileSync(path, encodePng(picture));
  return path;
}

export interface Difference {
  /** Pixels that are further apart than is allowed. */
  readonly over: number;
  readonly most: number;
  /** Where the first of them is. */
  readonly first: { readonly x: number; readonly y: number } | null;
  /** The differences, enhanced; `null` for images of different sizes. */
  readonly picture: Picture | null;
}

export function difference(actual: Picture, golden: Picture): Difference {
  if (actual.width !== golden.width || actual.height !== golden.height) {
    const over = actual.width * actual.height;
    return { over, most: 255, first: null, picture: null };
  }
  const { width, height } = actual;
  const rgba = new Uint8Array(width * height * 4).fill(255);
  let over = 0;
  let most = 0;
  let first: Difference["first"] = null;
  for (let pixel = 0; pixel < width * height; pixel++) {
    let worst = 0;
    for (let channel = 0; channel < 4; channel++) {
      const at = pixel * 4 + channel;
      const apart = Math.abs((actual.rgba[at] ?? 0) - (golden.rgba[at] ?? 0));
      worst = Math.max(worst, apart);
      if (channel < 3) {
        rgba[at] = Math.min(255, apart * 32);
      }
    }
    most = Math.max(most, worst);
    if (worst > ALLOWED) {
      over++;
      first ??= { x: pixel % width, y: Math.floor(pixel / width) };
    }
  }
  return { over, most, first, picture: { width, height, rgba } };
}
