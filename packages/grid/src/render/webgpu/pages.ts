import { PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import type { PageCache } from "../../cache.ts";
import type { Page } from "../../page.ts";
import type { Look } from "../look.ts";
import type { PageLayout } from "../page-layout.ts";
import type { PixelBox } from "../rects.ts";
import { InstanceWriter } from "./frame-buffer.ts";
import type { Glyphs } from "./glyphs.ts";
import { BUFFER, INSTANCE_BYTES } from "./pipeline.ts";

/** Quads of a buffer, unless a text needs more of them than that. */
const CHUNK = 1 << 16;
/** Bytes of quads that are kept of pages out of view. */
const MOST_KEPT = 48 << 20;

/** Hands out stretches of a range, and takes them back. */
export class Stretches {
  readonly length: number;
  /** Where the free ones start and end, in order, none touching the next. */
  #free: { start: number; end: number }[];

  constructor(length: number) {
    this.length = length;
    this.#free = [{ start: 0, end: length }];
  }

  get isEmpty(): boolean {
    const [first] = this.#free;
    return first !== undefined && first.start === 0 && first.end === this.length;
  }

  /** The start of a stretch, or `null` if there is none that long. */
  take(length: number): number | null {
    const index = this.#free.findIndex(({ start, end }) => end - start >= length);
    const free = this.#free[index];
    if (!free) {
      return null;
    }
    const { start } = free;
    free.start += length;
    if (free.start === free.end) {
      this.#free.splice(index, 1);
    }
    return start;
  }

  give(start: number, length: number): void {
    const end = start + length;
    let index = this.#free.findIndex((free) => free.start >= start);
    if (index < 0) {
      index = this.#free.length;
    }
    const before = this.#free[index - 1];
    const after = this.#free[index];
    const joinsBefore = before?.end === start;
    const joinsAfter = after?.start === end;
    if (before && joinsBefore) {
      before.end = after && joinsAfter ? after.end : end;
      if (joinsAfter) {
        this.#free.splice(index, 1);
      }
    } else if (after && joinsAfter) {
      after.start = start;
    } else {
      this.#free.splice(index, 0, { start, end });
    }
  }
}

interface Chunk {
  readonly buffer: GPUBuffer;
  readonly stretches: Stretches;
}

/** Quads that stay on the GPU. */
export interface Block {
  readonly chunk: Chunk;
  readonly first: number;
  readonly count: number;
}

/** The texts of some rows of a page as quads. */
interface Band {
  readonly block: Block | null;
  /** Layers of the atlas they copy from, and what generation those were of. */
  readonly layers: number;
  readonly generations: readonly number[];
  /** Whether bars stand for some of them. */
  readonly complete: boolean;
  /** What may be in view for nothing to be missing, from the page's origin. */
  readonly left: number;
  readonly right: number;
  readonly clipped: number;
}

/** Where a page is, and which of its columns of pixels are in view. */
export interface PageView {
  /** Where its origin is on the screen. */
  readonly origin: number;
  readonly left: number;
  readonly right: number;
}

interface Retained {
  readonly page: Page;
  readonly layout: PageLayout;
  fills: Block | null | undefined;
  borders: Block | null | undefined;
  readonly bands: (Band | undefined)[];
  /** Number of the frame it was last part of. */
  used: number;
}

/**
 * The quads of the pages in view and around it. A page has its own for as long
 * as it has its layout, which is as long as it looks the same: wherever it is
 * scrolled to, nothing is uploaded.
 */
export class RetainedPages {
  /** Uploads, and how many quads they had. */
  uploads = 0;
  uploadedQuads = 0;
  bandsBuilt = 0;
  #device: GPUDevice;
  #glyphs: Glyphs;
  #guard: <T>(allocate: () => T) => T;
  #chunks: Chunk[] = [];
  #pages = new Map<Page, Retained>();
  #writer = new InstanceWriter();
  #kept = 0;
  #frame = 0;
  /** Not needed any more, but for what has yet to be submitted. */
  #discarded: Block[] = [];

  constructor(device: GPUDevice, glyphs: Glyphs, guard: <T>(allocate: () => T) => T) {
    this.#device = device;
    this.#glyphs = glyphs;
    this.#guard = guard;
  }

  /** Pages that have quads. */
  get size(): number {
    return this.#pages.size;
  }

  /** Bytes on the GPU. */
  get bytes(): number {
    return this.#chunks.reduce((sum, { buffer }) => sum + buffer.size, 0);
  }

  /** Starts a frame: lets go of what is not held any more, or too much. */
  begin(cache: PageCache): void {
    this.#frame++;
    for (const block of this.#discarded) {
      this.#free(block);
    }
    this.#discarded = [];
    for (const [page, retained] of this.#pages) {
      const held = cache.get(page.firstRow / PAGE_ROWS, page.firstCol / PAGE_COLS);
      if (held !== page) {
        this.#drop(retained);
      }
    }
    if (this.#kept > MOST_KEPT) {
      const unused = [...this.#pages.values()]
        .filter(({ used }) => used < this.#frame - 1)
        .sort((a, b) => a.used - b.used);
      let excess = this.#kept - MOST_KEPT;
      for (const retained of unused) {
        if (excess <= 0) {
          break;
        }
        excess -= this.#drop(retained);
      }
    }
  }

  rects(layout: PageLayout, kind: "fills" | "borders"): Block | null {
    const retained = this.#retained(layout);
    let block = retained[kind];
    if (block === undefined) {
      const list = layout[kind];
      this.#writer.length = 0;
      this.#writer.rects(list, 0, list.length);
      block = this.#store();
      retained[kind] = block;
    }
    return block;
  }

  /** Whether a band is to be had without building it. */
  has(layout: PageLayout, index: number, view: PageView): boolean {
    return this.#found(layout, index, view) !== null;
  }

  /** If there is a band, notes that what it copies from must not make room for others. */
  keep(layout: PageLayout, index: number, view: PageView): void {
    const band = this.#found(layout, index, view);
    if (band) {
      this.#glyphs.atlas.plan.use(band.layers);
    }
  }

  /** The texts of a band. */
  band(look: Look, layout: PageLayout, index: number, view: PageView): Band {
    const retained = this.#retained(layout);
    let band = retained.bands[index];
    if (!band || !(this.#holds(band, view) && (band.complete || !this.#glyphs.hasTime))) {
      this.#discard(band?.block);
      band = this.#build(look, layout, index, view, band);
      retained.bands[index] = band;
    }
    this.#glyphs.atlas.plan.use(band.layers);
    return band;
  }

  destroy(): void {
    for (const { buffer } of this.#chunks) {
      buffer.destroy();
    }
    this.#chunks = [];
    this.#pages.clear();
  }

  #found(layout: PageLayout, index: number, view: PageView): Band | null {
    const retained = this.#pages.get(layout.page);
    const band = retained?.layout === layout ? retained.bands[index] : undefined;
    return band?.complete && this.#holds(band, view) ? band : null;
  }

  #holds(band: Band, { left, right }: PageView): boolean {
    return (
      band.left <= left &&
      band.right >= right &&
      this.#glyphs.atlas.plan.holds(band.layers, band.generations)
    );
  }

  #build(
    look: Look,
    layout: PageLayout,
    index: number,
    { origin, left, right }: PageView,
    before: Band | undefined,
  ): Band {
    const { first, end, firstStrike, endStrike } = layout.band(index);
    const runs = layout.texts;
    const glyphs = this.#glyphs;
    const writer = this.#writer;
    writer.length = 0;
    glyphs.layers = 0;
    glyphs.partial = false;
    const { deferred } = glyphs;
    // More than is in view, or scrolling sideways would build it again all the
    // time; and what was in view before, which another part of the frame may show.
    const spare = right - left;
    let from = left - spare;
    let to = right + spare;
    if (before && before.left > from - 2 * spare && before.right < to + 2 * spare) {
      from = Math.min(from, before.left);
      to = Math.max(to, before.right);
    }
    let clipped = 0;
    for (let run = first; run < end; run++) {
      let clip: PixelBox | null = null;
      if (runs.clipped[run] === 1) {
        const at = run * 4;
        clip = {
          left: runs.clip[at] ?? 0,
          top: runs.clip[at + 1] ?? 0,
          right: runs.clip[at + 2] ?? 0,
          bottom: runs.clip[at + 3] ?? 0,
        };
        clipped++;
      }
      glyphs.run(
        writer,
        look.fonts[runs.font[run] ?? 0] ?? "",
        runs.color[run] ?? 0,
        runs.texts[run] ?? "",
        {
          origin,
          x: runs.x[run] ?? 0,
          baseline: runs.baseline[run] ?? 0,
          width: runs.width[run] ?? 0,
          clip,
          row: runs.row[run] ?? 0,
          left: from,
          right: to,
        },
      );
    }
    writer.rects(runs.strikes, firstStrike, endStrike);
    this.bandsBuilt++;
    return {
      block: this.#store(),
      layers: glyphs.layers,
      generations: glyphs.atlas.plan.generations,
      complete: glyphs.deferred === deferred,
      left: glyphs.partial ? from : Number.NEGATIVE_INFINITY,
      right: glyphs.partial ? to : Number.POSITIVE_INFINITY,
      clipped,
    };
  }

  #retained(layout: PageLayout): Retained {
    const { page } = layout;
    let retained = this.#pages.get(page);
    if (retained && retained.layout !== layout) {
      this.#drop(retained);
      retained = undefined;
    }
    if (!retained) {
      retained = { page, layout, fills: undefined, borders: undefined, bands: [], used: 0 };
      this.#pages.set(page, retained);
    }
    retained.used = this.#frame;
    return retained;
  }

  /** Uploads what has been written. */
  #store(): Block | null {
    const count = this.#writer.length;
    if (count === 0) {
      return null;
    }
    let block: Block | null = null;
    for (const chunk of this.#chunks) {
      const first = chunk.stretches.take(count);
      if (first !== null) {
        block = { chunk, first, count };
        break;
      }
    }
    if (!block) {
      const length = Math.max(CHUNK, count);
      const chunk = {
        buffer: this.#guard(() =>
          this.#device.createBuffer({
            size: length * INSTANCE_BYTES,
            usage: BUFFER.vertex | BUFFER.copyDst,
          }),
        ),
        stretches: new Stretches(length),
      };
      this.#chunks.push(chunk);
      block = { chunk, first: chunk.stretches.take(count) ?? 0, count };
    }
    this.#device.queue.writeBuffer(
      block.chunk.buffer,
      block.first * INSTANCE_BYTES,
      this.#writer.bytes(),
    );
    this.uploads++;
    this.uploadedQuads += count;
    this.#kept += count * INSTANCE_BYTES;
    return block;
  }

  /** Returns the bytes that will be free. */
  #discard(block: Block | null | undefined): number {
    if (!block) {
      return 0;
    }
    this.#discarded.push(block);
    return block.count * INSTANCE_BYTES;
  }

  #free({ chunk, first, count }: Block): void {
    chunk.stretches.give(first, count);
    this.#kept -= count * INSTANCE_BYTES;
    if (chunk.stretches.isEmpty && this.#chunks.length > 1) {
      this.#chunks.splice(this.#chunks.indexOf(chunk), 1);
      chunk.buffer.destroy();
    }
  }

  #drop({ page, fills, borders, bands }: Retained): number {
    this.#pages.delete(page);
    let bytes = this.#discard(fills) + this.#discard(borders);
    for (const band of bands) {
      bytes += this.#discard(band?.block);
    }
    return bytes;
  }
}
