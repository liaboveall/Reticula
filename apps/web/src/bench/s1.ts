// Viewport pages between the engine's worker and the main thread: how long a
// page takes, whether scrolling outruns the data, and how soon a jump fills in.

import { type FrameReport, Grid } from "@reticula/grid";
import { decodePage, type Page } from "@reticula/grid/internals";
import { Engine, init } from "@reticula/protocol";
import {
  CELL_LEN,
  COL_COUNT_AT,
  HEADER_LEN,
  ID_LEN,
  PAGE_COLS,
  PAGE_ROWS,
  ROW_COUNT_AT,
  TEXT_LEN_AT,
} from "@reticula/protocol/viewport";
import wasmUrl from "@reticula/protocol/wasm?url";
import { METRICS, type OpenEngine, openEngine, release, stage } from "./stage.ts";
import { nextFrame, Random, type Summary, summarize } from "./stats.ts";

/** Asks the engine for one full page and resolves when it has been decoded. */
function fetchPage(
  engine: OpenEngine,
  epoch: number,
  pageRow: number,
): Promise<{ started: number; arrived: number; decoded: number; page: Page; buffer: ArrayBuffer }> {
  return new Promise((resolve) => {
    engine.onPage = (buffer) => {
      const arrived = performance.now();
      const page = decodePage(buffer);
      const decoded = performance.now();
      resolve({ started, arrived, decoded, page, buffer });
    };
    const started = performance.now();
    engine.client.send({
      kind: "subscribe",
      sheet: 0,
      epoch,
      firstRow: pageRow * PAGE_ROWS,
      rowCount: PAGE_ROWS,
      firstCol: 0,
      colCount: PAGE_COLS,
    });
  });
}

/** Page rows picked at random, never the same one twice in a row. */
function* randomPageRows(sheetRows: number, seed: number): Generator<number, never> {
  const random = new Random(seed);
  const fullPages = Math.floor(sheetRows / PAGE_ROWS);
  for (let previous = -1; ; ) {
    let pageRow = random.below(fullPages);
    if (pageRow === previous) {
      pageRow = (pageRow + 1) % fullPages;
    }
    previous = pageRow;
    yield pageRow;
  }
}

export interface RoundTripResult {
  readonly cellsPerPage: number;
  /** Size of a page's packet in bytes. */
  readonly bytes: Summary;
  /** Milliseconds from asking for a page until it is decoded. */
  readonly total: Summary;
  /** Of that, until the packet arrived on the main thread. */
  readonly arrival: Summary;
  /** Of that, decoding it. */
  readonly decode: Summary;
  /** Every sample of `total`, in the order taken. */
  readonly samples: readonly number[];
}

/**
 * One page at a time, at random positions, with the engine otherwise idle:
 * from posting the request to having the page decoded.
 */
export async function roundTrip(options: {
  samples: number;
  warmup: number;
  seed: number;
}): Promise<RoundTripResult> {
  const engine = await openEngine();
  const pageRows = randomPageRows(engine.sheet.rows, options.seed);
  const total: number[] = [];
  const arrival: number[] = [];
  const decode: number[] = [];
  const bytes: number[] = [];
  let cellsPerPage = 0;
  for (let index = 0; index < options.warmup + options.samples; index++) {
    const sample = await fetchPage(engine, index + 1, pageRows.next().value);
    if (index >= options.warmup) {
      total.push(sample.decoded - sample.started);
      arrival.push(sample.arrived - sample.started);
      decode.push(sample.decoded - sample.arrived);
      bytes.push(sample.page.byteLength);
      cellsPerPage = sample.page.rowCount * sample.page.colCount;
    }
  }
  release(engine);
  return {
    cellsPerPage,
    bytes: summarize(bytes),
    total: summarize(total),
    arrival: summarize(arrival),
    decode: summarize(decode),
    samples: total,
  };
}

