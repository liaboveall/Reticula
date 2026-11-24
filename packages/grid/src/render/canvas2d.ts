import { Colors, cssColor, serializer } from "./color.ts";
import type { Frame, FrameCounts } from "./frame.ts";
import { isEmptyBox, type PixelBox, type RectList } from "./rects.ts";
import { bandsOf, type PageStep, type Scene, SceneBuilder } from "./scene.ts";
import { SpriteCache } from "./sprites.ts";
import type { TextRuns } from "./text-layout.ts";
import { resetText, TextMeasurer, textContext } from "./text-measure.ts";
import type { GridRenderer } from "./types.ts";

export interface Canvas2dOptions {
  /** Shared with whoever else measures the sheet's text. */
  readonly measure?: TextMeasurer;
  /** Called when a context that was lost is back, with nothing left on the canvas. */
  readonly onRestored?: () => void;
}

/** Draws frames with the 2D context of a canvas, pixel for pixel: nothing is scaled. */
export class Canvas2dRenderer implements GridRenderer {
  readonly kind = "canvas2d";
  readonly canvas: HTMLCanvasElement;
  #context: CanvasRenderingContext2D;
  #options: Canvas2dOptions;
  #measure: TextMeasurer;
  #scenes: SceneBuilder;
  #sprites = new SpriteCache();
  #lost = false;
  /** Font and colour the context is set to, as far as text goes; -1 when that is not known. */
  #font = -1;
  #color = -1;

  constructor(options: Canvas2dOptions = {}) {
    this.#options = options;
    this.canvas = document.createElement("canvas");
    this.#context = textContext(this.canvas);
    this.#measure = options.measure ?? new TextMeasurer();
    this.#scenes = new SceneBuilder(this.#measure, new Colors(serializer(this.#context)));
    // What a lost context does by default is come back, unless told not to.
    this.canvas.addEventListener("contextlost", this.#onLost);
    this.canvas.addEventListener("contextrestored", this.#onRestored);
  }

  resize(width: number, height: number): void {
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
    }
  }

  draw(frame: Frame): FrameCounts {
    const context = this.#context;
    // Resizing the canvas and losing the context undo this.
    resetText(context);
    this.#font = -1;
    this.#color = -1;
    const scene = this.#scenes.build(frame);
    let clipped = 0;
    let view: PixelBox | null = null;
    for (const step of this.#lost ? [] : scene.steps) {
      // The pages of a part of the sheet are cut to it together.
      const next = step.kind === "texts" ? step.clip : null;
      if (next !== view) {
        if (view) {
          context.restore();
        }
        if (next) {
          this.#clipTo(next);
        }
        view = next;
        this.#font = -1;
        this.#color = -1;
      }
      switch (step.kind) {
        case "rects":
          this.#rects(scene.rects, step.first, step.end);
          break;
        case "fills":
        case "borders": {
          const rects = step.layout[step.kind];
          this.#pageRects(rects, 0, rects.length, step);
          break;
        }
        case "texts":
          clipped += this.#pageTexts(scene, step);
          break;
        case "labels":
          clipped += this.#texts(scene, step.runs, 0, step.runs.length, 0, 0, null);
          break;
        case "sprite": {
          const { x, y, clip, spec } = step;
          const left = Math.max(x, clip.left);
          const top = Math.max(y, clip.top);
          const width = Math.min(x + spec.width, clip.right) - left;
          const height = Math.min(y + spec.height, clip.bottom) - top;
          if (width > 0 && height > 0) {
            const sprite = this.#sprites.get(spec);
            context.drawImage(sprite, left - x, top - y, width, height, left, top, width, height);
          }
          break;
        }
      }
    }
    if (view) {
      context.restore();
    }
    return { cells: scene.cells, clipped, missing: scene.missing };
  }

  destroy(): void {
    this.canvas.removeEventListener("contextlost", this.#onLost);
    this.canvas.removeEventListener("contextrestored", this.#onRestored);
    this.canvas.remove();
  }

  #onLost = (): void => {
    this.#lost = true;
  };

  #onRestored = (): void => {
    this.#lost = false;
    this.#measure.reset();
    this.#scenes.reset();
    this.#sprites.reset();
    this.#options.onRestored?.();
  };

  #rects(rects: RectList, first: number, end: number): void {
    const context = this.#context;
    const { geometry, colors } = rects;
    let current = -1;
    for (let index = first; index < end; index++) {
      const color = colors[index] ?? 0;
      if (color !== current) {
        context.fillStyle = cssColor(color);
        current = color;
      }
      const at = index * 4;
      context.fillRect(
        geometry[at] ?? 0,
        geometry[at + 1] ?? 0,
        geometry[at + 2] ?? 0,
        geometry[at + 3] ?? 0,
      );
    }
  }

