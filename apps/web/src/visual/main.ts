// The page the visual regression tests draw on. Pictures are drawn and compared
// here, pixel by pixel: a screenshot would add the compositor to what is
// compared, and a software adapter cannot present a WebGPU canvas at all.

import "@reticula/grid/grid.css";
import {
  drawCanvas2d,
  masksOf,
  type Pixels,
  type RendererKind,
  readTheme,
  type Theme,
} from "@reticula/grid/internals";
import { type BuiltCase, buildCase } from "./cases.ts";
import {
  type Comparison,
  changedPixels,
  comparePictures,
  SMALL_TEXT,
  withPatchOf,
} from "./compare.ts";
import type { AdapterReport, CaseOptions, CompareOptions, Report, VisualHooks } from "./hooks.ts";
import { mutated, withoutDashes, withoutStrikes, withoutText } from "./mutations.ts";
import { CASE_NAMES, CASES, type CaseName, type Mutation } from "./names.ts";
import { drawWebGpu, gpuDevice } from "./webgpu.ts";

const TEST_FONT = "Reticula Test";
/** Around the worst pixel, in pixels, and how much they are enlarged. */
const CROP = { width: 120, height: 80, zoom: 4 };

function loadTheme(font: "test" | "product", dark: boolean): Theme {
  const host = document.getElementById("host");
  if (!host) {
    throw new Error("visual.html has no #host element");
  }
  host.classList.add("reticula-grid");
  host.style.fontFamily = "";
  if (font === "test") {
    if (!testFontLoaded) {
      throw new Error("the test font has not been loaded");
    }
    host.style.fontFamily = `"${TEST_FONT}", sans-serif`;
  }
  const theme = readTheme(host);
  if (theme.dark !== dark) {
    throw new Error(`the page is emulating the ${theme.dark ? "dark" : "light"} theme`);
  }
  return theme;
}

let testFontLoaded = false;

function build(name: CaseName, options: CaseOptions) {
  const { scale = CASES[name].scale, dark = CASES[name].dark, font = "test" } = options;
  const built = buildCase(name, loadTheme(font, dark), scale);
  return { built, scale, dark, font };
}

async function draw(frame: BuiltCase["frame"], renderer: RendererKind): Promise<Pixels> {
  if (renderer === "canvas2d") {
    return drawCanvas2d(frame);
  }
  if (!drawWebGpu) {
    throw new Error("there is no WebGPU renderer yet");
  }
  return drawWebGpu(frame, await gpuDevice());
}

async function drawMutated(
  built: BuiltCase,
  renderer: RendererKind,
  mutation: Mutation | undefined,
): Promise<Pixels> {
  if (!mutation) {
    return draw(built.frame, renderer);
  }
  if (mutation === "dash") {
    return withPatchOf(
      await draw(withoutDashes(built.frame), renderer),
      await draw(built.frame, renderer),
    );
  }
  return draw(mutated(built, mutation), renderer);
}

async function compare(name: CaseName, options: CompareOptions = {}): Promise<Report> {
  const started = performance.now();
  const { built, scale, dark, font } = build(name, options);
  const { frame } = built;
  const { left = "canvas2d", right = "canvas2d", mutation } = options;
  const leftPixels = await draw(frame, left);
  const rightPixels = await drawMutated(built, right, mutation);
  const masks = masksOf(frame);
  const ink = changedPixels(drawCanvas2d(withoutStrikes(frame)), drawCanvas2d(withoutText(built)));
  let strayInk = 0;
  let firstStray: Report["firstStray"] = null;
  for (let pixel = 0; pixel < ink.length; pixel++) {
    if (ink[pixel] === 1 && masks.text[pixel] !== 1) {
      strayInk++;
      firstStray ??= { x: pixel % leftPixels.width, y: Math.floor(pixel / leftPixels.width) };
    }
  }
  const smallText = frame.theme.fontSize * scale * (frame.zoom ?? 1) < SMALL_TEXT;
  const comparison = comparePictures(leftPixels, rightPixels, masks, ink, smallText);
  return {
    ...comparison,
    name,
    scale,
    dark,
    font,
    left,
    right,
    strayInk,
    firstStray,
    diff: comparison.ok ? null : await croppedDiff(leftPixels, rightPixels, comparison),
    milliseconds: performance.now() - started,
  };
}

