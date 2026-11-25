import { FLAG_NUMBER, PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import type { Scroll } from "../../layout.ts";
import { Colors, serializer } from "../color.ts";
import type { Frame, FrameCounts } from "../frame.ts";
import type { Look } from "../look.ts";
import { BAND_ROWS, edgeOf, type PageLayout } from "../page-layout.ts";
import type { PixelBox } from "../rects.ts";
import { bandsOf, type PageStep, type Scene, SceneBuilder, type SceneStep } from "../scene.ts";
import { type TextMeasurer, textContext } from "../text-measure.ts";
import type { GridRenderer, RendererCounters } from "../types.ts";
import type { Gpu, Lease } from "./device.ts";
import { ANY_ROW, FrameBuffer } from "./frame-buffer.ts";
import { faint } from "./glyphs.ts";
import { type BandState, type Block, type PageView, RetainedPages } from "./pages.ts";

/**
 * Milliseconds a frame may spend on making texts: laying them out, packing and
 * drawing them. They leave time for the rest of a frame at 60 Hz, and are enough
 * for another screen of texts whose characters have been drawn before.
 */
const BUDGET = 12;
/** And while the sheet is being zoomed, when the next frame may have no use for them. */
const ZOOM_BUDGET = 5;
/** As many more may go into the headers, which are there for the first time in many of those. */
const HEADER_BUDGET = 2;

/** Some quads to draw: the frame's own, or those of a page. */
interface Call {
  readonly block: Block["chunk"] | null;
  readonly first: number;
  count: number;
  /** Where they are: an offset into the frame's parts. */
  readonly part: number;
  readonly clip: PixelBox | null;
}

/** A band of a page that is or may come into view, and which of its columns of pixels will. */
interface BandInView {
  readonly layout: PageLayout;
  readonly index: number;
  readonly view: PageView;
}

function viewOf({ x, clip }: PageStep): PageView {
  return { origin: x, left: clip.left - x, right: clip.right - x };
}

/** Draws scenes into whatever a view is of. */
export class Painter {
  readonly pages: RetainedPages;
  frames = 0;
  extraSubmits = 0;
  drawCalls = 0;
  quads = 0;
  deferredFrames = 0;
  #gpu: Gpu;
  #format: GPUTextureFormat;
  #buffers: FrameBuffer;
  #group: GPUBindGroup | null = null;
  #groupVersion = -1;
  #calls: Call[] = [];
  /** Where the frame's own quads are that no call draws yet. */
  #mark = 0;
  #clipped = 0;
  /** Whether no bars stand for texts. */
  #complete = true;
  /** Whether the last frame showed more than the atlas can hold at a time. */
  #crowded = false;
  /** Whether a part of this frame has been submitted already. */
  #started = false;
  /** What the last frame was drawn with. */
  #look: Look | null = null;
  #budget = BUDGET;

  constructor(gpu: Gpu, format: GPUTextureFormat) {
    this.#gpu = gpu;
    this.#format = format;
    this.#buffers = new FrameBuffer(gpu.device, gpu.pipelines.partLayout);
    this.pages = new RetainedPages(gpu.device, gpu.glyphs, gpu.guard);
  }

  get bytes(): number {
    return this.#buffers.size + this.pages.bytes;
  }

  /** Texts of the last frame that were drawn through a clip of their own. */
  get clipped(): number {
    return this.#clipped;
  }

  /**
   * Draws a scene, or with no view only gets ready to. Returns whether all of
   * its texts are there, so that the next frame need not complete it. If nothing
   * new is in view, some of what is ahead gets ready.
   */
  paint(
    frame: Frame,
    scene: Scene,
    view: GPUTextureView | null,
    ahead: Iterable<BandInView> = [],
  ): boolean {
    const { glyphs } = this.#gpu;
    const { bandsBuilt } = this.pages;
    this.pages.begin(frame.cache);
    this.drawCalls = 0;
    this.quads = 0;
    const isZoomed = this.#look !== scene.look && this.#look?.theme === scene.look.theme;
    this.#budget = isZoomed ? ZOOM_BUDGET : BUDGET;
    this.#look = scene.look;
    const hasRoom = () => !this.#crowded && this.#compose(scene, false, view);
    if (!hasRoom() && !(this.pages.dropStale() && hasRoom())) {
      // Within the budget, bars would never go away: every frame, some texts
      // would make room for others that are in view as well.
      this.#crowded = !this.#compose(scene, true, view);
    }
    const complete = this.#complete || this.#crowded;
    if (complete && this.pages.bandsBuilt === bandsBuilt) {
      this.#prebuild(scene, ahead);
    }
    if (view) {
      this.#submit(scene, view, false);
      this.frames++;
    } else {
      glyphs.atlas.flush();
      this.pages.flush();
    }
    if (!complete) {
      this.deferredFrames++;
    }
    return complete;
  }

  destroy(): void {
    this.#buffers.destroy();
    this.pages.destroy();
  }

  /**
   * Makes calls of a scene's steps. When everything in the atlas is in use,
   * what there is so far is drawn and what it needed may make room; but not
   * unless `crowded`: then it returns `false`.
   */
  #compose(scene: Scene, crowded: boolean, view: GPUTextureView | null): boolean {
    const { glyphs } = this.#gpu;
    const { plan } = glyphs.atlas;
    let roomy = true;
    plan.now++;
    glyphs.atlas.full = false;
    glyphs.begin(crowded ? Number.POSITIVE_INFINITY : this.#budget);
    this.#buffers.clear();
    this.#calls = [];
    this.#mark = 0;
    this.#clipped = 0;
    this.#complete = true;
    // None of what is there already may be emptied for what is not.
    const lacking: Record<BandState, BandInView[]> = { ready: [], stale: [], missing: [] };
    for (const step of scene.steps) {
      if (step.kind === "texts") {
        const { first, end } = bandsOf(step);
        const { layout } = step;
        const pageView = viewOf(step);
        for (let index = first; index < end; index++) {
          const state = this.pages.stateOf(scene.look, layout, index, pageView);
          if (state !== "ready") {
            lacking[state].push({ layout, index, view: pageView });
          }
        }
      }
    }
    if (!crowded) {
      for (const { layout, index, view } of [...lacking.missing, ...lacking.stale]) {
        if (!glyphs.hasTime) {
          break;
        }
        this.pages.make(scene.look, layout, index, view);
        if (glyphs.atlas.full) {
          return false;
        }
      }
      glyphs.allow(HEADER_BUDGET);
    }
    for (const step of scene.steps) {
      if (step.kind === "texts") {
        let part = this.#part(step);
        const { first, end } = bandsOf(step);
        const pageView = viewOf(step);
        for (let index = first; index < end; index++) {
          if (crowded) {
            const make = () => this.pages.make(scene.look, step.layout, index, pageView);
            make();
            if (glyphs.atlas.full) {
              roomy = false;
              this.#makeRoom(scene, view);
              part = this.#part(step);
              make();
              glyphs.atlas.full = false;
            }
          }
          this.#band(scene.look, step, index, pageView, part);
        }
      } else if (!this.#step(scene, step)) {
        if (!crowded) {
          return false;
        }
        roomy = false;
        this.#makeRoom(scene, view);
        this.#step(scene, step);
        glyphs.atlas.full = false;
      }
    }
    return roomy;
  }

  /** Makes calls of a band, or of what there is in its place. */
  #band(look: Look, step: PageStep, index: number, view: PageView, part: number): void {
    const { layout, x, y, firstRow, endRow } = step;
    const band = this.pages.shown(look, layout, index, view);
    if (!band) {
      this.#complete = false;
      this.#bars(look, step, index);
      return;
    }
    let place = part;
    if (band.layout !== layout) {
      // Where it would be if all that is in it were enlarged with the sheet.
      const { geometry: from } = band.layout;
      const { geometry: to } = layout;
      const top = index * BAND_ROWS;
      const bottom = Math.min(layout.page.rowCount, top + BAND_ROWS);
      const ratio = (now: number, before: number) => (before > 0 ? now / before : 1);
      const at = ({ ys }: typeof to, row: number) => ys[row] ?? 0;
      const across = ratio(to.xs.at(-1) ?? 0, from.xs.at(-1) ?? 0);
      const down = ratio(at(to, bottom) - at(to, top), at(from, bottom) - at(from, top));
      const start = y + at(to, top) - at(from, top) * down;
      place = this.#buffers.part(x, start, firstRow, endRow, across, down);
    }
    this.#clipped += band.clipped;
    this.#complete &&= band.complete && band.layout === layout;
    this.#call(band.block, place, step.clip);
  }

  /** Bars about where a band's texts will be, of which nothing has been laid out yet. */
  #bars(look: Look, { layout, x, y, clip, firstRow, endRow }: PageStep, index: number): void {
    const { page, geometry } = layout;
    const { xs, ys } = geometry;
    const { instances } = this.#buffers;
    const color = faint(look.text);
    const { padding } = look;
    const end = Math.min(endRow, (index + 1) * BAND_ROWS);
    for (let row = Math.max(firstRow, index * BAND_ROWS); row < end; row++) {
      const height = (ys[row + 1] ?? 0) - (ys[row] ?? 0);
      const thickness = Math.ceil(height / 4);
      const top = y + (ys[row] ?? 0) + ((height - thickness) >> 1);
      for (let col = 0; col < page.colCount; col++) {
        const cell = row * page.colCount + col;
        const left = (xs[col] ?? 0) + padding;
        const right = (xs[col + 1] ?? 0) - padding;
        const { length } = page.texts[cell] ?? "";
        if (length === 0 || left >= right) {
          continue;
        }
        const width = Math.min(right - left, Math.ceil((length * height) / 3));
        const isNumber = ((page.flags[cell] ?? 0) & FLAG_NUMBER) !== 0;
        const start = x + (isNumber ? right - width : left);
        const from = Math.max(clip.left, start);
        const to = Math.min(clip.right, start + width);
        const above = Math.max(clip.top, top);
        const below = Math.min(clip.bottom, top + thickness);
        if (from < to && above < below) {
          instances.rect(from, above, to - from, below - above, color);
        }
      }
    }
  }

  /** Makes bands that are not in view yet, as many of them as are of one page. */
  #prebuild(scene: Scene, ahead: Iterable<BandInView>): void {
    let built: PageLayout | null = null;
    for (const { layout, index, view } of ahead) {
      const isReady =
        built === null && this.pages.stateOf(scene.look, layout, index, view) === "ready";
      if (built ? layout !== built : isReady) {
        if (built) {
          return;
        }
        continue;
      }
      if (!this.#gpu.glyphs.hasTime) {
        return;
      }
      built = layout;
      this.pages.make(scene.look, layout, index, view);
      if (this.#gpu.glyphs.atlas.full) {
        return;
      }
    }
  }

  /** Makes calls of a step without pages' texts; `false` if the atlas is full. */
  #step(scene: Scene, step: Exclude<SceneStep, { kind: "texts" }>): boolean {
    const { glyphs } = this.#gpu;
    const { instances } = this.#buffers;
    const { length } = instances;
    const { deferred } = glyphs;
    glyphs.layers = 0;
    switch (step.kind) {
      case "rects":
        instances.rects(scene.rects, step.first, step.end);
        break;
      case "fills":
      case "borders":
        this.#call(
          this.pages.rects(scene.look, step.layout, step.kind),
          this.#part(step),
          step.clip,
        );
        break;
      case "labels":
        this.#labels(scene, step.runs);
        break;
      case "sprite": {
        const sprite = glyphs.picture(step.spec);
        if (sprite) {
          instances.sprite(sprite, step.x, step.y, step.clip);
        } else {
          this.#complete = false;
        }
        break;
      }
    }
    this.#complete &&= glyphs.deferred === deferred;
    glyphs.atlas.plan.use(glyphs.layers);
    if (glyphs.atlas.full) {
      instances.length = length;
      return false;
    }
    return true;
  }

  #labels(scene: Scene, runs: Extract<SceneStep, { kind: "labels" }>["runs"]): void {
    for (let run = 0; run < runs.length; run++) {
      const at = run * 4;
      const clip =
        runs.clipped[run] === 1
          ? {
              left: runs.clip[at] ?? 0,
              top: runs.clip[at + 1] ?? 0,
              right: runs.clip[at + 2] ?? 0,
              bottom: runs.clip[at + 3] ?? 0,
            }
          : null;
      this.#clipped += clip ? 1 : 0;
      this.#gpu.glyphs.run(
        this.#buffers.instances,
        scene.look.fonts[runs.font[run] ?? 0] ?? "",
        runs.color[run] ?? 0,
        runs.texts[run] ?? "",
        {
          origin: 0,
          x: runs.x[run] ?? 0,
          baseline: runs.baseline[run] ?? 0,
          width: runs.width[run] ?? 0,
          clip,
          row: ANY_ROW,
          left: 0,
          right: scene.width,
        },
      );
    }
  }

  #part({ x, y, firstRow, endRow }: PageStep): number {
    return this.#buffers.part(x, y, firstRow, endRow);
  }

  /** The frame's own quads so far come before whatever is called next. */
  #flushOwn(): void {
    const { length } = this.#buffers.instances;
    if (length > this.#mark) {
      this.#calls.push({
        block: null,
        first: this.#mark,
        count: length - this.#mark,
        part: 0,
        clip: null,
      });
      this.#mark = length;
    }
  }

  #call(block: Block | null, part: number, clip: PixelBox): void {
    if (!block) {
      return;
    }
    this.#flushOwn();
    const last = this.#calls.at(-1);
    if (
      last?.block === block.chunk &&
      last.part === part &&
      last.first + last.count === block.first
    ) {
      last.count += block.count;
    } else {
      const { chunk, first, count } = block;
      this.#calls.push({ block: chunk, first, count, part, clip });
    }
  }

  /** Draws what there is, so that what it was copied from can be emptied. */
  #makeRoom(scene: Scene, view: GPUTextureView | null): void {
    const { plan } = this.#gpu.glyphs.atlas;
    if (view) {
      this.#submit(scene, view, true);
      this.extraSubmits++;
    }
    this.#buffers.clear();
    this.#calls = [];
    this.#mark = 0;
    plan.now++;
    this.#gpu.glyphs.atlas.full = false;
  }

  #submit(scene: Scene, view: GPUTextureView, more: boolean): void {
    const { device, glyphs, pipelines } = this.#gpu;
    const { width, height } = scene;
    this.#flushOwn();
    glyphs.atlas.flush();
    this.pages.flush();
    this.#buffers.upload(width, height);
    if (!this.#group || this.#groupVersion !== glyphs.atlas.version) {
      this.#groupVersion = glyphs.atlas.version;
      this.#group = device.createBindGroup({
        layout: pipelines.screenLayout,
        entries: [
          { binding: 0, resource: { buffer: this.#buffers.screen } },
          { binding: 1, resource: glyphs.atlas.view },
          { binding: 2, resource: pipelines.sampler },
        ],
      });
    }
    const encoder = device.createCommandEncoder();
    const pass = encoder.beginRenderPass({
      colorAttachments: [
        {
          view,
          loadOp: this.#started ? "load" : "clear",
          storeOp: "store",
          clearValue: [0, 0, 0, 0],
        },
      ],
    });
    this.#started = more;
    pass.setPipeline(pipelines.get(this.#format));
    pass.setBindGroup(0, this.#group);
    const { vertices, group } = this.#buffers;
    let block: Call["block"] | undefined;
    let part = -1;
    let clip: PixelBox | null = null;
    for (const call of this.#calls) {
      if (call.block !== block) {
        block = call.block;
        pass.setVertexBuffer(0, block ? block.buffer : vertices);
      }
      if (call.part !== part) {
        part = call.part;
        pass.setBindGroup(1, group, [part]);
      }
      if (call.clip !== clip) {
        clip = call.clip;
        const left = Math.min(width, Math.max(0, clip?.left ?? 0));
        const top = Math.min(height, Math.max(0, clip?.top ?? 0));
        const right = Math.min(width, Math.max(left, clip?.right ?? width));
        const bottom = Math.min(height, Math.max(top, clip?.bottom ?? height));
        pass.setScissorRect(left, top, right - left, bottom - top);
      }
      pass.draw(4, call.count, 0, call.first);
      this.drawCalls++;
      this.quads += call.count;
    }
    pass.end();
    device.queue.submit([encoder.finish()]);
  }
}

export interface WebGpuOptions {
  /** Shared with whoever else measures the sheet's text. */
  readonly measure: TextMeasurer;
  /** Called when the next frame would show what this one had no time for. */
  readonly onIncomplete: () => void;
}

/** Draws frames on the GPU, the way `Canvas2dRenderer` does pixel for pixel. */
export class WebGpuRenderer implements GridRenderer {
  readonly kind = "webgpu";
  readonly canvas: HTMLCanvasElement;
  #gpu: Gpu;
  #release: () => void;
  #options: WebGpuOptions;
  #context: GPUCanvasContext;
  #scenes: SceneBuilder;
  #painter: Painter;
  #scroll: Scroll = { top: 0, left: 0 };

  constructor({ gpu, release }: Lease, options: WebGpuOptions) {
    this.#gpu = gpu;
    this.#release = release;
    this.#options = options;
    this.canvas = document.createElement("canvas");
    const context = this.canvas.getContext("webgpu") as unknown as GPUCanvasContext | null;
    if (!context) {
      throw new Error("this browser cannot draw on a canvas with WebGPU");
    }
    this.#context = context;
    const format = navigator.gpu.getPreferredCanvasFormat();
    context.configure({ device: gpu.device, format, alphaMode: "opaque" });
    this.#painter = new Painter(gpu, format);
    const colors = new Colors(serializer(textContext(document.createElement("canvas"))));
    this.#scenes = new SceneBuilder(options.measure, colors);
  }

  get counters(): RendererCounters {
    const { atlas, hits, misses } = this.#gpu.glyphs;
    const painter = this.#painter;
    return {
      frames: painter.frames,
      extraSubmits: painter.extraSubmits,
      drawCalls: painter.drawCalls,
      quads: painter.quads,
      pageUploads: painter.pages.uploads,
      pageQuads: painter.pages.uploadedQuads,
      bandsBuilt: painter.pages.bandsBuilt,
      atlasHits: hits,
      atlasMisses: misses,
      atlasUploads: atlas.uploads,
      atlasUploadedBytes: atlas.uploadedBytes,
      atlasLayers: atlas.plan.layers,
      atlasEvictions: atlas.evictions,
      deferredFrames: painter.deferredFrames,
      bytes: painter.bytes + atlas.size,
      raster: atlas.raster.kind,
    };
  }

  resize(width: number, height: number): void {
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
    }
  }

  draw(frame: Frame): FrameCounts {
    const scene = this.#scenes.build(frame);
    if (!this.#gpu.gone) {
      const view = this.#context.getCurrentTexture().createView();
      if (!this.#painter.paint(frame, scene, view, this.#ahead(frame))) {
        this.#options.onIncomplete();
      }
    }
    this.#scroll = frame.scroll;
    return { cells: scene.cells, clipped: this.#painter.clipped, missing: scene.missing };
  }

  /**
   * Does a frame's share of what drawing one for the first time takes, without
   * showing anything. Returns whether nothing is left to do.
   */
  warm(frame: Frame): boolean {
    const scene = this.#scenes.build(frame);
    for (const font of scene.look.fonts) {
      this.#gpu.glyphs.prepare(font);
    }
    return this.#painter.paint(frame, scene, null);
  }

  /** What comes into view next if the sheet is scrolled on. */
  *#ahead(frame: Frame): Generator<BandInView> {
    const { visible, scroll, scale, cache, layout: sheet } = frame;
    const down = Math.sign(scroll.top - this.#scroll.top);
    const across = Math.sign(scroll.left - this.#scroll.left);
    let { firstRow, endRow } = visible;
    let firstCol = Math.floor(visible.firstCol / PAGE_COLS);
    let endCol = Math.ceil(visible.endCol / PAGE_COLS);
    if (down !== 0) {
      firstRow = down > 0 ? visible.endRow : Math.max(0, visible.firstRow - PAGE_ROWS);
      endRow = down > 0 ? visible.endRow + PAGE_ROWS : visible.firstRow;
    } else if (across !== 0) {
      firstCol = across > 0 ? endCol : firstCol - 1;
      endCol = firstCol + 1;
    } else {
      return;
    }
    const screenLeft =
      Math.round(frame.metrics.headerWidth * scale) - Math.round(scroll.left * scale);
    const firstPage = Math.floor(firstRow / PAGE_ROWS);
    const pages = Math.ceil(endRow / PAGE_ROWS) - firstPage;
    for (let nth = 0; nth < pages; nth++) {
      const pageRow = firstPage + (down < 0 ? pages - 1 - nth : nth);
      for (let pageCol = firstCol; pageCol < endCol; pageCol++) {
        const page = cache.get(pageRow, pageCol);
        const layout = page && this.#scenes.layoutOf(frame, page);
        if (!layout) {
          continue;
        }
        const { xs } = layout.geometry;
        const edge = (col: number) =>
          xs[Math.min(page.colCount, Math.max(0, col - page.firstCol))] ?? 0;
        const left = down === 0 ? 0 : edge(visible.firstCol);
        const right = down === 0 ? edge(page.firstCol + page.colCount) : edge(visible.endCol);
        const origin = screenLeft + (sheet ? edgeOf(sheet.colAxis, page.firstCol, scale) : 0);
        const view = { origin, left, right };
        const first = Math.max(0, firstRow - page.firstRow);
        const end = Math.min(page.rowCount, endRow - page.firstRow);
        for (let index = Math.floor(first / BAND_ROWS); index * BAND_ROWS < end; index++) {
          yield { layout, index, view };
        }
      }
    }
  }

  destroy(): void {
    this.canvas.remove();
    if (!this.#gpu.gone) {
      this.#context.unconfigure();
    }
    this.#painter.destroy();
    this.#release();
  }
}
