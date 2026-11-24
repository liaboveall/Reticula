import type { FrameMasks, Pixels } from "@reticula/grid/internals";

/** Below this font size, in device pixels, glyphs are too small to compare level by level. */
export const SMALL_TEXT = 8;

// How far apart two renderers may be, in levels of a channel.
const PLAIN = 1;
const MOST_TRANSLUCENT = 5;
const TEXT = 4;
const SMALL_TEXT_FAR = 16;
const SMALL_TEXT_FAR_SHARE = 0.005;
const SMALL_TEXT_MOST = 96;

type Rgba = readonly [number, number, number, number];

export interface WorstPixel {
  readonly x: number;
  readonly y: number;
  readonly left: Rgba;
  readonly right: Rgba;
  readonly difference: number;
  /** The difference that is allowed there. */
  readonly allowed: number;
  readonly inText: boolean;
}

export interface Comparison {
  readonly ok: boolean;
  /** What is wrong, in words. */
  readonly failures: readonly string[];
  readonly width: number;
  readonly height: number;
  /** Pixels a text run may draw on, and of those, the ones text did draw on. */
  readonly textPixels: number;
  readonly inkedPixels: number;
  readonly translucentPixels: number;
  /** Pixels that differ at all. */
  readonly differing: number;
  readonly outside: { readonly over: number; readonly most: number };
  readonly inside: {
    readonly over: number;
    readonly most: number;
    /** Share of the inked pixels that are more than 16 levels apart. */
    readonly farShare: number;
  };
  /** The pixel furthest beyond what is allowed, or else the one furthest apart. */
  readonly worst: WorstPixel | null;
}

function pixelAt(pixels: Pixels, pixel: number): Rgba {
  const { rgba } = pixels;
  const at = pixel * 4;
  return [rgba[at] ?? 0, rgba[at + 1] ?? 0, rgba[at + 2] ?? 0, rgba[at + 3] ?? 0];
}

/** The largest difference of a channel between two pixels. */
function apart(left: Uint8ClampedArray, right: Uint8ClampedArray, pixel: number): number {
  let most = 0;
  for (let at = pixel * 4; at < pixel * 4 + 4; at++) {
    most = Math.max(most, Math.abs((left[at] ?? 0) - (right[at] ?? 0)));
  }
  return most;
}

/** 1 for every pixel that two pictures of the same size do not agree on. */
export function changedPixels(a: Pixels, b: Pixels): Uint8Array {
  const changed = new Uint8Array(a.width * a.height);
  for (let pixel = 0; pixel < changed.length; pixel++) {
    changed[pixel] = apart(a.rgba, b.rgba, pixel) > 0 ? 1 : 0;
  }
  return changed;
}

/**
 * Whether two renderers drew the same frame.
 *
 * Blending rounds once for every translucent layer, so as many levels may be
 * off there. Text is anti-aliased by the two in different places: it may be
 * off by a little more, and very small text by more than that in a few pixels.
 *
 * `ink` has a 1 for every pixel that drawing the frame without its text changes.
 */
