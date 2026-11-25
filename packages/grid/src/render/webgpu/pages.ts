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
export interface Band {
  /** What they were laid out as, and with; their page may have been laid out at another zoom since. */
  readonly layout: PageLayout;
  readonly look: Look;
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
  layout: PageLayout;
  look: Look;
  fills: Block | null | undefined;
  borders: Block | null | undefined;
  readonly bands: (Band | undefined)[];
  /** Number of the frame it was last part of. */
  used: number;
}

/** Quads on their way to the GPU: as many from one on, to a place in a chunk. */
interface Write {
  readonly chunk: Chunk;
  readonly first: number;
  readonly from: number;
  count: number;
}

/** Whether a band is to be had, with something to show until it is made, or without. */
export type BandState = "ready" | "stale" | "missing";

/**
 * The quads of the pages in view and around it. A page has its own for as long
 * as it has its layout, which is as long as it looks the same: wherever it is
 * scrolled to, nothing is uploaded. When it is zoomed, those it had are kept to
 * stand for the new ones.
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
  /** The one that was asked for last in this frame. */
  #last: Retained | null = null;
  /** Quads that have yet to be uploaded, and where they go. */
  #writer = new InstanceWriter();
  #writes: Write[] = [];
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
    this.#last = null;
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

  rects(look: Look, layout: PageLayout, kind: "fills" | "borders"): Block | null {
    const retained = this.#retained(look, layout);
    let block = retained[kind];
    if (block === undefined) {
      const list = layout[kind];
      const { length } = this.#writer;
      this.#writer.rects(list, 0, list.length);
      block = this.#store(length);
      retained[kind] = block;
    }
    return block;
  }

  stateOf(look: Look, layout: PageLayout, index: number, view: PageView): BandState {
    const band = this.shown(look, layout, index, view);
    return !band ? "missing" : band.layout === layout && band.complete ? "ready" : "stale";
  }

  /**
   * The texts of a band as far as they have been made: some may be bars, and all
   * of them what they were at another zoom. What they copy from does not make
   * room for others in this frame.
   */
  shown(look: Look, layout: PageLayout, index: number, view: PageView): Band | null {
    const band = this.#retained(look, layout).bands[index];
    if (!band || !(band.layout === layout ? this.#holds(band, view) : this.#standsIn(band, look))) {
      return null;
    }
    this.#glyphs.atlas.plan.use(band.layers);
    return band;
  }

  /** Makes the texts of a band, if they are not there yet. */
  make(look: Look, layout: PageLayout, index: number, view: PageView): void {
    const { bands } = this.#retained(look, layout);
    const before = bands[index];
    if (this.stateOf(look, layout, index, view) === "ready") {
      return;
    }
    const { length } = this.#writer;
    const band = this.#build(look, layout, index, view, before);
    if (!band.complete && before && this.#standsIn(before, look)) {
      // Texts at another zoom are better than bars.
      this.#writer.length = length;
      return;
    }
    this.#discard(before?.block);
    bands[index] = { ...band, block: this.#store(length) };
    this.#glyphs.atlas.plan.use(band.layers);
  }

  /** Uploads what has been made since the last time. */
  flush(): void {
    for (const { chunk, first, from, count } of this.#writes) {
      const bytes = this.#writer.bytes(from, count);
      this.#device.queue.writeBuffer(chunk.buffer, first * INSTANCE_BYTES, bytes);
      this.uploads++;
      this.uploadedQuads += count;
    }
    this.#writes = [];
    this.#writer.length = 0;
  }

  /** Lets go of all that only stands for something else. Returns whether there was any. */
  dropStale(): boolean {
    let dropped = false;
    for (const { layout, bands } of this.#pages.values()) {
      for (const [index, band] of bands.entries()) {
        if (band && band.layout !== layout) {
          this.#discard(band.block);
          bands[index] = undefined;
          dropped = true;
        }
      }
    }
    return dropped;
  }

  destroy(): void {
    for (const { buffer } of this.#chunks) {
      buffer.destroy();
    }
    this.#chunks = [];
    this.#pages.clear();
  }

  /** Whether a band shows its texts as they are at another zoom. */
  #standsIn(band: Band, look: Look): boolean {
    return (
      band.look !== look &&
      band.look.theme === look.theme &&
      this.#glyphs.atlas.plan.holds(band.layers, band.generations)
    );
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
  ): Omit<Band, "block"> {
    const { first, end, firstStrike, endStrike } = layout.band(index);
    const runs = layout.texts;
    const glyphs = this.#glyphs;
    const writer = this.#writer;
    glyphs.layers = 0;
    glyphs.partial = false;
    const { deferred } = glyphs;
    // More than is in view, or scrolling sideways would build it again all the
    // time; and what was in view before, which another part of the frame may show.
    const spare = right - left;
    let from = left - spare;
    let to = right + spare;
    if (
      before?.layout === layout &&
      before.left > from - 2 * spare &&
      before.right < to + 2 * spare
    ) {
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
      layout,
      look,
      layers: glyphs.layers,
      generations: glyphs.atlas.plan.generations,
      complete: glyphs.deferred === deferred,
      left: glyphs.partial ? from : Number.NEGATIVE_INFINITY,
      right: glyphs.partial ? to : Number.POSITIVE_INFINITY,
      clipped,
    };
  }

  #retained(look: Look, layout: PageLayout): Retained {
    if (this.#last?.layout === layout) {
      return this.#last;
    }
    const { page } = layout;
    let retained = this.#pages.get(page);
    if (retained && retained.layout !== layout) {
      this.#discard(retained.fills);
      this.#discard(retained.borders);
      retained.fills = undefined;
      retained.borders = undefined;
      // Those of another zoom are where its sizes were, not where they are now.
      const isZoomed = retained.look !== look;
      retained.layout = layout;
      retained.look = look;
      for (const [index, band] of retained.bands.entries()) {
        if (band && !(isZoomed && this.#standsIn(band, look))) {
          this.#discard(band.block);
          retained.bands[index] = undefined;
        }
      }
    }
    if (!retained) {
      retained = { page, layout, look, fills: undefined, borders: undefined, bands: [], used: 0 };
      this.#pages.set(page, retained);
    }
    retained.used = this.#frame;
    this.#last = retained;
    return retained;
  }

  /** Finds a place for the quads from one on. */
  #store(from: number): Block | null {
    const count = this.#writer.length - from;
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
    const { chunk, first } = block;
    const last = this.#writes.at(-1);
    if (
      last?.chunk === chunk &&
      last.first + last.count === first &&
      last.from + last.count === from
    ) {
      last.count += count;
    } else {
      this.#writes.push({ chunk, first, from, count });
    }
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