/** The surroundings of the worst pixel: left, right, and how far apart they are. */
async function croppedDiff(
  left: Pixels,
  right: Pixels,
  { worst }: Comparison,
): Promise<string | null> {
  if (!worst || left.width !== right.width || left.height !== right.height) {
    return null;
  }
  const width = Math.min(CROP.width, left.width);
  const height = Math.min(CROP.height, left.height);
  const x = Math.max(0, Math.min(worst.x - (width >> 1), left.width - width));
  const y = Math.max(0, Math.min(worst.y - (height >> 1), left.height - height));
  const difference = new Uint8ClampedArray(left.rgba.length);
  for (let at = 0; at < difference.length; at++) {
    // Differences of a few levels would not be seen as they are.
    difference[at] =
      at % 4 === 3 ? 255 : 32 * Math.abs((left.rgba[at] ?? 0) - (right.rgba[at] ?? 0));
  }
  const canvas = document.createElement("canvas");
  canvas.width = 3 * (width * CROP.zoom + CROP.zoom) - CROP.zoom;
  canvas.height = height * CROP.zoom;
  const context = canvas.getContext("2d");
  if (!context) {
    return null;
  }
  context.imageSmoothingEnabled = false;
  const panels = [left.rgba, right.rgba, difference];
  for (const [index, rgba] of panels.entries()) {
    const image = new ImageData(new Uint8ClampedArray(rgba), left.width, left.height);
    context.drawImage(
      await createImageBitmap(image, x, y, width, height),
      index * (width + 1) * CROP.zoom,
      0,
      width * CROP.zoom,
      height * CROP.zoom,
    );
  }
  return canvas.toDataURL("image/png").replace(/^.*,/, "");
}

function toBase64(bytes: Uint8ClampedArray): string {
  const CHUNK = 0x8000;
  let text = "";
  for (let at = 0; at < bytes.length; at += CHUNK) {
    text += String.fromCharCode(...bytes.subarray(at, at + CHUNK));
  }
  return btoa(text);
}

async function adapter(): Promise<AdapterReport> {
  const found = await navigator.gpu?.requestAdapter();
  const info = found?.info;
  return {
    backend: drawWebGpu !== null,
    available: Boolean(found),
    vendor: info?.vendor ?? "",
    architecture: info?.architecture ?? "",
    description: info?.description ?? "",
    fallback: info?.isFallbackAdapter ?? false,
  };
}

const hooks: VisualHooks = {
  async loadFont(base64) {
    const bytes = Uint8Array.from(atob(base64), (char) => char.charCodeAt(0));
    document.fonts.add(await new FontFace(TEST_FONT, bytes).load());
    testFontLoaded = true;
  },
  cases: () => CASE_NAMES,
  characters(name) {
    // At the scale of the golden image: more of the sheet is in a smaller picture.
    const { scale, dark } = CASES[name];
    const { frame, pages } = buildCase(name, loadTheme("product", dark), scale);
    const texts = pages.flatMap((page) => [
      ...page.texts,
      ...page.overflows.map((overflow) => overflow.text),
    ]);
    const names = frame.remotePresences?.map((presence) => presence.displayName) ?? [];
    const { columns } = frame;
    if (columns) {
      const count = frame.sheet?.cols ?? 0;
      names.push(...Array.from({ length: count }, (_, col) => columns.nameOf(col)), "\u{2026}");
    }
    return [...new Set([...texts, ...names].join(""))].join("");
  },
  adapter,
  async pixels(name, renderer, options = {}) {
    const { width, height, rgba } = await draw(build(name, options).built.frame, renderer);
    return { width, height, rgba: toBase64(rgba) };
  },
  compare,
  mutate: (name, mutation, options = {}) => compare(name, { ...options, mutation }),
};

window.__visual = hooks;
