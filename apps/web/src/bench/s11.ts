// Drawing about ten thousand visible cells, the sheet zoomed out: what it costs
// on a 2D canvas today, and what a prototype on WebGPU would cost with each of
// two ways to keep text in a texture.
//
// `string`: every cell's text is rasterised whole, by a 2D canvas, when its
// page is first drawn. `glyph`: characters are rasterised once and a text is
// put together from them.
//
// The prototype keeps one buffer of instances for a page and moves it by a
// uniform, so that scrolling touches nothing but that uniform.

import {
  acquire,
  Canvas2dRenderer,
  type GridRenderer,
  type Page,
  PageCache,
  type RendererCounters,
  readTheme,
  TextMeasurer,
  type Theme,
  WebGpuRenderer,
} from "@reticula/grid/internals";
import { FLAG_NUMBER, PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import { stage } from "./stage.ts";
import { nextFrame, Random, type Summary, summarize } from "./stats.ts";

const SHEET = { rows: 1_000_000, cols: 400 };
const METRICS = {
  rowHeight: 24,
  colWidth: 112,
  headerHeight: 24,
  headerWidth: 64,
  scrollbarSize: 12,
};
const PADDING = 6;

// The type library that comes with TypeScript has the interfaces of WebGPU but
// not its constants.
const USAGE = { copyDst: 0x8, vertex: 0x20, uniform: 0x40 };
const TEXTURE = { copyDst: 0x2, binding: 0x4, attachment: 0x10 };
const STAGE = { vertex: 0x1, fragment: 0x2 };

export type TextMode = "string" | "glyph";

/** Three thousand different characters: a sheet of real Chinese text has thousands. */
const HANZI = Array.from({ length: 3000 }, (_, index) =>
  String.fromCodePoint(0x4e00 + index * 5),
).join("");
const LETTERS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-";

function number(random: Random): string {
  const whole = random.below(10 ** (1 + random.below(7)));
  const sign = random.below(8) === 0 ? "-" : "";
  return `${sign}${whole}.${String(random.below(100)).padStart(2, "0")}`;
}

function pick(random: Random, alphabet: string, least: number, most: number): string {
  let text = "";
  for (let length = least + random.below(most - least + 1); length > 0; length--) {
    text += alphabet.charAt(random.below(alphabet.length));
  }
  return text;
}

/** Half numbers, three in ten Chinese, two in ten ASCII; every text its own. */
function syntheticPage(pageRow: number, pageCol: number): Page {
  const firstRow = pageRow * PAGE_ROWS;
  const firstCol = pageCol * PAGE_COLS;
  const rowCount = Math.min(PAGE_ROWS, SHEET.rows - firstRow);
  const colCount = Math.min(PAGE_COLS, SHEET.cols - firstCol);
  const texts = new Array<string>(rowCount * colCount);
  const flags = new Uint16Array(rowCount * colCount);
  const random = new Random(
    Math.imul(pageRow + 1, 0x9e3779b1) ^ Math.imul(pageCol + 1, 0x85ebca6b),
  );
  for (let cell = 0; cell < texts.length; cell++) {
    const kind = random.below(10);
    if (kind < 5) {
      texts[cell] = number(random);
      flags[cell] = FLAG_NUMBER;
    } else if (kind < 8) {
      texts[cell] = pick(random, HANZI, 2, 7);
    } else {
      texts[cell] = pick(random, LETTERS, 4, 12);
    }
  }
  return {
    sheet: 0,
    epoch: 0,
    firstRow,
    firstCol,
    rowCount,
    colCount,
    rowIds: new BigUint64Array(rowCount),
    colIds: new BigUint64Array(colCount),
    texts,
    styleIds: new Uint16Array(rowCount * colCount),
    flags,
    overflows: [],
    byteLength: 0,
  };
}

// --- quality -----------------------------------------------------------------

interface Difference {
  /** Share of the pixels the reference inked that differ by more than 16, 48 and 96 levels. */
  readonly over16: number;
  readonly over48: number;
  readonly over96: number;
  /** Mean difference, in levels, over the pixels either image inked. */
  readonly mean: number;
  readonly max: number;
}

function difference(a: Uint8ClampedArray, b: Uint8ClampedArray, background: number): Difference {
  let inked = 0;
  let sum = 0;
  let max = 0;
  const over = [0, 0, 0];
  for (let i = 0; i < a.length; i += 4) {
    let worst = 0;
    let ink = false;
    for (let c = 0; c < 3; c++) {
      const x = a[i + c] ?? 0;
      const y = b[i + c] ?? 0;
      worst = Math.max(worst, Math.abs(x - y));
      ink ||= x !== background || y !== background;
    }
    if (!ink) {
      continue;
    }
    inked++;
    sum += worst;
    max = Math.max(max, worst);
    if (worst > 16) over[0] = (over[0] ?? 0) + 1;
    if (worst > 48) over[1] = (over[1] ?? 0) + 1;
    if (worst > 96) over[2] = (over[2] ?? 0) + 1;
  }
  const share = (count: number | undefined) => (inked > 0 ? (count ?? 0) / inked : 0);
  return {
    over16: share(over[0]),
    over48: share(over[1]),
    over96: share(over[2]),
    mean: inked > 0 ? sum / inked : 0,
    max,
  };
}

interface Placed {
  readonly text: string;
  /** Device pixels; the left end of the text and the middle of its line. */
  readonly x: number;
  readonly midY: number;
}

function place(scale: number, rows: number, cols: number, seed: number): Placed[] {
  const random = new Random(seed);
  const placed: Placed[] = [];
  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < cols; col++) {
      const kind = random.below(10);
      const text =
        kind < 5
          ? number(random)
          : kind < 8
            ? pick(random, HANZI, 2, 5)
            : pick(random, LETTERS, 4, 9);
      // Positions as layout leaves them: whole CSS pixels in, fractions of device pixels out.
      placed.push({
        text,
        x: (col * 112 + PADDING + random.unit()) * scale,
        midY: (row * 24 + 12) * scale,
      });
    }
  }
  return placed;
}

