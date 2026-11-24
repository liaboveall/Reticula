import { basename } from "node:path";
import { pathToFileURL } from "node:url";
import type { Plugin } from "vite";
import type { Catalogue } from "../src/i18n/index.ts";
import { hasValues, lines, type PackedMessages, patternsOf } from "../src/i18n/messages.ts";
import { keyOf, type Packed } from "../src/i18n/packed.ts";

/** What the texts of the interface are told apart by. */
const TEXT_KEY = 3;
/** Anything may turn up where a message is expected, so their keys tell more apart. */
const MESSAGE_KEY = 5;

const MESSAGES = "zh-engine";
const NAMES = "zh-functions";
const CATALOGUE = /[\\/]src[\\/]i18n[\\/]zh-\w+\.ts$/;

function packTexts(catalogue: Catalogue, length: number): Required<Packed> {
  const keys = Object.keys(catalogue).map((source) => keyOf(source, length));
  return [keys.join(""), lines(Object.values(catalogue)), length];
}

/** A catalogue as `engine-text.ts` and `index.ts` read it in the build, by its name. */
export function pack(name: string, catalogue: Catalogue): Packed | PackedMessages {
  switch (name) {
    case NAMES:
      return [Object.keys(catalogue).join(" "), lines(Object.values(catalogue))];
    case MESSAGES: {
      const plain = Object.entries(catalogue).filter(([message]) => !hasValues(message));
      return [...packTexts(Object.fromEntries(plain), MESSAGE_KEY), ...patternsOf(catalogue)];
    }
    default:
      return packTexts(catalogue, TEXT_KEY);
  }
}

/** Two texts with the same key, if there are any. */
function sameKey(
  catalogues: ReadonlyMap<string, Catalogue>,
  seed?: number,
): readonly [string, string] | null {
  const taken = new Map<string, string>();
  for (const [name, catalogue] of catalogues) {
    if (name === NAMES) {
      continue;
    }
    const length = name === MESSAGES ? MESSAGE_KEY : TEXT_KEY;
    for (const source of Object.keys(catalogue)) {
      const key = keyOf(source, length, seed);
      const other = taken.get(key);
      if (other !== undefined && other !== source) {
        return [other, source];
      }
      taken.set(key, source);
    }
  }
  return null;
}

/**
 * Fails when keys do not tell the English texts apart. Any text of the
 * interface may be looked up in any catalogue of such texts, also when it is
 * another catalogue that has it.
 */
export function checkKeys(catalogues: ReadonlyMap<string, Catalogue>): void {
  const texts = sameKey(catalogues);
  if (texts) {
    let seed = 1;
    while (sameKey(catalogues, seed)) {
      seed++;
    }
    const [one, other] = texts.map((text) => JSON.stringify(text));
    throw new Error(
      `${one} and ${other} have the same key: make ${seed} the SEED in src/i18n/packed.ts`,
    );
  }
}

/** The English texts have a place in the code already, so the catalogues go without them. */
export function packedCatalogues(): Plugin {
  const catalogues = new Map<string, Catalogue>();
  let isProduction = false;
  return {
    name: "reticula:catalogues",
    apply: "build",
    configResolved(config) {
      // The app asks the same question when it reads them.
      isProduction = config.isProduction;
    },
    buildStart() {
      catalogues.clear();
    },
    load: {
      filter: { id: CATALOGUE },
      async handler(id) {
        if (!isProduction) {
          return null;
        }
        const module: { readonly default: Catalogue } = await import(pathToFileURL(id).href);
        const name = basename(id, ".ts");
        catalogues.set(name, module.default);
        return `export default ${JSON.stringify(pack(name, module.default))};`;
      },
    },
    buildEnd(error) {
      if (!error) {
        checkKeys(catalogues);
      }
    },
  };
}
