import { cssColor, type PackedColor } from "./color.ts";
import { textContext } from "./text-measure.ts";

/** A small picture that is drawn once with a 2D context and then copied. */
export type SpriteSpec =
  | {
      /** The mark in the corner where the headers meet. */
      readonly kind: "corner";
      readonly width: number;
      readonly height: number;
      readonly color: PackedColor;
    }
  | {
      /** The sign on a filter's button: a funnel while the filter is in force. */
      readonly kind: "chevron" | "funnel";
      readonly width: number;
      readonly height: number;
      readonly scale: number;
      readonly color: PackedColor;
    }
  | {
      /** A collaborator's name. */
      readonly kind: "plate";
      readonly width: number;
      readonly height: number;
      readonly scale: number;
      readonly text: string;
      readonly font: string;
      readonly padding: number;
      readonly fill: PackedColor;
      readonly stroke: PackedColor;
      readonly color: PackedColor;
    };

/** Equal for sprites that look the same. */
export function spriteKey(spec: SpriteSpec): string {
  return JSON.stringify(Object.values(spec));
}

/** Pictures kept; a document has a few dozen at most. */
const MAX_SPRITES = 256;

function path(context: CanvasRenderingContext2D, points: readonly (readonly [number, number])[]) {
  context.beginPath();
  for (const [x, y] of points) {
    context.lineTo(x, y);
  }
  context.closePath();
}

/** Draws a sprite into a canvas of its size. */
export function rasterize(spec: SpriteSpec): HTMLCanvasElement {
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, spec.width);
  canvas.height = Math.max(1, spec.height);
  const context = textContext(canvas);
  context.fillStyle = cssColor(spec.kind === "plate" ? spec.fill : spec.color);
  if (spec.kind === "corner") {
    path(context, [
      [spec.width, 0],
      [spec.width, spec.height],
      [0, spec.height],
    ]);
    context.fill();
  } else if (spec.kind === "plate") {
    const { width, height, scale, padding } = spec;
    const line = Math.max(1, Math.round(scale));
    context.beginPath();
    context.roundRect(line / 2, line / 2, width - line, height - line, 5 * scale);
    context.fill();
    context.lineWidth = line;
    context.strokeStyle = cssColor(spec.stroke);
    context.stroke();
    context.font = spec.font;
    context.fillStyle = cssColor(spec.color);
    context.textBaseline = "middle";
    context.fillText(spec.text, padding, height / 2, width - 2 * padding);
  } else {
    const { scale } = spec;
    const x = spec.width / 2;
    const y = spec.height / 2;
    const points: readonly (readonly [number, number])[] =
      spec.kind === "funnel"
        ? [
            [-4, -3],
            [4, -3],
            [1.2, 0.5],
            [1.2, 3.5],
            [-1.2, 3.5],
            [-1.2, 0.5],
          ]
        : [
            [-3.5, -2],
            [3.5, -2],
            [0, 2.5],
          ];
    path(
      context,
      points.map(([dx, dy]) => [x + dx * scale, y + dy * scale] as const),
    );
    context.fill();
  }
  return canvas;
}

/** The sprites drawn so far; the ones unused for longest make room for new ones. */
export class SpriteCache {
  #drawn = new Map<string, HTMLCanvasElement>();

  get(spec: SpriteSpec): HTMLCanvasElement {
    const key = spriteKey(spec);
    let sprite = this.#drawn.get(key);
    if (sprite) {
      this.#drawn.delete(key);
    } else {
      sprite = rasterize(spec);
      const oldest = this.#drawn.keys().next();
      if (this.#drawn.size >= MAX_SPRITES && !oldest.done) {
        this.#drawn.delete(oldest.value);
      }
    }
    this.#drawn.set(key, sprite);
    return sprite;
  }

  reset(): void {
    this.#drawn.clear();
  }
}