function surface(width: number, height: number, alpha: boolean): OffscreenCanvasRenderingContext2D {
  const context = new OffscreenCanvas(width, height).getContext("2d", {
    alpha,
    willReadFrequently: true,
  });
  if (!context) {
    throw new Error("no 2D context");
  }
  return context;
}

/**
 * How close each way of putting text together comes to what a 2D canvas draws
 * directly. Everything here is done on 2D canvases: compositing a mask is
 * the same arithmetic a blending unit does.
 */
export function quality(options: { scale: number; color?: string; background?: string }) {
  const { scale } = options;
  const color = options.color ?? "#1f2328";
  const background = options.background ?? "#ffffff";
  const host = stage();
  host.classList.add("reticula-grid");
  const { fontFamily, fontSize } = readTheme(host);
  host.classList.remove("reticula-grid");
  const px = fontSize * scale;
  const font = `${px}px ${fontFamily}`;
  const rows = 24;
  const cols = 8;
  const width = Math.ceil(cols * 112 * scale);
  const height = Math.ceil(rows * 24 * scale);
  const placed = place(scale, rows, cols, 7);

  const finish = (context: OffscreenCanvasRenderingContext2D) =>
    context.getImageData(0, 0, width, height).data;
  const open = (alpha = false) => {
    const context = surface(width, height, alpha);
    context.fillStyle = background;
    context.fillRect(0, 0, width, height);
    context.font = font;
    context.fillStyle = color;
    return context;
  };

  const probe = open();
  const metrics = probe.measureText("Hg");
  const lift = (metrics.fontBoundingBoxAscent - metrics.fontBoundingBoxDescent) / 2;
  const baselineOf = (midY: number) => Math.round(midY + lift);

  // What the renderer does today: a scaled context and a "middle" baseline.
  const today = (() => {
    const context = surface(width, height, false);
    context.fillStyle = background;
    context.fillRect(0, 0, width, height);
    context.setTransform(scale, 0, 0, scale, 0, 0);
    context.font = `${fontSize}px ${fontFamily}`;
    context.textBaseline = "middle";
    context.fillStyle = color;
    for (const { text, x, midY } of placed) {
      context.fillText(text, x / scale, midY / scale);
    }
    return finish(context);
  })();

  const direct = (x: (x: number) => number, kerning: CanvasFontKerning, alpha = false) => {
    const context = open(alpha);
    context.fontKerning = kerning;
    for (const run of placed) {
      context.fillText(run.text, x(run.x), baselineOf(run.midY));
    }
    return finish(context);
  };
  const exact = direct((x) => x, "auto");
  const snapped = direct(Math.round, "auto");
  const snappedNoKerning = direct(Math.round, "none");
  const snappedGray = direct(Math.round, "auto", true);
  const snappedNoKerningGray = direct(Math.round, "none", true);
  const exactNoKerningGray = direct((x) => x, "none", true);

  // A mask: black text on nothing, of which only the coverage is kept.
  const mask = (text: string, phase: number, kerning: CanvasFontKerning, ink: string) => {
    const pad = 2;
    const measured = probe.measureText(text);
    const w = Math.max(1, Math.ceil(measured.width + px) + 2 * pad);
    const h = Math.ceil(px * 1.8);
    const context = surface(w, h, true);
    context.font = font;
    context.fontKerning = kerning;
    context.fillStyle = ink;
    const base = Math.round(h * 0.7);
    context.fillText(text, pad + phase, base);
    return { canvas: context.canvas, pad, base, w, h };
  };
  const tint = (m: ReturnType<typeof mask>) => {
    const context = surface(m.w, m.h, true);
    context.drawImage(m.canvas, 0, 0);
    context.globalCompositeOperation = "source-in";
    context.fillStyle = color;
    context.fillRect(0, 0, m.w, m.h);
    return context.canvas;
  };

  const strings = (ink: string | null, kerning: CanvasFontKerning) => {
    const context = open();
    for (const run of placed) {
      const m = mask(run.text, 0, kerning, ink ?? color);
      context.drawImage(
        ink === null ? m.canvas : tint(m),
        Math.round(run.x) - m.pad,
        baselineOf(run.midY) - m.base,
      );
    }
    return finish(context);
  };
  const glyphs = (phases: number, ink: string | null, snap = true) => {
    const context = open();
    probe.fontKerning = "none";
    const cache = new Map<string, { image: OffscreenCanvas; pad: number; base: number }>();
    const advances = new Map<string, number>();
    for (const run of placed) {
      let x = snap ? Math.round(run.x) : run.x;
      for (const char of run.text) {
        // The nearest phase, as the rasteriser itself chooses it.
        const pen = Math.floor(x * phases + 0.5) / phases;
        const whole = Math.floor(pen);
        const phase = pen - whole;
        const key = `${char}${phase}`;
        let glyph = cache.get(key);
        if (!glyph) {
          const m = mask(char, phase, "none", ink ?? color);
          glyph = { image: ink === null ? m.canvas : tint(m), pad: m.pad, base: m.base };
          cache.set(key, glyph);
        }
        context.drawImage(glyph.image, whole - glyph.pad, baselineOf(run.midY) - glyph.base);
        let advance = advances.get(char);
        if (advance === undefined) {
          advance = probe.measureText(char).width;
          advances.set(char, advance);
        }
        x += advance;
      }
    }
    probe.fontKerning = "auto";
    return finish(context);
  };

  const gray = Number.parseInt(background.slice(1, 3), 16);
  let lcd = 0;
  let lcdGray = 0;
  for (let i = 0; i < snapped.length; i += 4) {
    if (color === "#000000" || color === "#1f2328") {
      const spread = (data: Uint8ClampedArray) =>
        Math.max(
          Math.abs((data[i] ?? 0) - (data[i + 1] ?? 0)),
          Math.abs((data[i + 1] ?? 0) - (data[i + 2] ?? 0)),
        );
      if (spread(snapped) > 24) lcd++;
      if (spread(snappedGray) > 24) lcdGray++;
    }
  }
  const compare = (a: Uint8ClampedArray, b: Uint8ClampedArray) => difference(a, b, gray);
  return {
    scale,
    color,
    background,
    font,
    lcdPixelsOpaqueCanvas: lcd,
    lcdPixelsAlphaCanvas: lcdGray,
    todayVsExact: compare(today, exact),
    exactVsSnapped: compare(exact, snapped),
    kerningVsNone: compare(snapped, snappedNoKerning),
    opaqueVsAlphaCanvas: compare(snapped, snappedGray),
    stringOwnColor: compare(snappedGray, strings(null, "auto")),
    stringBlackMask: compare(snappedGray, strings("#000000", "auto")),
    stringWhiteMask: compare(snappedGray, strings("#ffffff", "auto")),
    glyph1OwnColor: compare(snappedNoKerningGray, glyphs(1, null)),
    glyph2OwnColor: compare(snappedNoKerningGray, glyphs(2, null)),
    glyph4OwnColor: compare(snappedNoKerningGray, glyphs(4, null)),
    glyph4Fractional: compare(exactNoKerningGray, glyphs(4, null, false)),
    glyph4BlackMask: compare(snappedNoKerningGray, glyphs(4, "#000000")),
    glyph4WhiteMask: compare(snappedNoKerningGray, glyphs(4, "#ffffff")),
  };
}

