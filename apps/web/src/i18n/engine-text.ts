/**
 * Text written by the engine, the workers or the server, for display. They write
 * English, and it is stored and sent as they wrote it. So what they have written
 * is recognized here, by the messages that `zh-engine.ts` translates.
 */

import { type Catalogue, catalogueOf, fill } from "./index.ts";

/** A message with values in it. */
interface Template {
  readonly pattern: RegExp;
  readonly names: readonly string[];
  readonly translation: string;
}

/** The placeholders that stand for a number. The others may stand for anything. */
export const NUMBER =
  /^(?:MAX_\w+|n|p|pos|got|found|expected|offset|cursor|bytes|replica|ids|cluster|format_items|sheets|sheet_id|charts|drawings|pivots|comments|external_links|txns|retries|\w*(?:count|seq|epoch|rows?|cols?|idx|args|runs|tokens|steps|issues|conflicts|Seq|Epoch|Count))$/;

/** The placeholders that may stand for another message, or for a word of the engine's. */
export const MESSAGE =
  /^(?:0|axis_err|corr|e|err|error|field|label|lex|msg|pos_hint|reason|status)$/;

/** Where rows are inserted. Nothing but a space is between it and the name of a worksheet. */
const PLACE = "at row \\d+|after anchor row|at top";

/** Which of several transactions a change is in. The server puts it after the label. */
const PART = / \(\d+\/\d+\)$/;

/** Longer texts are none of the messages. */
const MAX_LENGTH = 2_000;
const MAX_NESTING = 4;
const MAX_REMEMBERED = 500;

interface Messages {
  readonly catalogue: Catalogue;
  /** The more of a message is words, the sooner its template is tried. */
  readonly templates: readonly Template[];
}

let messages: Messages | undefined;
/** What has been translated lately. */
const remembered = new Map<string, string>();

function patternOf(name: string): string {
  if (name === "pos_hint") {
    return `(${PLACE})`;
  }
  if (name === "letters") {
    return "([A-Z]+)";
  }
  // What others have written, such as the body of an answer, may be empty.
  return NUMBER.test(name) ? "(\\d+)" : "([\\s\\S]*?)";
}

const escaped = (text: string) => text.replace(/[\\^$.*+?()[\]{}|]/g, "\\$&");

function compile(catalogue: Catalogue): Messages {
  const found: (Template & { readonly words: number })[] = [];
  for (const [message, translation] of Object.entries(catalogue)) {
    // Words at even places, the names of placeholders between them.
    const parts = message.split(/\{(\w+)\}/);
    if (parts.length > 1) {
      const pattern = parts.map((part, at) => (at % 2 === 0 ? escaped(part) : patternOf(part)));
      found.push({
        pattern: new RegExp(`^${pattern.join("")}$`),
        names: parts.filter((_, at) => at % 2 === 1),
        translation,
        words: parts.reduce((length, part, at) => length + (at % 2 === 0 ? part.length : 0), 0),
      });
    }
  }
  return { catalogue, templates: found.sort((a, b) => b.words - a.words) };
}

function translate(from: Messages, text: string, nesting: number): string {
  const known = lookUp(from.catalogue, text);
  if (known !== undefined || nesting > MAX_NESTING) {
    return known ?? text;
  }
  for (const { pattern, names, translation } of from.templates) {
    const [, ...values] = pattern.exec(text) ?? [];
    if (values.length > 0) {
      const params: Record<string, string> = {};
      for (const [at, name] of names.entries()) {
        const value = values[at] ?? "";
        params[name] = MESSAGE.test(name) ? translate(from, value, nesting + 1) : value;
      }
      return fill(translation, params);
    }
  }
  const [part] = PART.exec(text) ?? [];
  return part ? translate(from, text.slice(0, -part.length), nesting + 1) + part : text;
}

/** A message in the language shown, or the text as it is if it is none. */
export function translateEngineText(text: string): string {
  const catalogue = catalogueOf("zh-engine");
  if (!catalogue || text.length > MAX_LENGTH) {
    return text;
  }
  if (messages?.catalogue !== catalogue) {
    messages = compile(catalogue);
    remembered.clear();
  }
  let translation = remembered.get(text);
  if (translation === undefined) {
    translation = translate(messages, text, 0);
    if (remembered.size >= MAX_REMEMBERED) {
      remembered.clear();
    }
    remembered.set(text, translation);
  }
  return translation;
}

/** What a function of formulas does. */
export function functionSummary(name: string, english: string): string {
  return lookUp(catalogueOf("zh-functions"), name) ?? english;
}

/** A list of parameters, or one of them, with their names in the language shown. */
export function functionParameters(english: string): string {
  const catalogue = catalogueOf("zh-functions");
  return catalogue
    ? english.replace(/[a-z]\w*/g, (name) => {
        const [, word = name, number = ""] = /^(.*?)(\d*)$/.exec(name) ?? [];
        const known = lookUp(catalogue, word);
        return lookUp(catalogue, name) ?? (known === undefined ? name : known + number);
      })
    : english;
}

function lookUp(catalogue: Catalogue | undefined, key: string): string | undefined {
  return catalogue && Object.hasOwn(catalogue, key) ? catalogue[key] : undefined;
}
