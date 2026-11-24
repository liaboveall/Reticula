import { describe, expect, test } from "vitest";
import { axes, cacheOf, frameOf, measure, PLAIN, sceneOf } from "./fixtures.ts";
import type { Frame } from "./frame.ts";
import { masksOfScene } from "./masks.ts";

const masksOf = (frame: Frame) => {
  const scene = sceneOf(frame);
  const masks = masksOfScene(scene, measure);
  const at = (mask: Uint8Array) => (x: number, y: number) => mask[y * scene.width + x];
  return { scene, masks, text: at(masks.text), translucent: at(masks.translucent) };
};

describe("the mask of texts", () => {
  test("is the box of their ink and a pixel around it", () => {
    const frame = frameOf({ cache: cacheOf({ "2:1": "ink" }, 1, 1), scale: 1.5 });
    const { text } = masksOf(frame);
    const font = "19.5px sans-serif";
    const left = Math.round(45 * 1.5) + Math.round(75 * 1.5) + 9;
    const right = Math.ceil(left + measure.width(font, "ink"));
    const baseline = Math.round(45 + 2.5 * 20 * 1.5 + ((1.08 - 0.25) * 19.5) / 2);
    expect(text(left, baseline)).toBe(1);
    expect(text(left - 1, baseline)).toBe(1);
    expect(text(left - 2, baseline)).toBe(0);
    expect(text(right, baseline)).toBe(1);
    expect(text(right + 1, baseline)).toBe(0);
    expect(text(left, Math.floor(baseline - 0.8 * 19.5) - 1)).toBe(1);
    expect(text(left, Math.floor(baseline - 0.8 * 19.5) - 2)).toBe(0);
  });

  test("ends where the texts are cut", () => {
    const long = "a text that is cut by the number after it";
    const frame = frameOf({ cache: cacheOf({ "2:0": long, "2:1": "7" }, 1, 1) });
    const { text } = masksOf(frame);
    expect(text(45 + 75 - 7, 80)).toBe(1);
    expect(text(45 + 75 - 6, 80)).toBe(0);
    const frozen = masksOf({ ...frame, layout: axes([], [], 0, 1) });
    expect(frozen.text(45 + 75 - 7, 80)).toBe(1);
  });

  test("has the labels of the headers", () => {
    const { text } = masksOf(frameOf());
    expect(text(45 + 37, 18)).toBe(1);
    expect(text(22, 42)).toBe(1);
    expect(text(45 + 37, 42)).toBe(0);
  });
});

describe("the count of translucent rectangles", () => {
  test("is zero where everything is opaque", () => {
    const styles = [PLAIN, { ...PLAIN, fillColor: { kind: "rgb" as const, rgb: 0x123456 } }];
    const { masks } = masksOf(
      frameOf({ cache: cacheOf({ "2:1": { text: "", style: 1 } }, 1, 1), styles }),
    );
    // But for the mark in the corner, whose edges are.
    const covered = [...masks.translucent.entries()].filter(([, count]) => count > 0);
    expect(covered).toHaveLength(64);
    expect(
      covered.every(([pixel, count]) => count === 1 && pixel % 350 < 45 && pixel < 350 * 30),
    ).toBe(true);
  });

  test("is how many there are at each pixel", () => {
    const { translucent } = masksOf(
      frameOf({
        selection: { row: 1, col: 1 },
        selectionRange: { firstRow: 1, endRow: 4, firstCol: 1, endCol: 3 },
        formulaReferences: [{ colorIndex: 2, firstRow: 2, lastRow: 3, firstCol: 2, lastCol: 3 }],
        diffHighlights: new Map([["3:2", "added"]]),
      }),
    );
    // Cells 1:1, 2:1, 2:2, 3:2 and 3:3, below their outlines.
    expect(translucent(150, 60)).toBe(1);
    expect(translucent(150, 80)).toBe(1);
    expect(translucent(230, 80)).toBe(2);
    expect(translucent(230, 100)).toBe(3);
    expect(translucent(300, 100)).toBe(1);
    expect(translucent(150, 130)).toBe(0);
  });
});
