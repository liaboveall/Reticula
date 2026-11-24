import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { Canvas2dRenderer } from "./canvas2d.ts";
import { axes, cacheOf, frameOf, measure, theme } from "./fixtures.ts";

interface Call {
  readonly name: string;
  readonly args: readonly unknown[];
  readonly font: string;
  readonly fillStyle: string;
  readonly fontKerning: string;
}

/** A canvas that writes down what is drawn on it, and forgets its state when it is resized. */
class FakeCanvas extends EventTarget {
  readonly calls: Call[] = [];
  readonly options: unknown[] = [];
  readonly context: Record<string, unknown>;
  #width = 300;
  #height = 150;

  constructor() {
    super();
    const { calls } = this;
    const context: Record<string, unknown> = {
      measureText(text: string) {
        calls.push(call("measureText", [text]));
        const line = measure.line(String(context.font));
        return {
          width: measure.width(String(context.font), text),
          fontBoundingBoxAscent: line.ascent,
          fontBoundingBoxDescent: line.descent,
        };
      },
    };
    const call = (name: string, args: readonly unknown[]): Call => ({
      name,
      args,
      font: String(context.font),
      fillStyle: String(context.fillStyle),
      fontKerning: String(context.fontKerning),
    });
    for (const name of [
      "beginPath",
      "clip",
      "closePath",
      "drawImage",
      "fill",
      "fillRect",
      "fillText",
      "lineTo",
      "rect",
      "restore",
      "save",
    ]) {
      context[name] = (...args: unknown[]) => calls.push(call(name, args));
    }
    this.context = context;
    this.#reset();
  }

  #reset(): void {
    Object.assign(this.context, {
      font: "10px sans-serif",
      fillStyle: "#000000",
      fontKerning: "auto",
      textBaseline: "alphabetic",
      textAlign: "start",
      direction: "inherit",
    });
  }

  get width(): number {
    return this.#width;
  }

  set width(value: number) {
    this.#width = value;
    this.#reset();
  }

  get height(): number {
    return this.#height;
  }

  set height(value: number) {
    this.#height = value;
    this.#reset();
  }

  getContext(_kind: string, options: unknown) {
    this.options.push(options);
    return this.context;
  }

  remove(): void {}

  named(name: string): Call[] {
    return this.calls.filter((call) => call.name === name);
  }
}

let canvases: FakeCanvas[] = [];

