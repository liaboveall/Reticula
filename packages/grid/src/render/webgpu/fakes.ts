// A GPU and a 2D canvas that write down what they are told, for the tests of this
// directory. Texts have the sizes of `fixtures.ts`.

import { vi } from "vitest";
import { measure } from "../fixtures.ts";
import { INSTANCE_BYTES } from "./pipeline.ts";

export interface FakeCall {
  readonly name: string;
  readonly args: readonly unknown[];
}

/** An object whose methods are written down. */
class Recorder {
  readonly calls: FakeCall[] = [];

  note(name: string, ...args: unknown[]): void {
    this.calls.push({ name, args });
  }

  named(name: string): FakeCall[] {
    return this.calls.filter((call) => call.name === name);
  }

  clear(): void {
    this.calls.length = 0;
  }
}

export interface FakeDraw {
  readonly buffer: unknown;
  readonly count: number;
  readonly first: number;
  readonly part: number;
  readonly scissor: readonly number[];
  readonly loadOp: string;
  /** Whether what it reads had been destroyed when it was submitted. */
  isStale: boolean;
}

interface FakeResource {
  readonly destroy: () => void;
  readonly isGone: () => boolean;
}

interface FakeGroup {
  readonly descriptor: GPUBindGroupDescriptor;
}

/** The buffers and textures of a bind group. */
function resourcesOf({ descriptor }: FakeGroup): FakeResource[] {
  return [...descriptor.entries].map(({ resource }) => {
    const { buffer, texture } = resource as unknown as {
      buffer?: FakeResource;
      texture?: FakeResource;
    };
    const bound = buffer ?? texture;
    if (!bound) {
      throw new Error("bound to nothing");
    }
    return bound;
  });
}

export class FakeDevice extends EventTarget {
  readonly recorder = new Recorder();
  readonly lost: Promise<{ reason: string }>;
  /** Everything that has been drawn, by submission. */
  readonly submitted: FakeDraw[][] = [];
  live = 0;
  destroyed = false;
  #lose: (info: { reason: string }) => void = () => {};

  constructor() {
    super();
    this.lost = new Promise((resolve) => {
      this.#lose = resolve;
    });
  }

  get asDevice(): GPUDevice {
    return this as unknown as GPUDevice;
  }

  readonly queue = {
    // The data is the GPU's from here on, whatever becomes of the array.
    writeBuffer: (...args: unknown[]) =>
      this.recorder.note(
        "writeBuffer",
        ...args.map((each) => (ArrayBuffer.isView(each) ? structuredClone(each) : each)),
      ),
    writeTexture: (...args: unknown[]) => this.recorder.note("writeTexture", ...args),
    copyExternalImageToTexture: (...args: unknown[]) =>
      this.recorder.note("copyExternalImageToTexture", ...args),
    submit: (buffers: { draws: FakeDraw[]; reads: Map<FakeDraw, FakeResource[]> }[]) => {
      this.recorder.note("submit");
      for (const { draws, reads } of buffers) {
        for (const draw of draws) {
          draw.isStale = reads.get(draw)?.some((resource) => resource.isGone()) ?? true;
        }
        if (draws.length > 0) {
          this.submitted.push(draws);
        }
      }
    },
  };

  destroy(): void {
    this.destroyed = true;
    this.#lose({ reason: "destroyed" });
  }

  pushErrorScope(): void {}

  popErrorScope(): Promise<null> {
    return Promise.resolve(null);
  }

  createTexture(descriptor: GPUTextureDescriptor) {
    this.recorder.note("createTexture", descriptor);
    const texture = this.#resource({
      ...descriptor,
      createView: (view: unknown) => ({ view, descriptor, texture }),
    });
    return texture;
  }

  createBuffer(descriptor: GPUBufferDescriptor) {
    this.recorder.note("createBuffer", descriptor);
    return this.#resource({ size: descriptor.size });
  }

  createShaderModule = () => ({});
  createBindGroupLayout = () => ({});
  createPipelineLayout = () => ({});
  createBindGroup = (descriptor: GPUBindGroupDescriptor) => ({ descriptor });
  createRenderPipeline = (descriptor: unknown) => ({ descriptor });
  createRenderPipelineAsync(descriptor: unknown): Promise<unknown> {
    return Promise.resolve({ descriptor });
  }

  createCommandEncoder() {
    const draws: FakeDraw[] = [];
    const reads = new Map<FakeDraw, FakeResource[]>();
    return {
      copyTextureToTexture: (...args: unknown[]) =>
        this.recorder.note("copyTextureToTexture", ...args),
      beginRenderPass: ({ colorAttachments }: GPURenderPassDescriptor) => {
        const [attachment] = [...colorAttachments];
        const loadOp = attachment?.loadOp ?? "";
        let buffer: unknown = null;
        let part = -1;
        let scissor: readonly number[] = [];
        const groups: FakeGroup[] = [];
        return {
          setPipeline: () => {},
          setBindGroup: (index: number, group: FakeGroup, offsets?: number[]) => {
            groups[index] = group;
            part = index === 1 ? (offsets?.[0] ?? -1) : part;
          },
          setVertexBuffer: (_: number, next: unknown) => {
            buffer = next;
          },
          setScissorRect: (...box: number[]) => {
            scissor = box;
          },
          draw: (_: number, count: number, __: number, first: number) => {
            const draw = { buffer, count, first, part, scissor, loadOp, isStale: false };
            draws.push(draw);
            reads.set(draw, [buffer as FakeResource, ...groups.flatMap(resourcesOf)]);
          },
          end: () => {},
        };
      },
      finish: () => ({ draws, reads }),
    };
  }

  #resource<T>(resource: T): T & FakeResource {
    this.live++;
    let gone = false;
    return Object.assign(resource as T & object, {
      isGone: () => gone,
      destroy: () => {
        this.live -= gone ? 0 : 1;
        gone = true;
      },
    });
  }
}

