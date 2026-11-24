import { TEXTURE } from "./pipeline.ts";
import type { Raster } from "./raster.ts";

/** Sides of a layer of the atlas. */
export const LAYER_SIZE = 2048;
/** Layers are filled a strip of rows at a time: what is drawn into it is uploaded together. */
export const STRIP_HEIGHT = 512;
/** Four bytes a pixel make 64 MB of them. */
export const MAX_LAYERS = 4;
export const LAYER_BYTES = LAYER_SIZE * LAYER_SIZE * 4;

/** A picture in the atlas. */
export interface Sprite {
  layer: number;
  u: number;
  v: number;
  readonly width: number;
  readonly height: number;
}

interface Shelf {
  readonly y: number;
  readonly height: number;
  /** How far it is filled. */
  x: number;
}

/** Room that is wasted above a sprite in order to fill a shelf. */
const SLACK = 4;

/** Packs rectangles into rows of about their height. */
export class Shelves {
  readonly width: number;
  readonly height: number;
  #shelves: Shelf[] = [];
  #bottom = 0;

  constructor(width: number, height: number) {
    this.width = width;
    this.height = height;
  }

  clear(): void {
    this.#shelves = [];
    this.#bottom = 0;
  }

  place(width: number, height: number): { readonly x: number; readonly y: number } | null {
    let best: Shelf | null = null;
    for (const shelf of this.#shelves) {
      if (
        shelf.height >= height &&
        shelf.x + width <= this.width &&
        (!best || shelf.height < best.height)
      ) {
        best = shelf;
      }
    }
    if (!best || best.height > height + SLACK) {
      const tall = Math.min(Math.ceil(height / SLACK) * SLACK, this.height - this.#bottom);
      if (tall >= height && width <= this.width) {
        best = { y: this.#bottom, height: tall, x: 0 };
        this.#bottom += tall;
        this.#shelves.push(best);
      }
    }
    if (!best) {
      return null;
    }
    const place = { x: best.x, y: best.y };
    best.x += width;
    return place;
  }
}

/** Which strip sprites go into, and which layers make room for new ones. */
export class AtlasPlan {
  /** Layers there are. */
  layers = 1;
  /** The layer and the row the open strip is at. */
  layer = 0;
  top = 0;
  /** How often each layer has been emptied. Replaced as a whole when one is. */
  generations: readonly number[] = [0];
  /** Whatever is used now may not be emptied before later. */
  now = 1;
  #stamps = [0];
  #shelves: Shelves;
  #size: number;
  #strip: number;
  #most: number;

  constructor(size = LAYER_SIZE, strip = STRIP_HEIGHT, most = MAX_LAYERS) {
    this.#size = size;
    this.#strip = strip;
    this.#most = most;
    this.#shelves = new Shelves(size, strip);
  }

  /** Room in the open strip, in pixels from its corner. */
  place(width: number, height: number): { readonly x: number; readonly y: number } | null {
    return this.#shelves.place(width, height);
  }

  /**
   * Opens another strip: the next one of the layer, or the first one of a new
   * layer, or of the one that has not been used for longest. Returns the layer
   * that was emptied for it or -1, or `null` when all layers are in use.
   */
  advance(): number | null {
    if (this.top + 2 * this.#strip <= this.#size) {
      this.top += this.#strip;
      this.#shelves.clear();
      return -1;
    }
    let emptied = -1;
    if (this.layers < this.#most) {
      this.layer = this.layers++;
      this.generations = [...this.generations, 0];
      this.#stamps.push(0);
    } else {
      for (const [layer, stamp] of this.#stamps.entries()) {
        const oldest = this.#stamps[emptied] ?? this.now;
        if (stamp < oldest) {
          emptied = layer;
        }
      }
      if (emptied < 0) {
        return null;
      }
      this.layer = emptied;
      this.generations = this.generations.map((generation, layer) =>
        layer === emptied ? generation + 1 : generation,
      );
    }
    this.top = 0;
    this.#shelves.clear();
    return emptied;
  }

  /** Notes that some layers, as bits of a number, are used now. */
  use(layers: number): void {
    for (let layer = 0; layer < this.#stamps.length; layer++) {
      if (layers & (1 << layer)) {
        this.#stamps[layer] = this.now;
      }
    }
  }

  /** Whether some layers hold what they did at the time of the generations. */
  holds(layers: number, generations: readonly number[]): boolean {
    if (generations === this.generations) {
      return true;
    }
    for (let layer = 0; layer < generations.length; layer++) {
      if (layers & (1 << layer) && generations[layer] !== this.generations[layer]) {
        return false;
      }
    }
    return true;
  }
}

interface Box {
  readonly left: number;
  readonly top: number;
  right: number;
  bottom: number;
}

/** The texture that text and pictures are copied from, shared by the grids of a page. */
export class Atlas {
  readonly plan = new AtlasPlan();
  readonly raster: Raster;
  view: GPUTextureView;
  /** Changes when there is another texture. */
  version = 0;
  /** Set while all that is asked for has to be put in, at whatever cost. */
  unlimited = false;
  /** Set when there was no room because everything is in use. */
  full = false;
  uploads = 0;
  uploadedBytes = 0;
  evictions = 0;
  #device: GPUDevice;
  #guard: <T>(allocate: () => T) => T;
  #texture: GPUTexture;
  #onEvict: (layer: number) => void;
  /** What has been drawn into the open strip and not uploaded: a box for each shelf. */
  #dirty: Box[] = [];

  constructor(
    device: GPUDevice,
    raster: Raster,
    guard: <T>(allocate: () => T) => T,
    onEvict: (layer: number) => void,
  ) {
    this.#device = device;
    this.raster = raster;
    this.#guard = guard;
    this.#onEvict = onEvict;
    this.#texture = this.#create(1);
    this.view = this.#viewOf(this.#texture);
  }

  /** Bytes on the GPU. */
  get size(): number {
    return this.plan.layers * LAYER_BYTES;
  }

  /**
   * Room for a sprite, as a place in the open strip. There is none when this
   * frame has filled a strip already, so that it is uploaded once, and when
   * everything is in use.
   */
  reserve(width: number, height: number): { readonly x: number; readonly y: number } | null {
    let place = this.plan.place(width, height);
    if (!place) {
      if (this.#dirty.length > 0 && !this.unlimited) {
        return null;
      }
      this.flush();
      const { layers } = this.plan;
      const emptied = this.plan.advance();
      if (emptied === null) {
        this.full = true;
        return null;
      }
      if (emptied >= 0) {
        this.evictions++;
        this.#onEvict(emptied);
      }
      if (this.plan.layers > layers) {
        this.#grow(layers);
      }
      this.raster.clear();
      place = this.plan.place(width, height);
    }
    if (place) {
      const { x, y } = place;
      const box = this.#dirty.find((other) => other.top === y);
      if (box) {
        box.right = Math.max(box.right, x + width);
        box.bottom = Math.max(box.bottom, y + height);
      } else {
        this.#dirty.push({ left: x, top: y, right: x + width, bottom: y + height });
      }
    }
    return place;
  }

  /** Uploads what has been drawn since the last time. */
  flush(): void {
    let boxes = this.#dirty;
    if (boxes.length === 0) {
      return;
    }
    if (this.raster.kind === "copy") {
      // Every copy costs about the same, however large it is.
      boxes = [
        {
          left: Math.min(...boxes.map(({ left }) => left)),
          top: Math.min(...boxes.map(({ top }) => top)),
          right: Math.max(...boxes.map(({ right }) => right)),
          bottom: Math.max(...boxes.map(({ bottom }) => bottom)),
        },
      ];
    }
    for (const { left, top, right, bottom } of boxes) {
      const width = right - left;
      const height = bottom - top;
      this.raster.upload(this.#texture, left, top, width, height, {
        x: left,
        y: this.plan.top + top,
        z: this.plan.layer,
      });
      this.uploadedBytes += width * height * 4;
    }
    this.uploads++;
    this.#dirty = [];
  }

  destroy(): void {
    this.#texture.destroy();
  }

  /** Takes the place of the texture with one that has another layer. */
  #grow(layers: number): void {
    const texture = this.#create(this.plan.layers);
    const encoder = this.#device.createCommandEncoder();
    encoder.copyTextureToTexture({ texture: this.#texture }, { texture }, [
      LAYER_SIZE,
      LAYER_SIZE,
      layers,
    ]);
    this.#device.queue.submit([encoder.finish()]);
    this.#texture.destroy();
    this.#texture = texture;
    this.view = this.#viewOf(texture);
    this.version++;
  }

  #create(layers: number): GPUTexture {
    return this.#guard(() =>
      this.#device.createTexture({
        size: [LAYER_SIZE, LAYER_SIZE, layers],
        format: "rgba8unorm",
        usage: TEXTURE.binding | TEXTURE.copySrc | TEXTURE.copyDst | TEXTURE.attachment,
      }),
    );
  }

  /** Also while there is only one layer, the shader is given an array of them. */
  #viewOf(texture: GPUTexture): GPUTextureView {
    return texture.createView({ dimension: "2d-array" });
  }
}
