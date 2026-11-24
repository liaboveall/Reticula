// B13: frames of a thousand and of more than ten thousand visible cells, drawn
// by the grid's own renderers.
//
// The renderers are driven directly, with pages made up here, so that nothing
// but drawing is timed. What the app makes of it, with the engine and everything
// else in the way, is measured on the app itself: bench/run-render-bench.mjs.

import { ZOOM_STEPS } from "@reticula/grid";
import {
  acquire,
  BAND_ROWS,
  Canvas2dRenderer,
  DEFAULT_METRICS,
  type Frame,
  type GridRenderer,
  type Metrics,
  type Page,
  PageCache,
  pagesToHold,
  type RendererCounters,
  type RendererKind,
  readTheme,
  type SheetLayout,
  type SheetSize,
  type Span,
  SparseAxis,
  scrollLimits,
  TextMeasurer,
  viewMetrics,
  visibleCells,
  WebGpuRenderer,
} from "@reticula/grid/internals";
import { PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import { type Content, POOL, poolOf } from "./b13-content.ts";
import { GpuTimer } from "./gpu-time.ts";
import type { FrameSamples } from "./report.ts";
import { METRICS, stage } from "./stage.ts";
import { nextFrame, Random, type Summary, summarize } from "./stats.ts";

export type Size = "1k" | "10k";

/**
 * `scroll`: five screens a second. `jump`: another screen every frame. `zoom`:
 * every frame the next zoom, from the size's own to 200% and back, once. `edit`:
 * one cell's text changes every frame.
 */
export type Motion = "scroll" | "jump" | "zoom" | "edit";

interface Preset {
  /** CSS pixels. */
  readonly width: number;
  readonly height: number;
  readonly base: Metrics;
  readonly zoom: number;
  readonly sheet: SheetSize;
}

const PRESETS: Readonly<Record<Size, Preset>> = {
  // The thousand cells that the targets since the beginning are stated for.
  "1k": { width: 1676, height: 1036, base: METRICS, zoom: 1, sheet: { rows: 1_000_000, cols: 20 } },
  "10k": {
    width: 1920,
    height: 1080,
    base: DEFAULT_METRICS,
    zoom: 0.25,
    sheet: { rows: 1_000_000, cols: 400 },
  },
};

const SCREENS_PER_SECOND = 5;
/** Pages that are held around the ones in view, in screens. */
const MARGIN = { rows: 0.5, cols: 0.5 };
/** Frames that are measured of a run, however long they take. */
const LEAST_FRAMES = 5;
/** As often the atlas is filled in idle time before anything is drawn from it, as in the app. */
const MOST_WARMUPS = 120;

/** How many pages are next to each other before the texts go on with the next row of them. */
const pagesAcross = ({ width, base, zoom }: Preset) =>
  Math.ceil(width / (base.colWidth * zoom) / PAGE_COLS) + 1;

function makePage(preset: Preset, pageRow: number, pageCol: number, content: Content): Page {
  const { sheet } = preset;
  const firstRow = pageRow * PAGE_ROWS;
  const firstCol = pageCol * PAGE_COLS;
  const rowCount = Math.min(PAGE_ROWS, sheet.rows - firstRow);
  const colCount = Math.min(PAGE_COLS, sheet.cols - firstCol);
  const cells = rowCount * colCount;
  const pool = poolOf(content);
  const at = ((pageRow * pagesAcross(preset) + pageCol) * PAGE_ROWS * PAGE_COLS) % POOL;
  return {
    sheet: 0,
    epoch: 0,
    firstRow,
    firstCol,
    rowCount,
    colCount,
    rowIds: new BigUint64Array(rowCount),
    colIds: new BigUint64Array(colCount),
    texts: pool.texts.slice(at, at + cells),
    styleIds: new Uint16Array(cells),
    flags: pool.flags.slice(at, at + cells),
    overflows: [],
    byteLength: 0,
  };
}

/** The same page but for the text of one cell. */
function edited(page: Page, cell: number, text: string): Page {
  return { ...page, texts: page.texts.with(cell, text) };
}

/** A renderer on the stage, and the sheet it draws. */
class Subject {
  readonly renderer: GridRenderer;
  readonly adapter: string | null;
  /** Times the atlas was filled before the first frame, and what that took. */
  warmups = 0;
  warmupMs = 0;
  visible: Span = { firstRow: 0, endRow: 0, firstCol: 0, endCol: 0 };
  #preset: Preset;
  #content: Content;
  #size: { readonly width: number; readonly height: number };
  #scale = window.devicePixelRatio || 1;
  #theme;
  #cache = new PageCache();
  #zoom: number;
  #metrics: Metrics;
  #layout: SheetLayout;

  private constructor(
    host: HTMLElement,
    renderer: GridRenderer,
    adapter: string | null,
    preset: Preset,
    content: Content,
  ) {
    this.renderer = renderer;
    this.adapter = adapter;
    this.#preset = preset;
    this.#content = content;
    const width = Math.round(preset.width * this.#scale);
    const height = Math.round(preset.height * this.#scale);
    this.#size = { width: width / this.#scale, height: height / this.#scale };
    renderer.resize(width, height);
    renderer.canvas.style.width = `${this.#size.width}px`;
    renderer.canvas.style.height = `${this.#size.height}px`;
    host.append(renderer.canvas);
    this.#theme = readTheme(host);
    this.#zoom = preset.zoom;
    [this.#metrics, this.#layout] = this.#sizes(preset.zoom);
  }

  static async open(
    kind: RendererKind,
    size: Size,
    content: Content,
    software: boolean,
  ): Promise<Subject> {
    const host = stage();
    host.classList.add("reticula-grid");
    const measure = new TextMeasurer();
    if (kind === "canvas2d") {
      return new Subject(host, new Canvas2dRenderer({ measure }), null, PRESETS[size], content);
    }
    // A device of its own, so that no run finds the atlas of an earlier one.
    const lease = await acquire(software, () => {
      throw new Error("the WebGPU device was lost");
    });
    const renderer = new WebGpuRenderer(lease, { measure, onIncomplete: () => {} });
    const subject = new Subject(host, renderer, lease.gpu.adapter, PRESETS[size], content);
    const started = performance.now();
    const frame = subject.frame(0);
    while (subject.warmups < MOST_WARMUPS && !renderer.warm(frame)) {
      subject.warmups++;
      await new Promise((resolve) => setTimeout(resolve));
    }
    subject.warmupMs = performance.now() - started;
    return subject;
  }

  get screen(): number {
    const { headerHeight, scrollbarSize } = this.#metrics;
    return this.#size.height - headerHeight - scrollbarSize;
  }

  get rowHeight(): number {
    return this.#metrics.rowHeight;
  }

  get limit(): number {
    return scrollLimits(this.#metrics, this.#preset.sheet, this.#size, this.#layout).top;
  }

  zoomTo(zoom: number): void {
    this.#zoom = zoom;
    [this.#metrics, this.#layout] = this.#sizes(zoom);
  }

  /** What is to be drawn at a scroll position, with the pages that takes. */
  frame(top: number): Frame {
    const { sheet } = this.#preset;
    const scroll = { top: Math.round(top * this.#scale) / this.#scale, left: 0 };
    const visible = visibleCells(this.#metrics, sheet, scroll, this.#size, this.#layout);
    this.visible = visible;
    const pages = pagesToHold(visible, sheet, MARGIN);
    this.#cache.retain(pages);
    for (let pageRow = pages.firstRow; pageRow < pages.endRow; pageRow++) {
      for (let pageCol = pages.firstCol; pageCol < pages.endCol; pageCol++) {
        if (!this.#cache.get(pageRow, pageCol)) {
          this.#cache.accept(makePage(this.#preset, pageRow, pageCol, this.#content));
        }
      }
    }
    return {
      size: this.#size,
      scale: this.#scale,
      zoom: this.#zoom,
      metrics: this.#metrics,
      theme: this.#theme,
      scroll,
      visible,
      cache: this.#cache,
      selection: null,
      sheet,
      layout: this.#layout,
    };
  }

  /** Changes a cell in view, and returns how many bands of its page are in view. */
  edit(row: number, col: number, text: string): number {
    const page = this.#cache.pageOf(row, col);
    if (!page) {
      throw new Error("the cell to be changed is not held");
    }
    const cell = (row - page.firstRow) * page.colCount + (col - page.firstCol);
    this.#cache.accept(edited(page, cell, text));
    const first = Math.max(0, this.visible.firstRow - page.firstRow);
    const end = Math.min(page.rowCount, this.visible.endRow - page.firstRow);
    return Math.ceil(end / BAND_ROWS) - Math.floor(first / BAND_ROWS);
  }

  close(): void {
    this.renderer.destroy();
    stage().classList.remove("reticula-grid");
  }

  #sizes(zoom: number): [Metrics, SheetLayout] {
    const { base } = this.#preset;
    const axis = (size: number) => new SparseAxis(size, [], this.#scale, zoom);
    return [
      viewMetrics(base, zoom, this.#scale),
      {
        rowAxis: axis(base.rowHeight),
        colAxis: axis(base.colWidth),
        frozenRows: 0,
        frozenCols: 0,
      },
    ];
  }
}

/** What single edits made a renderer that keeps pages build again. */
export interface EditCounts {
  readonly frames: number;
  /** Frames in which more than the bands of the edited page that are in view were built or uploaded. */
  readonly excessive: number;
  /** Frames in which nothing was built. */
  readonly unbuilt: number;
  readonly mostBands: number;
  readonly bandsInView: number;
}

export interface RunOptions {
  readonly renderer: RendererKind;
  readonly size: Size;
  readonly motion: Motion;
  readonly content: Content;
  readonly seconds: number;
  readonly warmupSeconds: number;
  /** Whether to make do with an emulated GPU. */
  readonly software?: boolean;
  /** Whether to have the GPU time its passes, which costs the main thread something. */
  readonly timeGpu?: boolean;
}

export interface RunResult extends FrameSamples {
  readonly renderer: RendererKind;
  readonly adapter: string | null;
  /** CSS pixels of the grid's area. */
  readonly width: number;
  readonly height: number;
  readonly zoom: number;
  /** Cells whose text was drawn, per frame. */
  readonly cells: Summary;
  /** The frames of the warm-up, the first of which finds nothing drawn before. */
  readonly cold: { readonly work: Summary; readonly warmups: number; readonly warmupMs: number };
  /** At the end of the warm-up, and at the end. */
  readonly counters: { readonly warm: RendererCounters; readonly end: RendererCounters } | null;
  readonly edits: EditCounts | null;
  /** Milliseconds the GPU took for a pass, where it says so. */
  readonly gpu: Summary | null;
}

/** Draws frames for a while, one per frame of the display, and times them. */
export async function run(options: RunOptions): Promise<RunResult> {
  const { motion, size, content } = options;
  const preset = PRESETS[size];
  poolOf(content);
  const timer = options.timeGpu ? new GpuTimer() : null;
  const subject = await Subject.open(options.renderer, size, content, options.software ?? false);
  const { renderer } = subject;
  const { limit } = subject;
  const up = ZOOM_STEPS.filter((zoom) => zoom >= preset.zoom);
  const zooms = [...up, ...up.slice(1, -1).reverse()];
  const random = new Random(13);

  const work: number[] = [];
  const interval: number[] = [];
  const cold: number[] = [];
  const cells: number[] = [];
  const edits = { frames: 0, excessive: 0, unbuilt: 0, mostBands: 0, bandsInView: 0 };
  let warm: RendererCounters | null = null;
  let top = 0;
  let number = 0;
  const started = await nextFrame();
  for (let last = started; ; ) {
    const now = await nextFrame();
    const elapsed = now - started;
    const isOver = elapsed >= (options.warmupSeconds + options.seconds) * 1000;
    if (motion !== "zoom" && isOver && work.length >= LEAST_FRAMES) {
      break;
    }
    const isWarm = elapsed >= options.warmupSeconds * 1000;
    if (isWarm) {
      warm ??= renderer.counters ?? null;
      // However fast a renderer is, it goes through the same frames.
      if (motion === "zoom" && work.length === zooms.length) {
        break;
      }
    }
    let frame: Frame;
    let bandsInView = 0;
    switch (motion) {
      case "scroll":
        top = (top + (SCREENS_PER_SECOND * subject.screen * (now - last)) / 1000) % limit;
        frame = subject.frame(top);
        break;
      case "jump":
        top = (top + subject.screen) % limit;
        frame = subject.frame(top);
        break;
      case "zoom": {
        subject.zoomTo(zooms[work.length] ?? preset.zoom);
        // The first row in view stays the same one.
        frame = subject.frame(1000 * subject.rowHeight);
        break;
      }
      case "edit": {
        frame = subject.frame(0);
        const { endRow, endCol } = subject.visible;
        const text = `${number}.${random.below(10)}`;
        bandsInView = subject.edit(random.below(endRow), random.below(endCol), text);
        break;
      }
    }
    number++;
    const before = motion === "edit" ? renderer.counters : undefined;
    // Only the renderer's work is timed; making up the pages is not.
    const begun = performance.now();
    const counts = renderer.draw(frame);
    const spent = performance.now() - begun;
    const after = before && renderer.counters;
    if (counts.missing > 0) {
      throw new Error("a page that should have been made is missing");
    }
    if (isWarm) {
      work.push(spent);
      interval.push(now - last);
      cells.push(counts.cells);
      if (motion === "edit" && before && after) {
        const bands = after.bandsBuilt - before.bandsBuilt;
        const uploads = after.pageUploads - before.pageUploads;
        edits.frames++;
        edits.excessive += bands > bandsInView || uploads > bandsInView ? 1 : 0;
        edits.unbuilt += bands === 0 ? 1 : 0;
        edits.mostBands = Math.max(edits.mostBands, bands);
        edits.bandsInView = Math.max(edits.bandsInView, bandsInView);
      }
    } else {
      cold.push(spent);
    }
    last = now;
  }
  const end = renderer.counters ?? null;
  const gpu = (await timer?.finish()) ?? null;
  subject.close();
  return {
    renderer: options.renderer,
    adapter: subject.adapter,
    width: preset.width,
    height: preset.height,
    zoom: preset.zoom,
    work,
    interval,
    // No page is missing here; but texts may have to wait for a later frame.
    placeholderFrames: (end?.deferredFrames ?? 0) - (warm?.deferredFrames ?? 0),
    cells: summarize(cells),
    cold: { work: summarize(cold), warmups: subject.warmups, warmupMs: subject.warmupMs },
    counters: warm && end ? { warm, end } : null,
    edits: motion === "edit" && end ? edits : null,
    gpu,
  };
}

/** The machine's GPU, as the browser presents it. */
export interface AdapterInfo {
  readonly vendor: string;
  readonly architecture: string;
  readonly device: string;
  readonly description: string;
  readonly software: boolean;
  /** Whether it can time its passes. */
  readonly timestamps: boolean;
}

export async function adapter(): Promise<AdapterInfo | null> {
  const found = await navigator.gpu?.requestAdapter();
  if (!found) {
    return null;
  }
  const { vendor, architecture, device, description, isFallbackAdapter } = found.info;
  // An older name of the same.
  const { isFallbackAdapter: older } = found as { isFallbackAdapter?: boolean };
  return {
    vendor,
    architecture,
    device,
    description,
    software: isFallbackAdapter ?? older ?? false,
    timestamps: found.features.has("timestamp-query"),
  };
}
