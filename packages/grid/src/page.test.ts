import { HEADER_LEN, TEXT_LEN_AT, VERSION_AT } from "@reticula/protocol/viewport";
import { fixture } from "@reticula/protocol/viewport-fixture";
import { describe, expect, test } from "vitest";
import { decodePage, isBool, isError, isFormula, isNumber, isTruncated } from "./page.ts";

/** The fixture's packet, as a buffer of its own. */
function packet(): ArrayBuffer {
  const bytes = new Uint8Array(fixture.hex.length / 2);
  for (let index = 0; index < bytes.length; index++) {
    bytes[index] = Number.parseInt(fixture.hex.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes.buffer;
}

describe("a page encoded by the engine", () => {
  const page = decodePage(packet());

  test("says where it belongs", () => {
    expect(page.sheet).toBe(fixture.sheet);
    expect(page.epoch).toBe(fixture.epoch);
    expect(page.firstRow).toBe(fixture.firstRow);
    expect(page.firstCol).toBe(fixture.firstCol);
    expect(page.rowCount).toBe(fixture.cells.length);
    expect(page.colCount).toBe(fixture.colIds.length);
    expect(page.byteLength).toBe(fixture.hex.length / 2);
  });

  test("carries the identifiers of its rows and columns", () => {
    expect([...page.rowIds]).toEqual(fixture.rowIds);
    expect([...page.colIds]).toEqual(fixture.colIds);
  });

  test("gives every cell the text and the v1 flags the engine gave it", () => {
    const cells = fixture.cells.map((row, rowIndex) =>
      row.map((_, colIndex) => {
        const cell = rowIndex * page.colCount + colIndex;
        const flags = page.flags[cell] ?? 0;
        return {
          text: page.texts[cell],
          number: isNumber(flags),
          bool: isBool(flags),
          error: isError(flags),
          formula: isFormula(flags),
          truncated: isTruncated(flags),
        };
      }),
    );
    expect(cells).toEqual(fixture.cells);
  });

  test("slices text by UTF-16 code units, not by bytes or characters", () => {
    // The first cell ends in a character that takes four bytes and two code
    // units; counting either way wrongly would shift every cell after it.
    expect(fixture.cells[0][0].text).toBe("品名 😀");
    expect(page.texts[0]).toBe("品名 😀");
    expect(page.texts[1]).toBe(fixture.cells[0][1].text);
  });
});

describe("a packet that is not a page", () => {
  test("is rejected when it is too short for a header", () => {
    expect(() => decodePage(new ArrayBuffer(HEADER_LEN - 1))).toThrow(/too short/);
  });

  test("is rejected when it has another layout version", () => {
    const buffer = packet();
    new DataView(buffer).setUint32(VERSION_AT, 99, true);
    expect(() => decodePage(buffer)).toThrow(/layout version 99/);
  });

  test("is rejected when its length does not match its header", () => {
    expect(() => decodePage(packet().slice(0, -1))).toThrow(/its header describes/);
    const longer = new Uint8Array(fixture.hex.length / 2 + 1);
    longer.set(new Uint8Array(packet()));
    expect(() => decodePage(longer.buffer)).toThrow(/its header describes/);
  });

  test("is rejected when its cells do not account for all of its text", () => {
    // One more byte of text than the cell records cover.
    const longer = new Uint8Array(fixture.hex.length / 2 + 1);
    longer.set(new Uint8Array(packet()));
    longer[longer.length - 1] = 0x21;
    const view = new DataView(longer.buffer);
    view.setUint32(TEXT_LEN_AT, view.getUint32(TEXT_LEN_AT, true) + 1, true);
    expect(() => decodePage(longer.buffer)).toThrow(/code units of text/);
  });
});
