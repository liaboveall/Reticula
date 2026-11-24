// The texts that B13 fills its sheets with.

import { FLAG_NUMBER } from "@reticula/protocol/viewport";
import { Random } from "./stats.ts";

/**
 * `distinct`: every cell on screen has a text of its own. `repeating`: texts
 * come from short lists, as the values of real columns mostly do.
 */
export type Content = "repeating" | "distinct";

/** Texts in a pool; as many as more than ten screens of the larger size show. */
export const POOL = 2 ** 18;

export interface Pool {
  readonly texts: readonly string[];
  readonly flags: Uint16Array;
}

/** Three thousand different characters: a sheet of real Chinese text has thousands. */
const HAN = Array.from({ length: 3000 }, (_, index) => String.fromCharCode(0x4e00 + index * 5));
const LETTERS = [..."ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-"];

export function number(random: Random): string {
  const whole = random.below(10 ** (1 + random.below(7)));
  const sign = random.below(8) === 0 ? "-" : "";
  return `${sign}${whole}.${String(random.below(100)).padStart(2, "0")}`;
}

function pick(random: Random, alphabet: readonly string[], least: number, most: number): string {
  let text = "";
  for (let length = least + random.below(most - least + 1); length > 0; length--) {
    text += alphabet[random.below(alphabet.length)];
  }
  return text;
}

export const han = (random: Random) => pick(random, HAN, 2, 7);
export const ascii = (random: Random) => pick(random, LETTERS, 4, 12);

/** Half numbers, three in ten Chinese, two in ten ASCII. */
export function makePool(content: Content, length = POOL): Pool {
  const random = new Random(content === "distinct" ? 1 : 2);
  const source = (make: (random: Random) => string, count: number) => {
    if (content === "distinct") {
      return () => make(random);
    }
    const list = Array.from({ length: count }, () => make(random));
    return () => list[random.below(count)] ?? "";
  };
  const numbers = source(number, 200);
  const hans = source(han, 60);
  const asciis = source(ascii, 200);
  const flags = new Uint16Array(length);
  const texts = Array.from({ length }, (_, index) => {
    const kind = random.below(10);
    if (kind < 5) {
      flags[index] = FLAG_NUMBER;
      return numbers();
    }
    return kind < 8 ? hans() : asciis();
  });
  return { texts, flags };
}

const pools = new Map<Content, Pool>();

/** The pool of a kind of content, made when it is first asked for. */
export function poolOf(content: Content): Pool {
  let pool = pools.get(content);
  if (!pool) {
    pool = makePool(content);
    pools.set(content, pool);
  }
  return pool;
}

/** The same mix as a text of comma-separated values, for the app to import. */
export function csvOf(rows: number, cols: number): string {
  const random = new Random(3);
  let text = "";
  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < cols; col++) {
      const kind = random.below(10);
      const make = kind < 5 ? number : kind < 8 ? han : ascii;
      // No text has a comma, a quote or a line break in it.
      text += `${col > 0 ? "," : ""}${make(random)}`;
    }
    text += "\n";
  }
  return text;
}