export interface CodecResult {
  readonly pages: number;
  readonly rounds: number;
  readonly cellsPerPage: number;
  /** Bytes of a whole packet as it is sent now. */
  readonly packetBytes: Summary;
  /** Bytes of a page's text as UTF-8, which is what is sent now. */
  readonly textBytesUtf8: Summary;
  /** Bytes the same text would take as UTF-16. */
  readonly textBytesUtf16: Summary;
  /** UTF-16 code units of a page's text. */
  readonly textUnits: Summary;
  /** Microseconds for one page, one value for each round. */
  readonly encodeMicros: Summary;
  readonly decodeMicros: Summary;
  /** Microseconds to turn one page's text into a string, from each encoding. */
  readonly textFromUtf8Micros: Summary;
  readonly textFromUtf16Micros: Summary;
}

/**
 * What encoding and decoding a page cost on their own, and what sending the
 * text as UTF-16 instead of UTF-8 would change.
 */
export async function codec(options: {
  pages: number;
  rounds: number;
  seed: number;
}): Promise<CodecResult> {
  const engine = await openEngine();
  const pageRows = randomPageRows(engine.sheet.rows, options.seed);
  const packets: ArrayBuffer[] = [];
  let cellsPerPage = 0;
  for (let index = 0; index < options.pages; index++) {
    const sample = await fetchPage(engine, index + 1, pageRows.next().value);
    packets.push(sample.buffer);
    cellsPerPage = sample.page.rowCount * sample.page.colCount;
  }
  release(engine);

  // The text of each page, in both encodings.
  const utf8Decoder = new TextDecoder("utf-8");
  const utf16Decoder = new TextDecoder("utf-16le");
  const asUtf8: Uint8Array[] = [];
  const asUtf16: Uint8Array[] = [];
  const units: number[] = [];
  for (const packet of packets) {
    const view = new DataView(packet);
    const rows = view.getUint16(ROW_COUNT_AT, true);
    const cols = view.getUint16(COL_COUNT_AT, true);
    const textAt = HEADER_LEN + (rows + cols) * ID_LEN + rows * cols * CELL_LEN;
    const utf8 = new Uint8Array(packet, textAt, view.getUint32(TEXT_LEN_AT, true));
    const text = utf8Decoder.decode(utf8);
    const utf16 = new Uint8Array(text.length * 2);
    const writer = new DataView(utf16.buffer);
    for (let unit = 0; unit < text.length; unit++) {
      writer.setUint16(unit * 2, text.charCodeAt(unit), true);
    }
    asUtf8.push(utf8);
    asUtf16.push(utf16);
    units.push(text.length);
  }

  // A second copy of the engine on this thread, to time encoding by itself.
  await init({ module_or_path: wasmUrl });
  const local = new Engine();
  local.handle({ kind: "openSample", seed: engine.sheet.seed, rows: 200_000 });
  const localRows = randomPageRows(200_000, options.seed);

  // One page takes a few microseconds, which is close to what the clock can
  // resolve, so a whole round is timed and divided by its number of pages.
  const perPage = (run: () => void): number => {
    const started = performance.now();
    run();
    return ((performance.now() - started) * 1000) / options.pages;
  };
  let sink = 0;
  const encode: number[] = [];
  const decode: number[] = [];
  const fromUtf8: number[] = [];
  const fromUtf16: number[] = [];
  for (let round = 0; round < options.rounds; round++) {
    encode.push(
      perPage(() => {
        for (let index = 0; index < options.pages; index++) {
          local.handle({
            kind: "subscribe",
            sheet: 0,
            epoch: index,
            firstRow: localRows.next().value * PAGE_ROWS,
            rowCount: PAGE_ROWS,
            firstCol: 0,
            colCount: PAGE_COLS,
          });
          sink += local.takePage()?.length ?? 0;
        }
      }),
    );
    decode.push(
      perPage(() => {
        for (const packet of packets) {
          sink += decodePage(packet).texts.length;
        }
      }),
    );
    fromUtf8.push(
      perPage(() => {
        for (const bytes of asUtf8) {
          sink += utf8Decoder.decode(bytes).length;
        }
      }),
    );
    fromUtf16.push(
      perPage(() => {
        for (const bytes of asUtf16) {
          sink += utf16Decoder.decode(bytes).length;
        }
      }),
    );
  }
  local.free();
  if (sink === 0) {
    throw new Error("the codec measurement did no work");
  }

  return {
    pages: options.pages,
    rounds: options.rounds,
    cellsPerPage,
    packetBytes: summarize(packets.map((packet) => packet.byteLength)),
    textBytesUtf8: summarize(asUtf8.map((bytes) => bytes.length)),
    textBytesUtf16: summarize(asUtf16.map((bytes) => bytes.length)),
    textUnits: summarize(units),
    encodeMicros: summarize(encode),
    decodeMicros: summarize(decode),
    textFromUtf8Micros: summarize(fromUtf8),
    textFromUtf16Micros: summarize(fromUtf16),
  };
}