export function comparePictures(
  left: Pixels,
  right: Pixels,
  masks: FrameMasks,
  ink: Uint8Array,
  smallText: boolean,
): Comparison {
  const { width, height } = left;
  const failures: string[] = [];
  const count = width * height;
  if (right.width !== width || right.height !== height) {
    failures.push(`the pictures are ${width}x${height} and ${right.width}x${right.height}`);
  }
  if (masks.text.length !== count || masks.translucent.length !== count || ink.length !== count) {
    failures.push("the masks are not the size of the picture");
  }

  let textPixels = 0;
  let inkedPixels = 0;
  let translucentPixels = 0;
  let differing = 0;
  let far = 0;
  const outside = { over: 0, most: 0 };
  const inside = { over: 0, most: 0 };
  let worst: WorstPixel | null = null;
  let farthest: WorstPixel | null = null;

  for (let pixel = 0; failures.length === 0 && pixel < count; pixel++) {
    const inText = masks.text[pixel] === 1;
    const layers = masks.translucent[pixel] ?? 0;
    textPixels += inText ? 1 : 0;
    translucentPixels += layers > 0 ? 1 : 0;
    if (inText && ink[pixel] === 1) {
      inkedPixels++;
    }
    const difference = apart(left.rgba, right.rgba, pixel);
    if (difference === 0) {
      continue;
    }
    differing++;
    const allowed = inText
      ? smallText
        ? SMALL_TEXT_MOST
        : TEXT
      : Math.max(PLAIN, Math.min(MOST_TRANSLUCENT, layers));
    const area = inText ? inside : outside;
    area.most = Math.max(area.most, difference);
    if (difference > allowed) {
      area.over++;
    }
    const isFar = inText && difference > SMALL_TEXT_FAR;
    far += isFar ? 1 : 0;
    const found = (): WorstPixel => ({
      x: pixel % width,
      y: Math.floor(pixel / width),
      left: pixelAt(left, pixel),
      right: pixelAt(right, pixel),
      difference,
      allowed,
      inText,
    });
    if (isFar && difference > (farthest?.difference ?? 0)) {
      farthest = found();
    }
    if (
      !worst ||
      difference - allowed > worst.difference - worst.allowed ||
      (difference - allowed === worst.difference - worst.allowed && difference > worst.difference)
    ) {
      worst = found();
    }
  }

  // Pixels only the other picture has ink on count too, hence the floor.
  const farShare = far / Math.max(1, inkedPixels);
  if (outside.over > 0) {
    failures.push(`${outside.over} pixels outside text are off, by up to ${outside.most}`);
  }
  if (inside.over > 0) {
    failures.push(`${inside.over} pixels of text are off, by up to ${inside.most}`);
  }
  if (smallText && farShare > SMALL_TEXT_FAR_SHARE) {
    // Each of these pixels may be as far off as it is, but not all of them.
    if (outside.over === 0 && inside.over === 0) {
      worst = farthest;
    }
    failures.push(`${(farShare * 100).toFixed(2)}% of the small text is off by more than 16`);
  }
  return {
    ok: failures.length === 0,
    failures,
    width,
    height,
    textPixels,
    inkedPixels,
    translucentPixels,
    differing,
    outside,
    inside: { ...inside, farShare },
    worst,
  };
}

/**
 * Puts one patch of `from` into a copy of `onto`: the pixels that differ
 * between the two fall into patches that do not touch, such as the dashes of
 * an outline, and the patch of middling size is taken.
 */
export function withPatchOf(from: Pixels, onto: Pixels): Pixels {
  const { width, height } = onto;
  const changed = changedPixels(from, onto);
  const patches: number[][] = [];
  for (let start = 0; start < changed.length; start++) {
    if (changed[start] !== 1) {
      continue;
    }
    const patch: number[] = [];
    const open = [start];
    changed[start] = 0;
    for (let pixel = open.pop(); pixel !== undefined; pixel = open.pop()) {
      patch.push(pixel);
      const x = pixel % width;
      const y = Math.floor(pixel / width);
      for (const [nx, ny] of [
        [x - 1, y],
        [x + 1, y],
        [x, y - 1],
        [x, y + 1],
      ] as const) {
        const next = ny * width + nx;
        if (nx >= 0 && nx < width && ny >= 0 && ny < height && changed[next] === 1) {
          changed[next] = 0;
          open.push(next);
        }
      }
    }
    patches.push(patch);
  }
  if (patches.length < 2) {
    throw new Error(`there are ${patches.length} patches to choose from`);
  }
  patches.sort((a, b) => a.length - b.length);
  const rgba = new Uint8ClampedArray(onto.rgba);
  for (const pixel of patches[patches.length >> 1] ?? []) {
    rgba.set(pixelAt(from, pixel), pixel * 4);
  }
  return { width, height, rgba };
}
