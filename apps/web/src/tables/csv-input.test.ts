import { describe, expect, it } from "vitest";
import { markedEncoding, readHead, type TextEncoding, TextInput } from "./csv-input.ts";

async function readAll(input: TextInput): Promise<{ text: string; pieces: number }> {
  const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
  let text = "";
  let pieces = 0;
  for (let piece = await input.read(); piece; piece = await input.read()) {
    text += decoder.decode(piece, { stream: true });
    pieces++;
  }
  return { text: text + decoder.decode(), pieces };
}

const BYTE_ORDER_MARK = String.fromCodePoint(0xfeff);

const utf16 = (text: string, little: boolean) => {
  const view = new DataView(new ArrayBuffer(text.length * 2));
  for (let at = 0; at < text.length; at++) {
    view.setUint16(at * 2, text.charCodeAt(at), little);
  }
  return new Uint8Array(view.buffer);
};

describe("a text file", () => {
  it("is handed out in pieces, again and again", async () => {
    const bytes = Uint8Array.from({ length: 1_000 }, (_, at) => 0x30 + (at % 10));
    const input = new TextInput(new Blob([bytes]), "utf-8", 64);
    const first = await readAll(input);
    expect(first.pieces).toBe(16);
    expect(first.text).toBe("0123456789".repeat(100));
    expect(input.position).toBe(1_000);
    input.restart();
    expect(input.position).toBe(0);
    expect((await readAll(input)).text).toBe(first.text);
    input.restart();
    await input.read();
    input.restart();
    expect((await readAll(input)).text).toBe(first.text);
  });

  it("that is empty has no pieces", async () => {
    expect(await new TextInput(new Blob([]), "utf-8").read()).toBeNull();
  });

  it("is made UTF-8 wherever it is cut", async () => {
    // The last four bytes are one character.
    const gb = Uint8Array.of(
      ...[0xc3, 0xfb, 0x2c, 0xbc, 0xdb, 0xb8, 0xf1, 0x0a, 0xb2, 0xe8, 0x2c, 0x33],
      ...[0x94, 0x39, 0xda, 0x33],
    );
    const text = "a,𝄞\r\né";
    for (const size of [1, 2, 3, 5, 7, 100]) {
      const read = async (bytes: Uint8Array<ArrayBuffer>, encoding: TextEncoding) =>
        (await readAll(new TextInput(new Blob([bytes]), encoding, size))).text;
      expect(await read(gb, "gb18030")).toBe(`名,价格\n茶,3${String.fromCodePoint(0x1f4a9)}`);
      expect(await read(utf16(BYTE_ORDER_MARK + text, true), "utf-16le")).toBe(text);
      expect(await read(utf16(text, false), "utf-16be")).toBe(text);
    }
    const latin = new Blob([Uint8Array.of(0x80, 0xe9)]);
    expect((await readAll(new TextInput(latin, "windows-1252"))).text).toBe("€é");
  });

  it("that is no longer the one that was picked is told from others", async () => {
    const gone = {
      size: 10,
      slice: () => ({
        arrayBuffer: () => Promise.reject(new DOMException("gone", "NotReadableError")),
      }),
    } as unknown as Blob;
    await expect(new TextInput(gone, "utf-8").read()).rejects.toMatchObject({ code: "changed" });
    const shorter = {
      size: 10,
      slice: () => ({ arrayBuffer: async () => new ArrayBuffer(4) }),
    } as unknown as Blob;
    await expect(new TextInput(shorter, "utf-8").read()).rejects.toMatchObject({ code: "changed" });
  });
});

describe("the start of a text file", () => {
  it("is as long as it was asked for, or as the file is", async () => {
    const text = "价格".repeat(100);
    const file = new Blob([utf16(text, true)]);
    const whole = new TextEncoder().encode(text);
    expect(await readHead(file, "utf-16le", 100)).toEqual(whole.subarray(0, 100));
    expect(await readHead(file, "utf-16le", 10_000)).toEqual(whole);
    expect(await readHead(new Blob(["ab"]), "utf-8", 2)).toEqual(Uint8Array.of(0x61, 0x62));
    expect(await readHead(new Blob([]), "utf-8", 2)).toEqual(new Uint8Array());
  });

  it("may say what the file is written in", async () => {
    const marked = (...bytes: number[]) => markedEncoding(new Blob([Uint8Array.of(...bytes)]));
    expect(await marked(0xef, 0xbb, 0xbf, 0x61)).toBe("utf-8");
    expect(await marked(0xff, 0xfe)).toBe("utf-16le");
    expect(await marked(0xfe, 0xff)).toBe("utf-16be");
    expect(await marked(0x61)).toBeNull();
    expect(await marked()).toBeNull();
  });
});
