/**
 * Text written by the engine, the workers or the server, for display. They write
 * English, and it is stored and sent as they wrote it. So what they have written
 * is recognized here, by the messages that `zh-engine.ts` translates.
 */

import {
  type Catalogue,
  catalogueOf,
  fill,
  type LoadedCatalogue,
  type Lookup,
  textsOf,
} from "./index.ts";
import { type PackedMessages, type Patterns, patternsOf } from "./messages.ts";

/** A message with values in it. */
interface Template {
  readonly pattern: RegExp;
  readonly kinds: readonly string[];
  readonly translation: string;
  /** How much of it is words. */
  readonly words: number;
}

/** What a value may be, by its kind: a number, the name of a column, or where rows are inserted. */
const VALUES: Readonly<Record<string, string>> = {
  "#": "(\\d+)",
  "^": "([A-Z]+)",
  // Nothing but a space is between it and the name of a worksheet.
  "~": "(at row \\d+|after anchor row|at top)",
};
/** Any other value. What others have written, such as the body of an answer, may be empty. */
const ANYTHING = "([\\s\\S]*?)";
/** The kinds of values that may be messages themselves, or words of the engine's. */
const NESTED = "@~";

/** Which of several transactions a change is in. The server puts it after the label. */
const PART = / \(\d+\/\d+\)$/;

/** Longer texts are none of the messages. */
const MAX_LENGTH = 2_000;
const MAX_NESTING = 4;
const MAX_REMEMBERED = 500;

interface Translations {
  readonly catalogue: LoadedCatalogue;
  readonly plain: Lookup;
  /** The more of a message is words, the sooner its template is tried. */
  readonly templates: readonly Template[];
}

interface Functions {
  readonly catalogue: LoadedCatalogue;
  readonly lookUp: Lookup;
}

let messages: Translations | undefined;
let functions: Functions | undefined;
/** What has been translated lately. */
const remembered = new Map<string, string>();

const escaped = (text: string) => text.replace(/[\\^$.*+?()[\]{}|]/g, "\\$&");

function read(catalogue: LoadedCatalogue): Translations {
  const [patterns, translations]: Patterns = import.meta.env.PROD
    ? [(catalogue as PackedMessages)[3], (catalogue as PackedMessages)[4]]
    : patternsOf(catalogue as Catalogue);
  const lines = translations.split("\n");
  const templates = patterns.split("\n").map((pattern, line): Template => {
    // Words at even places, the kinds of values between them.
    const parts = pattern.split(/\{([#^~@]?)\}/);
    const source = parts.map((part, at) =>
      at % 2 === 0 ? escaped(part) : (VALUES[part] ?? ANYTHING),
    );
    const kinds = parts.filter((_, at) => at % 2 === 1);
    return {
      pattern: new RegExp(`^${source.join("")}$`),
      kinds,
      translation: lines[line] ?? "",
      words: parts.join("").length - kinds.join("").length,
    };
  });
  templates.sort((a, b) => b.words - a.words);
  return { catalogue, plain: textsOf(catalogue), templates };
}

function translate(from: Translations, text: string, nesting: number): string {
  const known = from.plain(text);
  if (known !== undefined || nesting > MAX_NESTING) {
    return known ?? text;
  }
  for (const { pattern, kinds, translation } of from.templates) {
    const [, ...values] = pattern.exec(text) ?? [];
    if (values.length > 0) {
      const params = kinds.map((kind, at) => {
        const value = values[at] ?? "";
        return kind && NESTED.includes(kind) ? translate(from, value, nesting + 1) : value;
      });
      return fill(translation, Object.fromEntries(params.entries()));
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
    messages = read(catalogue);
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

/** What is known about functions and their parameters, by their names. */
function functionTexts(): Lookup | undefined {
  const catalogue = catalogueOf("zh-functions");
  if (catalogue && functions?.catalogue !== catalogue) {
    functions = { catalogue, lookUp: textsOf(catalogue) };
  }
  return catalogue && functions?.lookUp;
}

/** What a function of formulas does. */
export function functionSummary(name: string, english: string): string {
  return functionTexts()?.(name) ?? english;
}

/** A list of parameters, or one of them, with their names in the language shown. */
export function functionParameters(english: string): string {
  const lookUp = functionTexts();
  return lookUp
    ? english.replace(/[a-z]\w*/g, (name) => {
        const [, word = name, number = ""] = /^(.*?)(\d*)$/.exec(name) ?? [];
        const known = lookUp(word);
        return lookUp(name) ?? (known === undefined ? name : known + number);
      })
    : english;
}
