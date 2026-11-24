/**
 * Prepares the messages of `zh-engine.ts` for `engine-text.ts`. The build does
 * this; where there is none, it is done when they are loaded.
 */

import type { Catalogue } from "./index.ts";
import type { Packed } from "./packed.ts";

/** The placeholders that stand for a number. The others may stand for anything. */
export const NUMBER =
  /^(?:MAX_\w+|n|p|pos|got|found|expected|offset|cursor|bytes|replica|ids|cluster|format_items|sheets|sheet_id|charts|drawings|pivots|comments|external_links|txns|retries|\w*(?:count|seq|epoch|rows?|cols?|idx|args|runs|tokens|steps|issues|conflicts|Seq|Epoch|Count))$/;

/** The placeholders that may stand for another message, or for a word of the engine's. */
export const MESSAGE =
  /^(?:0|axis_err|corr|e|err|error|field|label|lex|msg|pos_hint|reason|status)$/;

/** What messages with values in them look like, and what they read as. A line for each. */
export type Patterns = readonly [patterns: string, translations: string];

/** The other messages as they are packed, and the patterns. */
export type PackedMessages = readonly [...Required<Packed>, ...Patterns];

/** What a pattern has in place of a placeholder's name. */
function kindOf(name: string): string {
  if (name === "pos_hint") {
    return "~";
  }
  if (name === "letters") {
    return "^";
  }
  return NUMBER.test(name) ? "#" : MESSAGE.test(name) ? "@" : "";
}

/** The texts, a line for each. */
export function lines(texts: readonly string[]): string {
  const broken = texts.find((text) => text.includes("\n"));
  if (broken !== undefined) {
    throw new Error(`a text of several lines cannot be in a catalogue: ${JSON.stringify(broken)}`);
  }
  return texts.join("\n");
}

/** Words at even places, the names of placeholders between them. */
const partsOf = (message: string) => message.split(/\{(\w+)\}/);

export function hasValues(message: string): boolean {
  return partsOf(message).length > 1;
}

export function patternsOf(catalogue: Catalogue): Patterns {
  const messages = Object.entries(catalogue).filter(([message]) => hasValues(message));
  const patterns = messages.map(([message]) =>
    partsOf(message).map((part, at) => (at % 2 === 0 ? part : `{${kindOf(part)}}`)),
  );
  const translations = messages.map(([message, translation]) => {
    const names = partsOf(message).filter((_, at) => at % 2 === 1);
    return translation.replace(/\{(\w+)\}/g, (_, name: string) => `{${names.lastIndexOf(name)}}`);
  });
  return [lines(patterns.map((parts) => parts.join(""))), lines(translations)];
}