/** A grid on the stage, fed by the engine, reporting every frame to `listener`. */
async function openGrid(margin: { rows: number; cols: number }): Promise<{
  engine: OpenEngine;
  grid: Grid;
  listen(listener: ((report: FrameReport) => void) | null): void;
  /** Resolves with the first frame from now on that `accept` returns true for. */
  frame(accept: (report: FrameReport) => boolean): Promise<FrameReport>;
  close(): void;
}> {
  const engine = await openEngine();
  let listener: ((report: FrameReport) => void) | null = null;
  let waiting: { accept: (report: FrameReport) => boolean; resolve: (r: FrameReport) => void }[] =
    [];
  const grid = new Grid(stage(), {
    subscribe: (subscription) =>
      engine.client.send({ kind: "subscribe", sheet: 0, ...subscription }),
    margin,
    metrics: METRICS,
    onFrame(report) {
      listener?.(report);
      const still = [];
      for (const waiter of waiting) {
        if (waiter.accept(report)) {
          waiter.resolve(report);
        } else {
          still.push(waiter);
        }
      }
      waiting = still;
    },
  });
  engine.onPage = (page) => grid.acceptPage(page);
  grid.setSheet(engine.sheet);
  const frame = (accept: (report: FrameReport) => boolean) =>
    new Promise<FrameReport>((resolve) => waiting.push({ accept, resolve }));
  await frame((report) => report.cells > 0 && report.missing === 0);
  return {
    engine,
    grid,
    listen(next) {
      listener = next;
    },
    frame,
    close() {
      grid.destroy();
      release(engine);
    },
  };
}

export interface ScrollResult {
  readonly margin: { readonly rows: number; readonly cols: number };
  readonly visibleCells: number;
  readonly screensPerSecond: number;
  /** Frames drawn after the warm-up. */
  readonly frames: number;
  /** Of those, the frames that showed at least one placeholder. */
  readonly framesWithPlaceholders: number;
  /** The most placeholder cells any one frame showed. */
  readonly mostPlaceholders: number;
  readonly rowsScrolled: number;
  readonly pagesReceived: number;
  readonly bytesReceived: number;
  /** The most pages held at any time. */
  readonly mostPagesHeld: number;
  /** Main-thread work per frame in milliseconds. */
  readonly work: Summary;
  /** Time between frames in milliseconds. */
  readonly interval: Summary;
}

/**
 * Scrolls down at a steady speed and counts the frames in which the data had
 * not kept up.
 */
