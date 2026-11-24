import { alphaOf } from "./color.ts";
import type { PixelBox, RectList } from "./rects.ts";
import { bandsOf, type Scene } from "./scene.ts";
import type { TextRuns } from "./text-layout.ts";
import type { TextMeasure } from "./text-measure.ts";
import type { FrameMasks } from "./types.ts";

/** The masks of a scene, from the same primitives that are painted. */
export function masksOfScene(scene: Scene, measure: TextMeasure): FrameMasks {
  const { width, height, look } = scene;
  const text = new Uint8Array(width * height);
  const translucent = new Uint8Array(width * height);
  const screen: PixelBox = { left: 0, top: 0, right: width, bottom: height };

  /** Visits each row of the pixels that are in every one of the boxes. */
  const visit = (boxes: readonly PixelBox[], row: (start: number, end: number) => void): void => {
    const left = Math.max(...boxes.map((box) => box.left));
    const top = Math.max(...boxes.map((box) => box.top));
    const right = Math.min(...boxes.map((box) => box.right));
    const bottom = Math.min(...boxes.map((box) => box.bottom));
    for (let y = top; left < right && y < bottom; y++) {
      row(y * width + left, y * width + right);
    }
  };

  const cover = (boxes: readonly PixelBox[]): void =>
    visit(boxes, (start, end) => {
      for (let pixel = start; pixel < end; pixel++) {
        translucent[pixel] = Math.min(255, (translucent[pixel] ?? 0) + 1);
      }
    });

  const rectangles = (
    rects: RectList,
    first: number,
    end: number,
    x: number,
    y: number,
    clip: PixelBox,
  ): void => {
    for (let index = first; index < end; index++) {
      if (alphaOf(rects.colors[index] ?? 0) < 255) {
        const left = x + (rects.geometry[index * 4] ?? 0);
        const top = y + (rects.geometry[index * 4 + 1] ?? 0);
        const right = left + (rects.geometry[index * 4 + 2] ?? 0);
        const bottom = top + (rects.geometry[index * 4 + 3] ?? 0);
        cover([{ left, top, right, bottom }, clip, screen]);
      }
    }
  };

  const texts = (
    runs: TextRuns,
    first: number,
    end: number,
    x: number,
    y: number,
    clip: PixelBox,
    firstRow: number,
    endRow: number,
  ): void => {
    for (let run = first; run < end; run++) {
      const row = runs.row[run] ?? 0;
      if (row < firstRow || row >= endRow) {
        continue;
      }
      const ink = measure.ink(look.fonts[runs.font[run] ?? 0] ?? "", runs.texts[run] ?? "");
      const pen = x + (runs.x[run] ?? 0);
      const baseline = y + (runs.baseline[run] ?? 0);
      // One more pixel all round, for what antialiasing adds.
      const boxes = [
        {
          left: Math.floor(pen - ink.left) - 1,
          top: Math.floor(baseline - ink.ascent) - 1,
          right: Math.ceil(pen + ink.right) + 1,
          bottom: Math.ceil(baseline + ink.descent) + 1,
        },
        clip,
        screen,
      ];
      if (runs.clipped[run] === 1) {
        const [left = 0, top = 0, right = 0, bottom = 0] = runs.clip.subarray(run * 4);
        boxes.push({ left: x + left, top: y + top, right: x + right, bottom: y + bottom });
      }
      visit(boxes, (start, end) => text.fill(1, start, end));
    }
  };

  for (const step of scene.steps) {
    switch (step.kind) {
      case "rects":
        rectangles(scene.rects, step.first, step.end, 0, 0, screen);
        break;
      case "fills":
      case "borders": {
        const rects = step.layout[step.kind];
        rectangles(rects, 0, rects.length, step.x, step.y, step.clip);
        break;
      }
      case "texts": {
        const bands = bandsOf(step);
        for (let band = bands.first; band < bands.end; band++) {
          const { first, end, firstStrike, endStrike } = step.layout.band(band);
          const runs = step.layout.texts;
          texts(runs, first, end, step.x, step.y, step.clip, step.firstRow, step.endRow);
          rectangles(runs.strikes, firstStrike, endStrike, step.x, step.y, step.clip);
        }
        break;
      }
      case "labels":
        texts(step.runs, 0, step.runs.length, 0, 0, screen, 0, Number.POSITIVE_INFINITY);
        break;
      case "sprite": {
        // The edges of a sprite are translucent, and only a copy of it knows where.
        const { x, y, clip, spec } = step;
        cover([{ left: x, top: y, right: x + spec.width, bottom: y + spec.height }, clip, screen]);
        break;
      }
    }
  }
  return { text, translucent };
}