  #pageRects(rects: RectList, first: number, end: number, step: PageStep): void {
    const context = this.#context;
    const { geometry, colors } = rects;
    const { clip } = step;
    let current = -1;
    for (let index = first; index < end; index++) {
      const at = index * 4;
      const x = step.x + (geometry[at] ?? 0);
      const y = step.y + (geometry[at + 1] ?? 0);
      const left = Math.max(clip.left, x);
      const top = Math.max(clip.top, y);
      const right = Math.min(clip.right, x + (geometry[at + 2] ?? 0));
      const bottom = Math.min(clip.bottom, y + (geometry[at + 3] ?? 0));
      if (left >= right || top >= bottom) {
        continue;
      }
      const color = colors[index] ?? 0;
      if (color !== current) {
        context.fillStyle = cssColor(color);
        current = color;
      }
      context.fillRect(left, top, right - left, bottom - top);
    }
  }

  #pageTexts(scene: Scene, step: PageStep): number {
    const { layout } = step;
    const { texts } = layout;
    const bands = bandsOf(step);
    let clipped = 0;
    for (let index = bands.first; index < bands.end; index++) {
      const { first, end, firstStrike, endStrike } = layout.band(index);
      clipped += this.#texts(scene, texts, first, end, step.x, step.y, step);
      if (firstStrike < endStrike) {
        this.#pageRects(texts.strikes, firstStrike, endStrike, step);
        this.#color = -1;
      }
    }
    return clipped;
  }

  #clipTo(box: PixelBox): void {
    const context = this.#context;
    context.save();
    context.beginPath();
    context.rect(box.left, box.top, box.right - box.left, box.bottom - box.top);
    context.clip();
  }

  /**
   * Draws some runs, or those of them that are in view of a page's step.
   * Returns how many needed a clip of their own.
   */
  #texts(
    scene: Scene,
    runs: TextRuns,
    first: number,
    end: number,
    x: number,
    y: number,
    step: PageStep | null,
  ): number {
    const context = this.#context;
    const { fonts, lineHeights } = scene.look;
    const { texts } = runs;
    // Pixels of a text may reach past its ends, if by less than its height.
    const slack = lineHeights[0] ?? 0;
    const firstRow = step?.firstRow ?? 0;
    const endRow = step?.endRow ?? Number.POSITIVE_INFINITY;
    const least = step ? step.clip.left - slack : Number.NEGATIVE_INFINITY;
    const most = step ? step.clip.right + slack : Number.POSITIVE_INFINITY;
    let clipped = 0;
    // Every index below is that of a run. Saying what to do without one, for each
    // of a thousand texts, would cost about as much as issuing their drawing.
    for (let run = first; run < end; run++) {
      const row = runs.row[run] as number;
      const left = x + (runs.x[run] as number);
      if (
        row < firstRow ||
        row >= endRow ||
        left >= most ||
        left + (runs.width[run] as number) <= least
      ) {
        continue;
      }
      const font = runs.font[run] as number;
      if (font !== this.#font) {
        context.font = fonts[font] ?? "";
        this.#font = font;
      }
      const color = runs.color[run] as number;
      if (color !== this.#color) {
        context.fillStyle = cssColor(color);
        this.#color = color;
      }
      const text = texts[run] as string;
      const baseline = y + (runs.baseline[run] as number);
      if (runs.clipped[run] !== 1) {
        context.fillText(text, left, baseline);
        continue;
      }
      const at = run * 4;
      const box: PixelBox = {
        left: x + (runs.clip[at] ?? 0),
        top: y + (runs.clip[at + 1] ?? 0),
        right: x + (runs.clip[at + 2] ?? 0),
        bottom: y + (runs.clip[at + 3] ?? 0),
      };
      if (
        isEmptyBox(box) ||
        (step && (box.left >= step.clip.right || box.right <= step.clip.left))
      ) {
        continue;
      }
      this.#clipTo(box);
      context.fillText(text, left, baseline);
      context.restore();
      clipped++;
    }
    return clipped;
  }
}
