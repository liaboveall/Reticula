/**
 * The form in which the build has the catalogues (`build/catalogues.ts`). The
 * code that shows a text has the English one already, so instead of it a
 * catalogue has a short key made from it.
 */

import type { Lookup } from "./index.ts";

/**
 * Keys, all of the same length, and Chinese texts, a line for each. Without a
 * length the keys are names, with spaces between them.
 */
export type Packed = readonly [keys: string, texts: string, length?: number];

/** To be another one when two texts have the same key. The build finds that out, and which one to take. */
const SEED = 40;
/** The digits of the keys are characters in a row, none of which a literal has to escape. */
const FIRST_DIGIT = 35;
const DIGITS = 57;

/** The key of an English text. It has up to five digits. */
export function keyOf(text: string, length: number, seed = SEED): string {
  let hash = seed;
  for (let at = 0; at < text.length; at++) {
    hash = Math.imul(hash ^ text.charCodeAt(at), 0x01000193);
  }
  hash >>>= 0;
  let key = "";
  while (key.length < length) {
    key += String.fromCharCode(FIRST_DIGIT + (hash % DIGITS));
    hash = Math.floor(hash / DIGITS);
  }
  return key;
}

export function unpack([keys, texts, length]: Packed): Lookup {
  const names = keys.split(" ");
  const found = new Map(
    texts
      .split("\n")
      .map((text, at) => [length ? keys.substr(at * length, length) : names[at], text]),
  );
  return (source) => found.get(length ? keyOf(source, length) : source);
}