export class FakeCanvas extends EventTarget {
  width = 300;
  height = 150;
  readonly recorder = new Recorder();
  readonly context: Record<string, unknown>;
  removed = false;

  constructor() {
    super();
    const context: Record<string, unknown> = {
      canvas: this,
      font: "10px sans-serif",
      fillStyle: "#000000",
      measureText: (text: string) => {
        const font = String(context.font);
        const ink = measure.ink(font, text);
        this.recorder.note("measureText", text);
        return {
          width: measure.width(font, text),
          actualBoundingBoxLeft: ink.left,
          actualBoundingBoxRight: text.trim() === "" ? 0 : ink.right,
          actualBoundingBoxAscent: text.trim() === "" ? 0 : ink.ascent,
          actualBoundingBoxDescent: text.trim() === "" ? 0 : ink.descent,
          fontBoundingBoxAscent: measure.line(font).ascent,
          fontBoundingBoxDescent: measure.line(font).descent,
        };
      },
      fillText: (...args: unknown[]) =>
        this.recorder.note("fillText", ...args, context.font, context.fillStyle),
      getImageData: (_: number, __: number, width: number, height: number) => ({
        data: new Uint8ClampedArray(width * height * 4),
      }),
    };
    for (const name of [
      "beginPath",
      "clearRect",
      "clip",
      "closePath",
      "drawImage",
      "fill",
      "fillRect",
      "lineTo",
      "rect",
      "restore",
      "roundRect",
      "save",
      "stroke",
    ]) {
      context[name] = (...args: unknown[]) => this.recorder.note(name, ...args);
    }
    this.context = context;
  }

  setAttribute(): void {}

  remove(): void {
    this.removed = true;
  }

  getContext(kind: string) {
    return kind === "webgpu"
      ? {
          configure: () => {},
          unconfigure: () => this.recorder.note("unconfigure"),
          getCurrentTexture: () => ({ createView: () => ({}) }),
        }
      : this.context;
  }
}

/** Stands in for a browser with a GPU. Returns the canvases that are made. */
export function fakeBrowser(adapter: { isFallbackAdapter?: boolean } | null = {}): {
  canvases: FakeCanvas[];
  devices: FakeDevice[];
  media: EventTarget & { matches: boolean };
} {
  const canvases: FakeCanvas[] = [];
  const devices: FakeDevice[] = [];
  const media = Object.assign(new EventTarget(), { matches: false });
  vi.stubGlobal("document", {
    documentElement: { lang: "en" },
    createElement: () => {
      const canvas = new FakeCanvas();
      canvases.push(canvas);
      return canvas;
    },
  });
  vi.stubGlobal("matchMedia", () => media);
  vi.stubGlobal("window", {
    setTimeout: (callback: () => void, delay?: number) => setTimeout(callback, delay),
    requestIdleCallback: (callback: () => void) => setTimeout(callback, 0),
  });
  vi.stubGlobal("navigator", {
    gpu: {
      getPreferredCanvasFormat: () => "bgra8unorm",
      requestAdapter: () =>
        Promise.resolve(
          adapter && {
            info: { vendor: "acme", architecture: "fast", description: "", ...adapter },
            limits: { maxTextureDimension2D: 16_384 },
            requestDevice: () => {
              const device = new FakeDevice();
              devices.push(device);
              return Promise.resolve(device);
            },
          },
        ),
    },
  });
  return { canvases, devices, media };
}

/** A quad as the shader reads it. */
export function quadsOf(bytes: Uint8Array) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const quads = [];
  for (let at = 0; at < bytes.length; at += INSTANCE_BYTES) {
    const flags = view.getUint32(at + 20, true);
    quads.push({
      x: view.getInt32(at, true),
      y: view.getInt32(at + 4, true),
      width: view.getUint16(at + 8, true),
      height: view.getUint16(at + 10, true),
      u: view.getUint16(at + 12, true),
      v: view.getUint16(at + 14, true),
      color: view.getUint32(at + 16, true),
      row: flags & 0x7f,
      textured: (flags & 0x80) !== 0,
      layer: flags >>> 8,
    });
  }
  return quads;
}
