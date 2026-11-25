// What bench/run-table-bench.mjs does in the app's page. It is run before the
// page's own scripts, and adds nothing to them but to see when the worker behind
// tables is asked for pages and when those come.
"use strict";

{
  const traffic = [];
  const Native = window.Worker;
  window.Worker = class extends Native {
    #isTables;
    constructor(url, options) {
      super(url, options);
      this.#isTables = /table-worker/.test(String(url));
      if (this.#isTables) {
        this.addEventListener("message", ({ data }) => {
          if (data instanceof ArrayBuffer) {
            traffic.push({ isPage: true, at: performance.now() });
          }
        });
      }
    }
    postMessage(message, transfer) {
      if (this.#isTables && message?.kind === "subscribe") {
        traffic.push({ isPage: false, at: performance.now() });
      }
      super.postMessage(message, transfer);
    }
  };

  const nextFrame = () => new Promise((resolve) => requestAnimationFrame(resolve));
  const hooks = () => window.__reticula;
  const table = () => hooks().table();

  async function until(what, test, ms = 120_000) {
    const end = performance.now() + ms;
    while (!test()) {
      if (performance.now() > end) {
        throw new Error(`waited for ${what}`);
      }
      await nextFrame();
    }
  }

  /** Until every cell in view is there. */
  const isWhole = () => {
    const stats = table()?.stats();
    return stats !== undefined && stats.cellsDrawn > 0 && stats.missingCells === 0;
  };
  const isOpen = () =>
    document.querySelector('[data-testid="table-view"]')?.getAttribute("data-status") === "open";

  /** Sees how long it is between frames, and which tasks are long, until it is stopped. */
  function watchFrames() {
    const gaps = [];
    const tasks = [];
    let isOver = false;
    const observer = PerformanceObserver.supportedEntryTypes.includes("longtask")
      ? new PerformanceObserver((list) => tasks.push(...list.getEntries().map((one) => one.duration)))
      : null;
    observer?.observe({ type: "longtask" });
    void (async () => {
      let last = await nextFrame();
      while (!isOver) {
        const now = await nextFrame();
        gaps.push(now - last);
        last = now;
      }
    })();
    return () => {
      isOver = true;
      observer?.disconnect();
      return { gaps, longTasks: observer ? tasks : null };
    };
  }

  /** Starts with the click on the button of the dialog, and ends with a table in view. */
  async function importTable(name) {
    const button = document.querySelector('[data-testid="table-import-start"]');
    await until("the dialog", () => button.getAttribute("aria-disabled") !== "true");
    const known = new Set(hooks().tableEntries().map(({ id }) => id));
    const stop = watchFrames();
    const started = performance.now();
    button.click();
    const added = () => hooks().tableEntries().find(({ id }) => !known.has(id));
    await until(
      "the import",
      () => {
        const failure = document.querySelector('[data-testid="table-import-failure"]');
        if (failure) {
          throw new Error(failure.textContent);
        }
        return added() !== undefined;
      },
      3_600_000,
    );
    const ms = performance.now() - started;
    const { gaps, longTasks } = stop();
    document.querySelector('[data-testid="table-import-show"]')?.click();
    await until("the table", () => isOpen() && isWhole());
    const entry = added();
    return { name, ...entry, ms, shownMs: performance.now() - started, gaps, longTasks };
  }

  /** Opens a table anew, with nothing kept from before. */
  async function open(id) {
    let ms = 0;
    for (const isAgain of [false, true]) {
      hooks().showTable(null);
      await until("the worksheet", () => table() === null);
      const started = performance.now();
      hooks().showTable(id);
      await until("the table", () => isOpen() && isWhole());
      ms = performance.now() - started;
      // It is opened the way it was left, which would be the last question.
      if (!isAgain) {
        await ask({ filters: [], order: [] });
      }
    }
    return ms;
  }

  /** How long a question takes until it is answered, and until the rows are on the screen. */
  async function ask(query) {
    const started = performance.now();
    const isShown = await table().ask(query);
    const ms = performance.now() - started;
    // What has come is drawn in the frame after.
    await nextFrame();
    await until("the cells", isWhole);
    const refused = document.querySelector('[data-testid="table-refused"] span');
    return {
      isShown,
      ms,
      shownMs: performance.now() - started,
      rows: table().stats().sheet.rows,
      refused: isShown ? null : (refused?.textContent ?? "nothing is said"),
    };
  }

  async function timed(answer) {
    const started = performance.now();
    const value = await answer;
    return { ms: performance.now() - started, value };
  }

  /** The values of a column, and how long it takes to list them. */
  async function values(column, limit = 1_000) {
    const { ms, value } = await timed(table().client().values(column, "", limit));
    return { ms, count: value.values.length, first: value.values.slice(0, 20) };
  }

  async function summary() {
    const columns = Array.from({ length: table().stats().sheet.cols }, (_, column) => column);
    const { ms } = await timed(table().client().summary(columns));
    return { ms };
  }

  async function memory() {
    return table().client().memory();
  }

  /** Has the table scrolled at a speed for a while, and sees every frame of it. */
  async function scroll({ renderer, seconds, screensPerSecond }) {
    const work = [];
    const interval = [];
    let placeholderFrames = 0;
    let otherFrames = 0;
    let top = 0;
    table().scrollTo({ top });
    await nextFrame();
    await until("the cells", isWhole);
    let { frames: drawn, renderer: before } = table().stats();
    const started = await nextFrame();
    for (let last = started, isFirst = true; ; isFirst = false) {
      const now = await nextFrame();
      // The grid has drawn this frame already: its callback was asked for first.
      const stats = table().stats();
      if (!isFirst) {
        if (stats.frames !== drawn + 1) {
          throw new Error(`the grid drew ${stats.frames - drawn} frames in one`);
        }
        work.push(stats.frameWork.last);
        interval.push(now - last);
        // Bars stand for cells whose pages are missing, and with WebGPU for texts
        // that the frame had no time for.
        const bars =
          (stats.renderer.counters?.deferredFrames ?? 0) - (before.counters?.deferredFrames ?? 0);
        placeholderFrames += stats.missingCells > 0 || bars > 0 ? 1 : 0;
        otherFrames += stats.renderer.kind === renderer ? 0 : 1;
      }
      drawn = stats.frames;
      before = stats.renderer;
      if (now - started >= seconds * 1000) {
        return { work, interval, placeholderFrames, otherFrames };
      }
      const screen = (stats.visible.endRow - stats.visible.firstRow) * stats.metrics.rowHeight;
      top += (screensPerSecond * screen * (now - last)) / 1000;
      if (top >= stats.scrollLimits.top) {
        throw new Error("the table is too short to be scrolled for so long");
      }
      table().scrollTo({ top });
      last = now;
    }
  }

  /** Jumps anywhere in the table, and waits for what is there. */
  async function jumps(positions) {
    const filled = [];
    for (const position of positions) {
      const { scrollLimits } = table().stats();
      const started = performance.now();
      table().scrollTo({ top: Math.floor(position * scrollLimits.top) });
      await nextFrame();
      await until("the cells", isWhole);
      filled.push(performance.now() - started);
    }
    return filled;
  }

  /** Asks a question and jumps about in what is shown until it is answered: how long pages take. */
  async function jumpsDuring(query, positions) {
    const trips = [];
    let isOver = false;
    const started = performance.now();
    const answer = table()
      .ask(query)
      .finally(() => {
        isOver = true;
      });
    for (const position of positions) {
      if (isOver) {
        break;
      }
      const from = traffic.length;
      table().scrollTo({ top: Math.floor(position * table().stats().scrollLimits.top) });
      const gaveUp = performance.now() + 1_000;
      const trip = () => {
        const asked = traffic.findIndex((one, at) => at >= from && !one.isPage);
        const came = asked < 0 ? -1 : traffic.findIndex((one, at) => at > asked && one.isPage);
        return came < 0 ? null : traffic[came].at - traffic[asked].at;
      };
      while (!isOver && trip() === null && performance.now() < gaveUp) {
        await nextFrame();
      }
      // What came after the answer is of the new view.
      if (!isOver) {
        trips.push(trip() ?? Number.POSITIVE_INFINITY);
      }
      await nextFrame();
    }
    await answer;
    return { trips, ms: performance.now() - started };
  }

  window.__b14 = {
    ask,
    importTable,
    jumps,
    jumpsDuring,
    memory,
    open,
    scroll,
    summary,
    until,
    values,
    isWhole,
    isOpen,
  };
}