// --- the prototype -----------------------------------------------------------

const SHADER = /* wgsl */ `
struct Frame { viewport: vec2f, atlas: vec2f };
struct PageOrigin { origin: vec2f };
@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var glyphs: texture_2d<f32>;
@group(1) @binding(0) var<uniform> page: PageOrigin;
struct Out { @builtin(position) position: vec4f, @location(0) uv: vec2f, @location(1) @interpolate(flat) color: vec4f };
fn clip(point: vec2f) -> vec4f {
  return vec4f(point.x / frame.viewport.x * 2.0 - 1.0, 1.0 - point.y / frame.viewport.y * 2.0, 0.0, 1.0);
}
@vertex fn text(@builtin(vertex_index) v: u32, @location(0) pos: vec2f, @location(1) rect: vec4u, @location(2) color: vec4f) -> Out {
  let corner = vec2f(f32(v & 1u), f32(v >> 1u));
  var out: Out;
  out.position = clip(page.origin + pos + corner * vec2f(rect.zw));
  out.uv = vec2f(rect.xy) + corner * vec2f(rect.zw);
  out.color = color;
  return out;
}
@fragment fn textColor(in: Out) -> @location(0) vec4f {
  return in.color * textureLoad(glyphs, vec2u(in.uv), 0).a;
}
@vertex fn rect(@builtin(vertex_index) v: u32, @location(0) box: vec4f, @location(1) color: vec4f) -> Out {
  let corner = vec2f(f32(v & 1u), f32(v >> 1u));
  var out: Out;
  out.position = clip(box.xy + corner * box.zw);
  out.color = color;
  return out;
}
@fragment fn rectColor(in: Out) -> @location(0) vec4f { return in.color; }
`;

const ATLAS = 4096;
const INSTANCE = 20;

interface Held {
  readonly buffer: GPUBuffer;
  readonly count: number;
}