beforeEach(() => {
  canvases = [];
  vi.stubGlobal("document", {
    createElement: () => {
      const canvas = new FakeCanvas();
      canvases.push(canvas);
      return canvas;
    },
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function open(onRestored?: () => void) {
  const renderer = new Canvas2dRenderer(onRestored ? { onRestored } : {});
  renderer.resize(525, 285);
  const [canvas] = canvases;
  if (!canvas) {
    throw new Error("no canvas was made");
  }
  return { renderer, canvas };
}

const CELLS = { "1:1": "left", "2:1": "a text that is too long for it", "2:3": "7" };
const frame = frameOf({ scale: 1.5, cache: cacheOf(CELLS, 1, 1) });

describe("the canvas", () => {
  test("has alpha, so that its text is grey, and is painted opaque all the same", () => {
    const { renderer, canvas } = open();
    renderer.draw(frame);
    expect(canvas.options[0]).toEqual({ alpha: true });
    const [ground] = canvas.named("fillRect");
    expect(ground).toMatchObject({ args: [0, 0, 525, 285], fillStyle: theme.background });
  });

  test("is drawn on in whole device pixels, without a transform", () => {
    const { renderer, canvas } = open();
    renderer.draw(frame);
    expect(canvas.named("setTransform")).toEqual([]);
    for (const name of ["fillRect", "rect", "drawImage"]) {
      expect(canvas.named(name).length).toBeGreaterThan(0);
      for (const call of canvas.named(name)) {
        for (const arg of call.args.filter((value) => typeof value === "number")) {
          expect(Number.isInteger(arg)).toBe(true);
        }
      }
    }
  });

  test("is given its size only when that has changed, as that clears it", () => {
    const { renderer, canvas } = open();
    canvas.context.fillStyle = "#123456";
    renderer.resize(525, 285);
    expect(canvas.context.fillStyle).toBe("#123456");
    renderer.resize(526, 285);
    expect(canvas.context.fillStyle).toBe("#000000");
  });
});

describe("text", () => {
  test("is drawn unkerned in its size on the screen, also after the canvas was resized", () => {
    const { renderer, canvas } = open();
    renderer.draw(frame);
    renderer.resize(530, 290);
    renderer.draw(frame);
    const texts = canvas.named("fillText");
    expect(texts.filter((call) => call.args[0] === "left")).toHaveLength(2);
    for (const call of texts) {
      expect(call.fontKerning).toBe("none");
      expect(call.font).toBe("19.5px sans-serif");
    }
    expect(canvas.context).toMatchObject({ textBaseline: "alphabetic", textAlign: "left" });
  });

  test("has its font and colour in every frame, also when only the headers have text", () => {
    const { renderer, canvas } = open();
    const empty = frameOf({ scale: 1.5 });
    renderer.draw(empty);
    const first = canvas.named("fillText").length;
    expect(first).toBeGreaterThan(0);
    renderer.draw(empty);
    renderer.resize(526, 286);
    renderer.draw(empty);
    const texts = canvas.named("fillText");
    expect(texts).toHaveLength(3 * first);
    for (const call of texts) {
      expect(call.font).toBe("19.5px sans-serif");
      expect(call.fillStyle).toBe(theme.headerText);
    }
  });

  test("is measured on a canvas apart, unkerned as well", () => {
    const { renderer, canvas } = open();
    renderer.draw(frame);
    expect(canvas.named("measureText")).toEqual([]);
    const measured = canvases.flatMap((other) => other.named("measureText"));
    expect(measured.length).toBeGreaterThan(0);
    expect(measured.every((call) => call.fontKerning === "none")).toBe(true);
  });

  test("is drawn at a pen that is not rounded, on a baseline that is", () => {
    const { renderer, canvas } = open();
    renderer.draw(frameOf({ cache: cacheOf({ "1:1": { text: "8.15", flags: 1 } }, 1, 1) }));
    const [, x, y] = canvas.named("fillText").find((call) => call.args[0] === "8.15")?.args ?? [];
    expect(x).toBeCloseTo(45 + 150 - 6 - measure.width("13px sans-serif", "8.15"), 9);
    expect(Number.isInteger(x)).toBe(false);
    expect(Number.isInteger(y)).toBe(true);
  });

  test("is clipped where it is cut, and all of it to its part of the sheet", () => {
    const { renderer, canvas } = open();
    const { clipped } = renderer.draw(frame);
    expect(clipped).toBeGreaterThan(0);
    const clips = canvas.named("rect").map((call) => call.args);
    const left = 68 + Math.round(75 * 1.5);
    expect(clips).toContainEqual([68, 45, Math.round(340 * 1.5) - 68, Math.round(180 * 1.5) - 45]);
    expect(clips).toContainEqual([left, 105, Math.round(225 * 1.5) - 9 + 68 - left, 30]);
    expect(canvas.named("save")).toHaveLength(canvas.named("restore").length);
  });

  test("that is out of view is not drawn", () => {
    const cells: Record<string, string> = {};
    for (let row = 0; row < 64; row++) {
      cells[`${row}:0`] = `row ${row}`;
      cells[`${row}:15`] = `far ${row}`;
    }
    const { renderer, canvas } = open();
    const layout = axes();
    renderer.draw(frameOf({ cache: cacheOf(cells, 1, 1), layout, scroll: { top: 400, left: 0 } }));
    const drawn = canvas
      .named("fillText")
      .map((call) => String(call.args[0]))
      .filter((text) => text.includes(" "));
    expect(drawn).toEqual([
      "row 20",
      "row 21",
      "row 22",
      "row 23",
      "row 24",
      "row 25",
      "row 26",
      "row 27",
    ]);
  });
});

describe("a lost context", () => {
  test("is left to come back, and everything is made again when it has", () => {
    const onRestored = vi.fn();
    const { renderer, canvas } = open(onRestored);
    renderer.draw(frame);
    const before = canvases.length;
    renderer.draw(frame);
    expect(canvases).toHaveLength(before);

    const lost = new Event("contextlost", { cancelable: true });
    canvas.dispatchEvent(lost);
    expect(lost.defaultPrevented).toBe(false);
    expect(onRestored).not.toHaveBeenCalled();
    const texts = canvas.named("fillText").length;
    expect(renderer.draw(frame).cells).toBe(3);
    expect(canvas.named("fillText")).toHaveLength(texts);
    canvas.dispatchEvent(new Event("contextrestored"));
    expect(onRestored).toHaveBeenCalledOnce();

    renderer.draw(frame);
    // The corner's mark is drawn anew, and all the text.
    expect(canvases).toHaveLength(before + 1);
    expect(canvas.named("fillText").length).toBeGreaterThan(texts);
  });

  test("is of no interest once the renderer is gone", () => {
    const onRestored = vi.fn();
    const { renderer, canvas } = open(onRestored);
    renderer.destroy();
    canvas.dispatchEvent(new Event("contextrestored"));
    expect(onRestored).not.toHaveBeenCalled();
  });
});
