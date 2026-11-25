import type { AppRoute } from "../routes.ts";

export type Locale = "en" | "zh-CN";

/** Where the browser keeps the language the user has chosen. */
export const LOCALE_KEY = "reticula:locale";

/** The files with Chinese texts, by the names of their modules. */
export type CatalogueName =
  | "zh-core"
  | "zh-pages"
  | "zh-landing"
  | "zh-labs"
  | "zh-engine"
  | "zh-functions"
  | "zh-tables";

/**
 * The texts each page waits for before it is rendered in Chinese. The build
 * preloads the same ones (`crates/xtask/src/web/bundle.rs` reads this table).
 */
export const CATALOGUES: Readonly<Record<AppRoute, readonly CatalogueName[]>> = {
  landing: ["zh-core", "zh-pages", "zh-landing"],
  app: ["zh-core"],
  workbooks: ["zh-core", "zh-pages"],
  account: ["zh-core", "zh-pages"],
  privacy: ["zh-core", "zh-pages"],
  sim: ["zh-core", "zh-pages", "zh-labs"],
  benchmarks: ["zh-core", "zh-pages", "zh-labs"],
};

/** The texts a page can do without at first. They are loaded after the others. */
export const LATER_CATALOGUES: Readonly<Partial<Record<AppRoute, readonly CatalogueName[]>>> = {
  app: ["zh-engine", "zh-functions", "zh-tables"],
  workbooks: ["zh-engine"],
  sim: ["zh-engine"],
};

/** The catalogues whose keys are no texts of the interface. They are read by name. */
export const KEYED_OTHERWISE: readonly CatalogueName[] = ["zh-engine", "zh-functions"];

/** The parts of a window the language depends on. */
export interface LocaleHost {
  readonly location: { readonly search: string };
  readonly localStorage: Pick<Storage, "getItem">;
  readonly navigator: { readonly languages: readonly string[] };
}

/**
 * The language to start in: the one the address asks for, else the one chosen
 * before, else the first of the browser's that there is.
 *
 * The inline script that preloads the texts contains this very function: the
 * build reads its source, without the types, and puts it there. So it uses
 * nothing from this module, and has no comments inside.
 */
export function detectLocale(host: LocaleHost, key: string): Locale {
  let chosen: string | null = null;
  try {
    chosen = host.localStorage.getItem(key);
  } catch {
    chosen = null;
  }
  const asked = new URLSearchParams(host.location.search).get("lang");
  for (const tag of [asked, chosen, ...host.navigator.languages]) {
    const language = /^(?:en|zh)(?![a-z])/i.exec(tag ?? "")?.[0].toLowerCase();
    if (language) {
      return language === "zh" ? "zh-CN" : "en";
    }
  }
  return "en";
}