class Prototype {
  readonly device: GPUDevice;
  readonly #canvas: HTMLCanvasElement;
  readonly #context: GPUCanvasContext;
  readonly #textPipeline: GPURenderPipeline;
  readonly #rectPipeline: GPURenderPipeline;
  readonly #atlas: GPUTexture;
  readonly #frame: GPUBuffer;
  readonly #origins: GPUBuffer;
  readonly #rects: GPUBuffer;
  readonly #group0: GPUBindGroup;
  readonly #group1: GPUBindGroup;
  readonly #mode: TextMode;
  readonly #px: number;
  readonly #scale: number;
  readonly #raster: CanvasRenderingContext2D;
  readonly #lift: number;
  readonly #line: number;
  readonly #bearing: number;
  readonly #pages = new Map<string, Held>();
  readonly #glyphs = new Map<string, { x: number; y: number; w: number }>();
  readonly #advances = new Map<string, number>();
  #shelfX = 0;
  #shelfY = 0;
  #wraps = 0;
  builds = 0;
  buildTime = 0;
  uploads = 0;
  instances = 0;

  constructor(
    device: GPUDevice,
    canvas: HTMLCanvasElement,
    theme: Theme,
    scale: number,
    mode: TextMode,
  ) {
    this.device = device;
    this.#canvas = canvas;
    this.#mode = mode;
    this.#scale = scale;
    this.#px = theme.fontSize * scale;
    const context = canvas.getContext("webgpu") as unknown as GPUCanvasContext | null;
    if (!context) {
      throw new Error("no webgpu context");
    }
    this.#context = context;
    const format = navigator.gpu.getPreferredCanvasFormat();
    context.configure({ device, format, alphaMode: "opaque" });
    const module = device.createShaderModule({ code: SHADER });
    const frameLayout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: STAGE.vertex, buffer: {} },
        { binding: 1, visibility: STAGE.fragment, texture: {} },
      ],
    });
    const pageLayout = device.createBindGroupLayout({
      entries: [{ binding: 0, visibility: STAGE.vertex, buffer: { hasDynamicOffset: true } }],
    });
    const layout = device.createPipelineLayout({ bindGroupLayouts: [frameLayout, pageLayout] });
    const blend: GPUBlendState = {
      color: { srcFactor: "one", dstFactor: "one-minus-src-alpha" },
      alpha: { srcFactor: "one", dstFactor: "one-minus-src-alpha" },
    };
    const pipeline = (vertex: string, fragment: string, buffers: GPUVertexBufferLayout[]) =>
      device.createRenderPipeline({
        layout,
        vertex: { module, entryPoint: vertex, buffers },
        fragment: { module, entryPoint: fragment, targets: [{ format, blend }] },
        primitive: { topology: "triangle-strip" },
      });
    this.#textPipeline = pipeline("text", "textColor", [
      {
        arrayStride: INSTANCE,
        stepMode: "instance",
        attributes: [
          { shaderLocation: 0, offset: 0, format: "float32x2" },
          { shaderLocation: 1, offset: 8, format: "uint16x4" },
          { shaderLocation: 2, offset: 16, format: "unorm8x4" },
        ],
      },
    ]);
    this.#rectPipeline = pipeline("rect", "rectColor", [
      {
        arrayStride: 20,
        stepMode: "instance",
        attributes: [
          { shaderLocation: 0, offset: 0, format: "float32x4" },
          { shaderLocation: 1, offset: 16, format: "unorm8x4" },
        ],
      },
    ]);
    this.#atlas = device.createTexture({
      size: [ATLAS, ATLAS],
      format: "rgba8unorm",
      usage: TEXTURE.binding | TEXTURE.copyDst | TEXTURE.attachment,
    });
    const buffer = (size: number, usage: number) =>
      device.createBuffer({ size, usage: usage | USAGE.copyDst });
    this.#frame = buffer(16, USAGE.uniform);
    this.#origins = buffer(256 * 256, USAGE.uniform);
    this.#rects = buffer(1 << 16, USAGE.vertex);
    this.#group0 = device.createBindGroup({
      layout: frameLayout,
      entries: [
        { binding: 0, resource: { buffer: this.#frame } },
        { binding: 1, resource: this.#atlas.createView() },
      ],
    });
    this.#group1 = device.createBindGroup({
      layout: pageLayout,
      entries: [{ binding: 0, resource: { buffer: this.#origins, size: 16 } }],
    });
    // Not `willReadFrequently`: that asks for a canvas in main memory, which
    // rasterises ten times slower and not quite the same.
    const scratch = document.createElement("canvas");
    scratch.width = ATLAS;
    scratch.height = 512;
    const raster = scratch.getContext("2d");
    if (!raster) {
      throw new Error("no 2D context");
    }
    this.#raster = raster;
    this.#raster.font = `${this.#px}px ${theme.fontFamily}`;
    this.#raster.fontKerning = mode === "glyph" ? "none" : "auto";
    this.#raster.fillStyle = "#000000";
    const metrics = this.#raster.measureText("Hg");
    this.#lift = (metrics.fontBoundingBoxAscent - metrics.fontBoundingBoxDescent) / 2;
    this.#line = Math.ceil(this.#px * 1.5) + 2;
    this.#bearing = Math.ceil(this.#px * 0.35) + 1;
  }

  #advance(char: string): number {
    let advance = this.#advances.get(char);
    if (advance === undefined) {
      advance = this.#raster.measureText(char).width;
      this.#advances.set(char, advance);
    }
    return advance;
  }

  #reserve(width: number): { x: number; y: number } {
    if (this.#shelfX + width > ATLAS) {
      this.#shelfX = 0;
      this.#shelfY += this.#line;
      if (this.#shelfY + this.#line > ATLAS) {
        // The prototype has no eviction; it starts over and counts how often.
        this.#shelfY = 0;
        this.#wraps++;
        this.#glyphs.clear();
      }
    }
    const at = { x: this.#shelfX, y: this.#shelfY };
    this.#shelfX += width;
    return at;
  }

  #glyph(char: string, phase: number): { x: number; y: number; w: number } {
    const key = `${char}${phase}`;
    let glyph = this.#glyphs.get(key);
    if (!glyph) {
      const w = Math.ceil(this.#advance(char)) + 2 * this.#bearing;
      const at = this.#reserve(w);
      const raster = this.#raster;
      raster.clearRect(0, 0, w, this.#line);
      raster.fillText(char, this.#bearing + phase / 4, Math.round(this.#px * 1.1));
      this.device.queue.copyExternalImageToTexture(
        { source: raster.canvas, origin: [0, 0] },
        { texture: this.#atlas, origin: [at.x, at.y], premultipliedAlpha: true },
        [w, this.#line],
      );
      this.uploads++;
      glyph = { ...at, w };
      this.#glyphs.set(key, glyph);
    }
    return glyph;
  }

  #build(page: Page): Held {
    const started = performance.now();
    const scale = this.#scale;
    const limit = page.texts.length * 16;
    const floats = new Float32Array((limit * INSTANCE) / 4);
    const shorts = new Uint16Array(floats.buffer);
    const words = new Uint32Array(floats.buffer);
    let count = 0;
    const push = (x: number, y: number, u: number, v: number, w: number, h: number) => {
      if (count >= limit) {
        return;
      }
      const at = (count * INSTANCE) / 4;
      floats[at] = x;
      floats[at + 1] = y;
      shorts[at * 2 + 4] = u;
      shorts[at * 2 + 5] = v;
      shorts[at * 2 + 6] = w;
      shorts[at * 2 + 7] = h;
      words[at + 4] = 0xff28231f;
      count++;
    };
    const colW = METRICS.colWidth * scale;
    const rowH = METRICS.rowHeight * scale;
    const pad = PADDING * scale;
    const top = Math.round(this.#px * 1.1);
    const raster = this.#raster;
    // For `string`, the texts of a page share one strip that is uploaded at once.
    let stripX = 0;
    let stripY = 0;
    const pending: [number, number, number, number, number][] = [];
    if (this.#mode === "string") {
      raster.clearRect(0, 0, ATLAS, 512);
    }
    const flush = () => {
      if (pending.length === 0) {
        return;
      }
      const height = stripY + this.#line;
      if (this.#shelfY + height > ATLAS) {
        this.#shelfY = 0;
        this.#wraps++;
      }
      this.device.queue.copyExternalImageToTexture(
        { source: raster.canvas, origin: [0, 0] },
        { texture: this.#atlas, origin: [0, this.#shelfY], premultipliedAlpha: true },
        [ATLAS, Math.min(512, height)],
      );
      this.uploads++;
      for (const [x, y, u, v, w] of pending) {
        push(x, y, u, v + this.#shelfY, w, this.#line);
      }
      this.#shelfY += height;
      pending.length = 0;
      stripX = 0;
      stripY = 0;
      raster.clearRect(0, 0, ATLAS, 512);
    };
    for (let row = 0; row < page.rowCount; row++) {
      const baseline = Math.round((row + 0.5) * rowH + this.#lift);
      for (let col = 0; col < page.colCount; col++) {
        const cell = row * page.colCount + col;
        const text = page.texts[cell];
        if (!text) {
          continue;
        }
        const right = ((page.flags[cell] ?? 0) & FLAG_NUMBER) !== 0;
        const left = col * colW + pad;
        const room = colW - 2 * pad;
        if (this.#mode === "string") {
          const width = raster.measureText(text).width;
          const w = Math.min(Math.ceil(width) + 2, Math.ceil(room));
          if (stripX + w > ATLAS) {
            stripX = 0;
            stripY += this.#line;
            if (stripY + this.#line > 512) {
              flush();
            }
          }
          const x = Math.round(right ? left + room - width : left);
          raster.save();
          raster.beginPath();
          raster.rect(stripX, stripY, w, this.#line);
          raster.clip();
          raster.fillText(text, stripX, stripY + top);
          raster.restore();
          pending.push([x, baseline - top, stripX, stripY, w]);
          stripX += w + 1;
          continue;
        }
        let width = 0;
        for (const char of text) {
          width += this.#advance(char);
        }
        let x = right ? left + room - width : left;
        const end = left + room;
        for (const char of text) {
          const pen = Math.floor(x * 4 + 0.5) / 4;
          const whole = Math.floor(pen);
          const glyph = this.#glyph(char, (pen - whole) * 4);
          const w = Math.min(glyph.w, Math.ceil(end) - whole + this.#bearing);
          if (w > 0) {
            push(whole - this.#bearing, baseline - top, glyph.x, glyph.y, w, this.#line);
          }
          x += this.#advance(char);
        }
      }
    }
    flush();
    const buffer = this.device.createBuffer({
      size: Math.max(INSTANCE, count * INSTANCE),
      usage: USAGE.vertex | USAGE.copyDst,
    });
    this.device.queue.writeBuffer(buffer, 0, floats.buffer, 0, count * INSTANCE);
    this.builds++;
    this.buildTime += performance.now() - started;
    return { buffer, count };
  }

  /** Where the text of a cell goes, for a reference drawn on a 2D canvas. */
  get lift(): number {
    return this.#lift;
  }

  /** Draws into a texture and returns its pixels as RGBA. */
  async read(pages: readonly Page[], scrollTop: number): Promise<Uint8ClampedArray> {
    const { width, height } = this.#canvas;
    const format = navigator.gpu.getPreferredCanvasFormat();
    const texture = this.device.createTexture({ size: [width, height], format, usage: 0x10 | 0x1 });
    this.draw(pages, scrollTop, texture.createView());
    const bytesPerRow = Math.ceil((width * 4) / 256) * 256;
    const buffer = this.device.createBuffer({ size: bytesPerRow * height, usage: 0x1 | 0x8 });
    const encoder = this.device.createCommandEncoder();
    encoder.copyTextureToBuffer({ texture }, { buffer, bytesPerRow }, [width, height]);
    this.device.queue.submit([encoder.finish()]);
    await buffer.mapAsync(1);
    const raw = new Uint8Array(buffer.getMappedRange());
    const out = new Uint8ClampedArray(width * height * 4);
    const swap = format.startsWith("bgra");
    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width; x++) {
        const from = y * bytesPerRow + x * 4;
        const to = (y * width + x) * 4;
        out[to] = raw[from + (swap ? 2 : 0)] ?? 0;
        out[to + 1] = raw[from + 1] ?? 0;
        out[to + 2] = raw[from + (swap ? 0 : 2)] ?? 0;
        out[to + 3] = raw[from + 3] ?? 0;
      }
    }
    buffer.unmap();
    return out;
  }

  draw(pages: readonly Page[], scrollTop: number, target?: GPUTextureView): void {
    const { width, height } = this.#canvas;
    const scale = this.#scale;
    this.device.queue.writeBuffer(this.#frame, 0, new Float32Array([width, height, ATLAS, ATLAS]));
    const wanted = new Set<string>();
    const origins = new Float32Array((256 / 4) * Math.max(1, pages.length));
    const held: Held[] = [];
    this.instances = 0;
    for (const [index, page] of pages.entries()) {
      const key = `${page.firstRow}:${page.firstCol}`;
      wanted.add(key);
      let entry = this.#pages.get(key);
      if (!entry) {
        entry = this.#build(page);
        this.#pages.set(key, entry);
      }
      held.push(entry);
      this.instances += entry.count;
      origins[(index * 256) / 4] =
        METRICS.headerWidth * scale + page.firstCol * METRICS.colWidth * scale;
      origins[(index * 256) / 4 + 1] = Math.round(
        METRICS.headerHeight * scale + (page.firstRow * METRICS.rowHeight - scrollTop) * scale,
      );
    }
    for (const [key, entry] of this.#pages) {
      if (!wanted.has(key)) {
        entry.buffer.destroy();
        this.#pages.delete(key);
      }
    }
    this.device.queue.writeBuffer(this.#origins, 0, origins);

    const rects: number[] = [];
    const rowH = METRICS.rowHeight * scale;
    const colW = METRICS.colWidth * scale;
    const line = Math.max(1, Math.round(scale));
    for (
      let y = Math.round(METRICS.headerHeight * scale - ((scrollTop * scale) % rowH));
      y < height;
      y += rowH
    ) {
      rects.push(0, Math.round(y), width, line);
    }
    for (let x = METRICS.headerWidth * scale; x < width; x += colW) {
      rects.push(Math.round(x), 0, line, height);
    }
    const data = new Float32Array((rects.length / 4) * 5);
    const words = new Uint32Array(data.buffer);
    for (let i = 0; i < rects.length / 4; i++) {
      data.set(rects.slice(i * 4, i * 4 + 4), i * 5);
      words[i * 5 + 4] = 0xffe8e5e3;
    }
    this.device.queue.writeBuffer(this.#rects, 0, data);

    const encoder = this.device.createCommandEncoder();
    const pass = encoder.beginRenderPass({
      colorAttachments: [
        {
          view: target ?? this.#context.getCurrentTexture().createView(),
          clearValue: [1, 1, 1, 1],
          loadOp: "clear",
          storeOp: "store",
        },
      ],
    });
    pass.setBindGroup(0, this.#group0);
    pass.setBindGroup(1, this.#group1, [0]);
    pass.setPipeline(this.#rectPipeline);
    pass.setVertexBuffer(0, this.#rects);
    pass.draw(4, rects.length / 4);
    pass.setPipeline(this.#textPipeline);
    for (const [index, entry] of held.entries()) {
      pass.setBindGroup(1, this.#group1, [index * 256]);
      pass.setVertexBuffer(0, entry.buffer);
      pass.draw(4, entry.count);
    }
    pass.end();
    this.device.queue.submit([encoder.finish()]);
  }

  usage() {
    let bytes = 0;
    for (const entry of this.#pages.values()) {
      bytes += entry.count * INSTANCE;
    }
    return {
      pages: this.#pages.size,
      instanceBytes: bytes,
      instances: this.instances,
      glyphs: this.#glyphs.size,
      atlasWraps: this.#wraps,
      atlasRowsUsed: this.#shelfY + this.#line,
      builds: this.builds,
      uploads: this.uploads,
      buildMs: this.builds > 0 ? this.buildTime / this.builds : 0,
    };
  }
}

/**
 * What the prototype really puts into a texture, against the same cells drawn
 * on a 2D canvas: text in grey levels, without kerning, on whole baselines.
 */
export async function fidelity(options: { zoom: number }) {
  const host = stage();
  host.classList.add("reticula-grid");
  const theme = readTheme(host);
  host.classList.remove("reticula-grid");
  const scale = (window.devicePixelRatio || 1) * options.zoom;
  const canvas = document.createElement("canvas");
  canvas.width = Math.round(1100 * scale);
  canvas.height = Math.round(700 * scale);
  const adapter = await navigator.gpu?.requestAdapter();
  if (!adapter) {
    throw new Error("no WebGPU adapter");
  }
  const prototype = new Prototype(await adapter.requestDevice(), canvas, theme, scale, "glyph");
  const reference = document.createElement("canvas");
  reference.width = canvas.width;
  reference.height = canvas.height;
  const context = reference.getContext("2d");
  if (!context) {
    throw new Error("no 2D context");
  }
  const colW = METRICS.colWidth * scale;
  const rowH = METRICS.rowHeight * scale;
  const pad = PADDING * scale;
  context.font = `${theme.fontSize * scale}px ${theme.fontFamily}`;
  context.fontKerning = "none";
  context.fillStyle = theme.background;
  context.fillRect(0, 0, canvas.width, canvas.height);
  const page = syntheticPage(3, 0);
  const texts = page.texts.map((text) =>
    context.measureText(text).width <= colW - 2 * pad ? text : "",
  );
  const shown: Page = { ...page, firstRow: 0, texts };
  const line = Math.max(1, Math.round(scale));
  context.fillStyle = "#e3e5e8";
  for (let y = METRICS.headerHeight * scale; y < canvas.height; y += rowH) {
    context.fillRect(0, Math.round(y), canvas.width, line);
  }
  for (let x = METRICS.headerWidth * scale; x < canvas.width; x += colW) {
    context.fillRect(Math.round(x), 0, line, canvas.height);
  }
  context.fillStyle = "#1f2328";
  const originX = METRICS.headerWidth * scale;
  const originY = Math.round(METRICS.headerHeight * scale);
  for (let row = 0; row < shown.rowCount; row++) {
    const baseline = originY + Math.round((row + 0.5) * rowH + prototype.lift);
    for (let col = 0; col < shown.colCount; col++) {
      const cell = row * shown.colCount + col;
      const text = texts[cell];
      if (text) {
        const left = originX + col * colW + pad;
        const right = ((shown.flags[cell] ?? 0) & FLAG_NUMBER) !== 0;
        const x = right ? left + colW - 2 * pad - context.measureText(text).width : left;
        context.fillText(text, x, baseline);
      }
    }
  }
  const drawn = await prototype.read([shown], 0);
  const wanted = context.getImageData(0, 0, canvas.width, canvas.height).data;
  let dark = 0;
  for (let i = 0; i < drawn.length; i += 4) {
    if ((drawn[i] ?? 255) < 128) {
      dark++;
    }
  }
  prototype.device.destroy();
  return {
    zoom: options.zoom,
    scale,
    adapter: `${adapter.info.vendor} ${adapter.info.architecture}`,
    texts: texts.filter((text) => text).length,
    darkPixels: dark,
    difference: difference(wanted, drawn, 255),
  };
}

/** The grid's own renderers, and the prototype. */
export type Subject = "canvas" | "webgpu" | TextMode;

export interface ScrollResult {
  readonly subject: Subject;
  readonly zoom: number;
  readonly rowsPerSecond: number;
  readonly visibleCells: number;
  readonly adapter: string;
  readonly frames: number;
  /** Milliseconds of the main thread in a frame. */
  readonly work: Summary;
  /** Milliseconds between frames. */
  readonly interval: Summary;
  readonly framesOver60Hz: number;
  /** The frames of the warm-up, the first of which finds nothing drawn before. */
  readonly cold: { readonly work: Summary; readonly interval: Summary };
  readonly usage: ReturnType<Prototype["usage"]> | null;
  readonly counters: {
    readonly warm: RendererCounters | null;
    readonly end: RendererCounters | null;
  };
}

/** Scrolls the zoomed-out sheet; `screensPerSecond` of 0 jumps a screen a frame. */
export async function scroll(options: {
  subject: Subject;
  zoom: number;
  screensPerSecond: number;
  seconds: number;
  warmupSeconds: number;
}): Promise<ScrollResult> {
  const host = stage();
  host.classList.add("reticula-grid");
  const dpr = window.devicePixelRatio || 1;
  const scale = dpr * options.zoom;
  const lease = options.subject === "webgpu" ? await acquire(false, () => {}) : null;
  const renderer: GridRenderer | null = lease
    ? new WebGpuRenderer(lease, { measure: new TextMeasurer(), onIncomplete: () => {} })
    : options.subject === "canvas"
      ? new Canvas2dRenderer()
      : null;
  const canvas = renderer?.canvas ?? document.createElement("canvas");
  canvas.width = Math.round(host.clientWidth * dpr);
  canvas.height = Math.round(host.clientHeight * dpr);
  canvas.style.width = `${host.clientWidth}px`;
  canvas.style.height = `${host.clientHeight}px`;
  host.append(canvas);
  const theme = readTheme(host);
  const size = { width: host.clientWidth / options.zoom, height: host.clientHeight / options.zoom };
  const rows = (size.height - METRICS.headerHeight) / METRICS.rowHeight;
  const cols = Math.ceil((size.width - METRICS.headerWidth) / METRICS.colWidth);
  const cache = new PageCache();
  let adapterName = lease?.gpu.adapter ?? "";
  let prototype: Prototype | null = null;
  if (!renderer && (options.subject === "string" || options.subject === "glyph")) {
    const adapter = await navigator.gpu?.requestAdapter();
    if (!adapter) {
      throw new Error("no WebGPU adapter");
    }
    adapterName = `${adapter.info.vendor} ${adapter.info.architecture}`;
    prototype = new Prototype(await adapter.requestDevice(), canvas, theme, scale, options.subject);
  }
  const made = new Map<string, Page>();
  // The grid holds pages around the ones in view.
  const spare = renderer ? 1 : 0;
  const pagesAt = (firstRow: number): Page[] => {
    const pages: Page[] = [];
    const wanted = new Set<string>();
    for (
      let pr = Math.max(0, Math.floor(firstRow / PAGE_ROWS) - spare);
      pr <= Math.floor((firstRow + rows) / PAGE_ROWS) + spare;
      pr++
    ) {
      for (let pc = 0; pc <= Math.floor((cols - 1) / PAGE_COLS); pc++) {
        const key = `${pr}:${pc}`;
        wanted.add(key);
        let page = made.get(key);
        if (!page) {
          page = syntheticPage(pr, pc);
          made.set(key, page);
          if (renderer) {
            cache.retain({ firstRow: pr - 8, endRow: pr + 8, firstCol: 0, endCol: 64 });
            cache.accept(page);
          }
        }
        pages.push(page);
      }
    }
    for (const key of made.keys()) {
      if (!wanted.has(key)) {
        made.delete(key);
      }
    }
    return pages;
  };

  const work: number[] = [];
  const intervals: number[] = [];
  const cold = { work: [] as number[], intervals: [] as number[] };
  let warm: RendererCounters | null = null;
  let top = 0;
  const started = await nextFrame();
  let buildsAt = 0;
  for (let last = started; ; ) {
    const now = await nextFrame();
    const elapsed = now - started;
    if (elapsed >= (options.warmupSeconds + options.seconds) * 1000) {
      break;
    }
    const step =
      options.screensPerSecond === 0
        ? rows * METRICS.rowHeight
        : (options.screensPerSecond * rows * METRICS.rowHeight * (now - last)) / 1000;
    top = Math.round(((top + step) % (500_000 * METRICS.rowHeight)) * scale) / scale;
    const firstRow = Math.floor(top / METRICS.rowHeight);
    const pages = pagesAt(firstRow);
    const begun = performance.now();
    if (renderer) {
      renderer.draw({
        size,
        scale,
        metrics: METRICS,
        theme,
        scroll: { top, left: 0 },
        visible: {
          firstRow,
          endRow: Math.ceil((top + size.height) / METRICS.rowHeight),
          firstCol: 0,
          endCol: cols,
        },
        cache,
        selection: null,
        sheet: SHEET,
      });
    } else {
      prototype?.draw(pages, top);
    }
    const spent = performance.now() - begun;
    if (elapsed >= options.warmupSeconds * 1000) {
      work.push(spent);
      intervals.push(now - last);
    } else {
      buildsAt = prototype?.builds ?? 0;
      cold.work.push(spent);
      cold.intervals.push(now - last);
      warm = renderer?.counters ?? null;
    }
    last = now;
  }
  const usage = prototype?.usage() ?? null;
  const end = renderer?.counters ?? null;
  renderer?.destroy();
  canvas.remove();
  prototype?.device.destroy();
  host.classList.remove("reticula-grid");
  void buildsAt;
  return {
    subject: options.subject,
    zoom: options.zoom,
    rowsPerSecond: options.screensPerSecond * rows,
    visibleCells: Math.round(rows) * cols,
    adapter: adapterName,
    frames: intervals.length,
    work: summarize(work),
    interval: summarize(intervals),
    framesOver60Hz: intervals.filter((sample) => sample > 1000 / 60 + 0.5).length,
    cold: { work: summarize(cold.work), interval: summarize(cold.intervals) },
    usage,
    counters: { warm, end },
  };
}
