// Drawing text on a 2D canvas: how long the main thread takes to issue the
// draw calls of one frame, and whether whole frames keep up.
//
// The renderer is driven directly, with pages made up here, so that nothing
// but drawing is timed and the mix of content is what the target specifies.

import {
  ALL_SHORTCUTS,
  type Page,
  PageCache,
  pagesToHold,
  Renderer,
  readTheme,
  type Shortcuts,
  scrollLimits,
  visibleCells,
} from "@reticula/grid/internals";
import { FLAG_NUMBER, PAGE_COLS, PAGE_ROWS } from "@reticula/protocol/viewport";
import { METRICS, stage } from "./stage.ts";
import { nextFrame, Random, type Summary, summarize } from "./stats.ts";

const SHEET = { rows: 1_000_000, cols: 20 };

/**
 * `unique`: every cell has a text of its own, so a string that scrolls into
 * view has never been measured. `repeating`: texts come from short lists, as
 * the values of real columns mostly do.
 */
export type Content = "unique" | "repeating";

const HANZI =
  "的一是不了人我在有他这为之大来以个中上们到说国和地也子时道出而要于就下得可你年生自会那后能对着事其里所去行过家十用发天如然作方成者多日都三小军二无同么经法当起与好看学进种将还分此心前面又定见只主没公从";
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

const hanzi = (random: Random) => pick(random, HANZI, 2, 7);
const ascii = (random: Random) => pick(random, LETTERS, 4, 12);

function list(seed: number, length: number, make: (random: Random) => string): string[] {
  const random = new Random(seed);
  return Array.from({ length }, () => make(random));
}

const LISTS = {
  numbers: list(1, 200, number),
  hanzi: list(2, 60, hanzi),
  ascii: list(3, 200, ascii),
};

/** A page of made-up cells: half numbers, three in ten Chinese, two in ten ASCII. */
function syntheticPage(pageRow: number, pageCol: number, content: Content): Page {
  const firstRow = pageRow * PAGE_ROWS;
  const firstCol = pageCol * PAGE_COLS;
  const rowCount = Math.min(PAGE_ROWS, SHEET.rows - firstRow);
  const colCount = Math.min(PAGE_COLS, SHEET.cols - firstCol);
  const texts = new Array<string>(rowCount * colCount);
  const flags = new Uint8Array(rowCount * colCount);
  // Seeded by the page, so that a page looks the same whenever it is made.
  const random = new Random(
    Math.imul(pageRow + 1, 0x9e3779b1) ^ Math.imul(pageCol + 1, 0x85ebca6b),
  );
  for (let cell = 0; cell < texts.length; cell++) {
    const kind = random.below(10);
    if (kind < 5) {
      texts[cell] =
        content === "unique" ? number(random) : (LISTS.numbers[random.below(200)] ?? "");
      flags[cell] = FLAG_NUMBER;
    } else if (kind < 8) {
      texts[cell] = content === "unique" ? hanzi(random) : (LISTS.hanzi[random.below(60)] ?? "");
    } else {
      texts[cell] = content === "unique" ? ascii(random) : (LISTS.ascii[random.below(200)] ?? "");
    }
  }
  return {
    epoch: 0,
    firstRow,
    firstCol,
    rowCount,
    colCount,
    rowIds: new BigUint64Array(rowCount),
    colIds: new BigUint64Array(colCount),
    texts,
    flags,
    byteLength: 0,
  };
}

/** A canvas on the stage, sized the way the grid sizes its own. */
function openCanvas(shortcuts: Shortcuts) {
  const host = stage();
  host.classList.add("reticula-grid");
  const canvas = document.createElement("canvas");
  const scale = window.devicePixelRatio || 1;
  const size = { width: host.clientWidth, height: host.clientHeight };
  canvas.width = Math.round(size.width * scale);
  canvas.height = Math.round(size.height * scale);
  canvas.style.width = `${size.width}px`;
  canvas.style.height = `${size.height}px`;
  host.append(canvas);
  const renderer = new Renderer(canvas, shortcuts);
  const theme = readTheme(host);
  const cache = new PageCache();

  return {
    size,
    scale,
    font: theme.font,
    limit: scrollLimits(METRICS, SHEET, size).top,
    /** Draws the frame at a scroll position; returns the time drawing took and what was drawn. */
    draw(top: number, content: Content) {
      const scroll = { top, left: 0 };
      const visible = visibleCells(METRICS, SHEET, scroll, size);
      const pages = pagesToHold(visible, SHEET, { rows: 0, cols: 0 });
      cache.retain(pages);
      for (let pageRow = pages.firstRow; pageRow < pages.endRow; pageRow++) {
        for (let pageCol = pages.firstCol; pageCol < pages.endCol; pageCol++) {
          if (!cache.get(pageRow, pageCol)) {
            cache.accept(syntheticPage(pageRow, pageCol, content));
          }
        }
      }
      // Only the renderer's work is timed; making up the pages is not.
      const started = performance.now();
      const counts = renderer.draw({
        size,
        scale,
        metrics: METRICS,
        theme,
        scroll,
        visible,
        cache,
        selection: null,
      });
      return { elapsed: performance.now() - started, counts };
    },
    close() {
      canvas.remove();
      host.classList.remove("reticula-grid");
    },
  };
}