export async function scroll(options: {
  marginRows: number;
  marginCols: number;
  screensPerSecond: number;
  seconds: number;
  warmupSeconds: number;
}): Promise<ScrollResult> {
  const margin = { rows: options.marginRows, cols: options.marginCols };
  const { grid, listen, close } = await openGrid(margin);
  const before = grid.stats();
  const screenRows = before.visible.endRow - before.visible.firstRow;
  const screen = screenRows * METRICS.rowHeight;

  let measuring = false;
  let frames = 0;
  let framesWithPlaceholders = 0;
  let mostPlaceholders = 0;
  let mostPagesHeld = 0;
  const work: number[] = [];
  listen((report) => {
    if (!measuring) {
      return;
    }
    frames++;
    work.push(report.work);
    if (report.missing > 0) {
      framesWithPlaceholders++;
      mostPlaceholders = Math.max(mostPlaceholders, report.missing);
    }
  });

  const intervals: number[] = [];
  let received = { pages: 0, bytes: 0, row: 0 };
  let top = 0;
  const started = await nextFrame();
  for (let last = started; ; ) {
    const now = await nextFrame();
    if (!measuring && now - started >= options.warmupSeconds * 1000) {
      measuring = true;
      const stats = grid.stats();
      received = {
        pages: stats.pagesReceived,
        bytes: stats.bytesReceived,
        row: stats.visible.firstRow,
      };
    } else if (measuring) {
      intervals.push(now - last);
      mostPagesHeld = Math.max(mostPagesHeld, grid.stats().pagesHeld);
    }
    if (now - started >= (options.warmupSeconds + options.seconds) * 1000) {
      break;
    }
    top += (options.screensPerSecond * screen * (now - last)) / 1000;
    grid.scrollTo({ top });
    last = now;
  }
  const after = grid.stats();
  close();
  return {
    margin,
    visibleCells: screenRows * (before.visible.endCol - before.visible.firstCol),
    screensPerSecond: options.screensPerSecond,
    frames,
    framesWithPlaceholders,
    mostPlaceholders,
    rowsScrolled: after.visible.firstRow - received.row,
    pagesReceived: after.pagesReceived - received.pages,
    bytesReceived: after.bytesReceived - received.bytes,
    mostPagesHeld,
    work: summarize(work),
    interval: summarize(intervals),
  };
}

export interface JumpResult {
  readonly margin: { readonly rows: number; readonly cols: number };
  readonly jumps: number;
  /** Milliseconds from the jump until a frame showed every visible cell. */
  readonly filled: Summary;
  /** Milliseconds from the jump until the first frame at the new position. */
  readonly firstFrame: Summary;
  /** Frames drawn at the new position before it was complete. */
  readonly framesWithPlaceholders: Summary;
  /**
   * Main-thread work, in milliseconds, of the frame that completed each jump:
   * a whole screen of cells that were not on screen the frame before.
   */
  readonly work: Summary;
  /** Every sample of `filled`, in the order taken. */
  readonly samples: readonly number[];
}

/**
 * Jumps to random positions, as dragging the scrollbar does, and times how
 * long each takes to fill in.
 */
export async function jump(options: {
  jumps: number;
  seed: number;
  marginRows: number;
  marginCols: number;
}): Promise<JumpResult> {
  const margin = { rows: options.marginRows, cols: options.marginCols };
  const { grid, frame, close } = await openGrid(margin);
  const random = new Random(options.seed);
  const limit = grid.stats().scrollLimits.top;
  const filled: number[] = [];
  const firstFrame: number[] = [];
  const placeholderFrames: number[] = [];
  const work: number[] = [];
  for (let index = 0; index < options.jumps; index++) {
    // Away from where the grid is now, so that nothing needed is held yet.
    const top = Math.floor(random.unit() * limit);
    const firstRow = Math.floor(top / METRICS.rowHeight);
    let first: number | null = null;
    let incomplete = 0;
    const done = frame((report) => {
      if (report.visible.firstRow !== firstRow) {
        return false;
      }
      first ??= report.time;
      if (report.missing > 0) {
        incomplete++;
        return false;
      }
      return true;
    });
    const started = performance.now();
    grid.scrollTo({ top });
    const report = await done;
    filled.push(report.time - started);
    firstFrame.push((first ?? report.time) - started);
    placeholderFrames.push(incomplete);
    work.push(report.work);
    await nextFrame();
    await nextFrame();
  }
  close();
  return {
    margin,
    jumps: options.jumps,
    filled: summarize(filled),
    firstFrame: summarize(firstFrame),
    framesWithPlaceholders: summarize(placeholderFrames),
    work: summarize(work),
    samples: filled,
  };
}
