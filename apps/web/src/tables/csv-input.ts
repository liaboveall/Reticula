import { TableError } from "./failure.ts";
import { fileFailure } from "./files.ts";

/** What text files are written in. */
export type TextEncoding = "utf-8" | "gb18030" | "utf-16le" | "utf-16be" | "windows-1252";

/** How much of a file is read at a time. */
export const CHUNK_BYTES = 4 << 20;

/**
 * A text file as UTF-8, piece by piece. While a piece is being used the next one
 * is read.
 */
export class TextInput {
  readonly #file: Blob;
  readonly #encoding: TextEncoding;
  readonly #chunkBytes: number;
  #at = 0;
  #next: Promise<ArrayBuffer> | null = null;
  #decoder: TextDecoder | null = null;

  constructor(file: Blob, encoding: TextEncoding, chunkBytes = CHUNK_BYTES) {
    this.#file = file;
    this.#encoding = encoding;
    this.#chunkBytes = chunkBytes;
    this.restart();
  }

  /** Goes back to the beginning. */
  restart(): void {
    this.#at = 0;
    this.#next = null;
    this.#decoder = this.#encoding === "utf-8" ? null : new TextDecoder(this.#encoding);
  }

  /** Bytes of the file that were handed out. */
  get position(): number {
    return this.#at;
  }

  /** The next piece, or null at the end. */
  async read(): Promise<Uint8Array | null> {
    if (this.#at >= this.#file.size) {
      return null;
    }
    const pending = this.#next ?? this.#slice(this.#at);
    const end = Math.min(this.#at + this.#chunkBytes, this.#file.size);
    this.#next = end < this.#file.size ? this.#slice(end) : null;
    let bytes: ArrayBuffer;
    try {
      bytes = await pending;
    } catch (error) {
      throw new TableError(fileFailure(error));
    }
    if (bytes.byteLength !== end - this.#at) {
      throw new TableError("changed");
    }
    this.#at = end;
    if (!this.#decoder) {
      return new Uint8Array(bytes);
    }
    const text = this.#decoder.decode(bytes, { stream: end < this.#file.size });
    return new TextEncoder().encode(text);
  }

  #slice(from: number): Promise<ArrayBuffer> {
    const pending = this.#file.slice(from, from + this.#chunkBytes).arrayBuffer();
    // Nobody may ever ask for it.
    pending.catch(() => {});
    return pending;
  }
}

/** The start of a text file as UTF-8: so many bytes, or all there is. */
export async function readHead(
  file: Blob,
  encoding: TextEncoding,
  bytes: number,
): Promise<Uint8Array> {
  const input = new TextInput(file, encoding, bytes);
  const head = new Uint8Array(bytes);
  let length = 0;
  while (length < bytes) {
    const piece = await input.read();
    if (!piece) {
      break;
    }
    const part = piece.subarray(0, bytes - length);
    head.set(part, length);
    length += part.length;
  }
  return head.subarray(0, length);
}

/** The encoding a file says it is in, if it does. */
export async function markedEncoding(file: Blob): Promise<TextEncoding | null> {
  const [a, b, c] = new Uint8Array(await file.slice(0, 3).arrayBuffer());
  if (a === 0xef && b === 0xbb && c === 0xbf) {
    return "utf-8";
  }
  if (a === 0xff && b === 0xfe) {
    return "utf-16le";
  }
  return a === 0xfe && b === 0xff ? "utf-16be" : null;
}