export interface DrawResult {
  readonly content: Content;
  readonly rowsPerFrame: number;
  readonly shortcuts: Shortcuts;
  readonly scale: number;
  readonly font: string;
  readonly frames: number;
  /** Cells whose text was drawn, per frame. */
  readonly cells: Summary;
  /** Of those, the cells drawn through a clip, per frame. */
  readonly clipped: Summary;
  /** Milliseconds the main thread took to issue one frame's draw calls. */
  readonly draw: Summary;
  /** Milliseconds between frames. */
  readonly interval: Summary;
  /** Every sample of `draw`, in the order taken. */
  readonly samples: readonly number[];
}

/**
 * Draws one frame per display frame, each `rowsPerFrame` further down than
 * the last, and times the drawing. Moving by a whole screen makes every
 * frame's text new; moving by a few rows is what scrolling looks like.
 */
export async function draw(options: {
  content: Content;
  rowsPerFrame: number;
  frames: number;
  warmup: number;
  shortcuts?: Partial<Shortcuts>;
}): Promise<DrawResult> {
  const shortcuts = { ...ALL_SHORTCUTS, ...options.shortcuts };
  const canvas = openCanvas(shortcuts);
  const draws: number[] = [];
  const intervals: number[] = [];
  const cells: number[] = [];
  const clipped: number[] = [];
  let top = 0;
  let last = await nextFrame();
  for (let frame = 0; frame < options.warmup + options.frames; frame++) {
    const now = await nextFrame();
    top = (top + options.rowsPerFrame * METRICS.rowHeight) % canvas.limit;
    const { elapsed, counts } = canvas.draw(top, options.content);
    if (frame >= options.warmup) {
      draws.push(elapsed);
      intervals.push(now - last);
      cells.push(counts.cells);
      clipped.push(counts.clipped);
    }
    last = now;
  }
  canvas.close();
  return {
    content: options.content,
    rowsPerFrame: options.rowsPerFrame,
    shortcuts,
    scale: canvas.scale,
    font: canvas.font,
    frames: options.frames,
    cells: summarize(cells),
    clipped: summarize(clipped),
    draw: summarize(draws),
    interval: summarize(intervals),
    samples: draws,
  };
}

export interface PaceResult {
  readonly content: Content;
  readonly screensPerSecond: number;
  readonly scale: number;
  readonly seconds: number;
  readonly frames: number;
  /** Milliseconds between frames. */
  readonly interval: Summary;
  /** Frames that came more than half an interval late, going by the median interval. */
  readonly lateFrames: number;
  /** Frames that took longer than 16.7 ms to come. */
  readonly framesOver60Hz: number;
  /** Milliseconds the main thread took to issue one frame's draw calls. */
  readonly draw: Summary;
  /** Every interval, in the order taken. */
  readonly samples: readonly number[];
}

/**
 * Scrolls at a steady speed for a while, drawing every frame, and records
 * when the frames came. Whether the browser kept up shows in the intervals.
 */
export async function pace(options: {
  content: Content;
  screensPerSecond: number;
  seconds: number;
  warmupSeconds: number;
}): Promise<PaceResult> {
  const canvas = openCanvas(ALL_SHORTCUTS);
  const screen = canvas.size.height - METRICS.headerHeight - METRICS.scrollbarSize;
  const draws: number[] = [];
  const intervals: number[] = [];
  let top = 0;
  const started = await nextFrame();
  for (let last = started; ; ) {
    const now = await nextFrame();
    const elapsed = now - started;
    if (elapsed >= (options.warmupSeconds + options.seconds) * 1000) {
      break;
    }
    top = (top + (options.screensPerSecond * screen * (now - last)) / 1000) % canvas.limit;
    const { elapsed: drawing } = canvas.draw(top, options.content);
    if (elapsed >= options.warmupSeconds * 1000) {
      draws.push(drawing);
      intervals.push(now - last);
    }
    last = now;
  }
  canvas.close();
  const interval = summarize(intervals);
  return {
    content: options.content,
    screensPerSecond: options.screensPerSecond,
    scale: canvas.scale,
    seconds: options.seconds,
    frames: intervals.length,
    interval,
    lateFrames: intervals.filter((sample) => sample > interval.p50 * 1.5).length,
    framesOver60Hz: intervals.filter((sample) => sample > 1000 / 60 + 0.5).length,
    draw: summarize(draws),
    samples: intervals,
  };
}
