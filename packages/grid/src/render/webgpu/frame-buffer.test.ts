import { describe, expect, test } from "vitest";
import { packColor } from "../color.ts";
import { RectList } from "../rects.ts";
import { FakeDevice, quadsOf } from "./fakes.ts";
import { ANY_ROW, FrameBuffer, InstanceWriter } from "./frame-buffer.ts";
import { INSTANCE_BYTES } from "./pipeline.ts";

const PLAIN = { u: 0, v: 0, row: ANY_ROW, textured: false, layer: 0 };

describe("quads", () => {
  test("have the layout the shader reads", () => {
    const writer = new InstanceWriter(1);
    writer.rect(-5, 70_000, 30, 2, packColor(1, 2, 3, 200), 9);
    writer.sprite({ layer: 3, u: 2000, v: 1500, width: 12, height: 20 }, 7, -8, null, 63);
    expect(quadsOf(writer.bytes())).toEqual([
      { ...PLAIN, x: -5, y: 70_000, width: 30, height: 2, color: 0xc8030201, row: 9 },
      {
        x: 7,
        y: -8,
        width: 12,
        height: 20,
        u: 2000,
        v: 1500,
        color: 0,
        row: 63,
        textured: true,
        layer: 3,
      },
    ]);
  });

  test("of sprites show the part of the picture inside the clip", () => {
    const writer = new InstanceWriter();
    const sprite = { layer: 0, u: 100, v: 50, width: 10, height: 20 };
    const clip = { left: 3, top: 5, right: 9, bottom: 12 };
    writer.sprite(sprite, 0, 0, clip);
    writer.sprite(sprite, 5, 10, clip);
    for (const [x, y] of [
      [9, 0],
      [-10, 0],
      [4, 12],
      [4, -20],
    ] as const) {
      writer.sprite(sprite, x, y, clip);
    }
    expect(
      quadsOf(writer.bytes()).map(({ x, y, width, height, u, v }) => [x, y, width, height, u, v]),
    ).toEqual([
      [3, 5, 6, 7, 103, 55],
      [5, 10, 4, 2, 100, 50],
    ]);
  });

  test("are no longer than sixteen bits say, and not there if nothing can be seen of them", () => {
    const writer = new InstanceWriter();
    writer.rect(0, 0, 5, 150_000, packColor(0, 0, 0));
    writer.rect(0, 0, 5, 5, packColor(9, 9, 9, 0));
    const quads = quadsOf(writer.bytes());
    expect(quads.map(({ y, height }) => [y, height])).toEqual([
      [0, 65_535],
      [65_535, 65_535],
      [131_070, 18_930],
    ]);
  });

  test("of a list are in its order", () => {
    const list = new RectList(1);
    list.push(1, 2, 3, 4, packColor(10, 0, 0));
    list.push(5, 6, 7, 8, packColor(20, 0, 0));
    list.push(9, 9, 9, 9, packColor(30, 0, 0));
    const writer = new InstanceWriter();
    writer.rects(list, 1, 3);
    expect(quadsOf(writer.bytes()).map(({ x, color }) => [x, color & 0xff])).toEqual([
      [5, 20],
      [9, 30],
    ]);
    expect(writer.bytes(1)).toHaveLength(INSTANCE_BYTES);
  });
});

describe("a frame's buffers", () => {
  test("have room for as many parts as there are, each where dynamic offsets may point", () => {
    const device = new FakeDevice();
    const buffers = new FrameBuffer(device.asDevice, {} as GPUBindGroupLayout);
    for (let part = 1; part <= 100; part++) {
      expect(buffers.part(part, -part, 3, 9)).toBe(part * 256);
    }
    buffers.upload(640, 480);
    const { group, size } = buffers;
    const parts = device.recorder.named("writeBuffer").at(-1)?.args;
    const records = parts?.[2] as Int32Array;
    expect(parts?.[4]).toBe((101 * 256) / 4);
    expect([...records.slice((100 * 256) / 4, (100 * 256) / 4 + 4)]).toEqual([100, -100, 3, 9]);
    // The frame's own quads are where they say, in any row.
    expect([...records.slice(0, 4)]).toEqual([0, 0, 0, ANY_ROW]);
    buffers.clear();
    buffers.upload(640, 480);
    expect(buffers.group).toBe(group);
    expect(buffers.size).toBe(size);
    buffers.destroy();
    expect(device.live).toBe(0);
  });
});
