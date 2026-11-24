/**
 * The two languages of the interface.
 *
 * The English text is the key: `t("Insert row above")`. English needs nothing
 * loaded; Chinese texts come in catalogues, which are chunks of their own.
 * Where one English text has two meanings, a suffix tells them apart:
 * `t("Open|verb")` reads "Open". `completeness.test.ts` keeps the catalogues
 * complete, which is why the texts have to be literals.
 *
 * What needs React is in `react.ts`: with it in here, the bundler makes a chunk
 * of its own out of React, one more for every page to start with.
 */

import type { AppRoute } from "../routes.ts";
import {
  CATALOGUES,
  type CatalogueName,
  detectLocale,
  KEYED_OTHERWISE,
  LATER_CATALOGUES,
  LOCALE_KEY,
  type Locale,
} from "./locale-rule.ts";

export type { Locale } from "./locale-rule.ts";

/** Chinese texts by the English ones. */
export type Catalogue = Readonly<Record<string, string>>;

/** What goes into the `{placeholders}` of a text. */
export type Params = Readonly<Record<string, string | number>>;

const LOADERS: Readonly<Record<CatalogueName, () => Promise<{ readonly default: Catalogue }>>> = {
  "zh-core": () => import("./zh-core.ts"),
  "zh-pages": () => import("./zh-pages.ts"),
  "zh-labs": () => import("./zh-labs.ts"),
  "zh-engine": () => import("./zh-engine.ts"),
  "zh-functions": () => import("./zh-functions.ts"),
};

/** The language of numbers and dates. */
const INTL_TAG: Readonly<Record<Locale, string>> = { en: "en-US", "zh-CN": "zh-CN" };

const MILLISECONDS: Readonly<Record<Locale, string>> = { en: "ms", "zh-CN": "毫秒" };

/** How long the first page, or the next one, waits for texts before it goes without. */
const PATIENCE_MS = 1_500;
const FIRST_RETRY_MS = 2_000;
const LAST_RETRY_MS = 60_000;

let route: AppRoute = "landing";
/** The language asked for; `shown` follows once its texts are there. */
let wanted: Locale = "en";
let shown: Locale = "en";
let retryTimer: ReturnType<typeof setTimeout> | undefined;

const texts = new Map<string, string>();
const catalogues = new Map<CatalogueName, Catalogue>();
const loads = new Map<CatalogueName, Promise<void>>();
const listeners = new Set<() => void>();
let revision = 0;
let hasNewTexts = false;

function load(name: CatalogueName): Promise<void> {
  let loading = loads.get(name);
  if (!loading) {
    loading = LOADERS[name]().then((module) => {
      catalogues.set(name, module.default);
      for (const [source, translation] of Object.entries(module.default)) {
        if (!KEYED_OTHERWISE.includes(name)) {
          texts.set(source, translation);
        }
        hasNewTexts = true;
      }
    });
    loads.set(name, loading);
    loading.catch(() => loads.delete(name));
  }
  return loading;
}

const loadAll = (names: readonly CatalogueName[] = []) => Promise.all(names.map(load));

function show(): void {
  if (shown === wanted && !hasNewTexts) {
    return;
  }
  shown = wanted;
  hasNewTexts = false;
  revision++;
  document.documentElement.lang = shown;
  for (const listener of listeners) {
    listener();
  }
}

/**
 * Loads what the page needs and shows it, trying again for as long as that
 * fails. Resolves after the first attempt.
 */
function bringTexts(retryMs: number): Promise<void> {
  clearTimeout(retryTimer);
  const again = () => {
    clearTimeout(retryTimer);
    if (wanted !== "en") {
      retryTimer = setTimeout(bringTexts, retryMs, Math.min(retryMs * 2, LAST_RETRY_MS));
    }
  };
  return loadAll(CATALOGUES[route]).then(() => {
    show();
    loadAll(LATER_CATALOGUES[route]).then(show, again);
  }, again);
}

/** Resolves when the language is shown, or when the page should not wait for it any longer. */
function settle(): Promise<void> {
  if (wanted === "en") {
    clearTimeout(retryTimer);
    show();
    return Promise.resolve();
  }
  const patience = new Promise<void>((later) => setTimeout(later, PATIENCE_MS));
  return Promise.race([bringTexts(FIRST_RETRY_MS), patience]);
}

/** Decides on a language. Without its texts in time the page starts in English. */
export function initLocale(first: AppRoute): Promise<void> {
  wanted = detectLocale(window, LOCALE_KEY);
  return enterRoute(first);
}

/** Gets the texts of the page that is about to be shown. Never rejects. */
export function enterRoute(next: AppRoute): Promise<void> {
  route = next;
  return settle();
}

export function setLocale(locale: Locale): Promise<void> {
  wanted = locale;
  try {
    window.localStorage.setItem(LOCALE_KEY, locale);
  } catch {
    // It holds for this page then.
  }
  const url = new URL(window.location.href);
  if (url.searchParams.has("lang")) {
    // The address would win over the choice when the page is loaded again.
    url.searchParams.set("lang", locale === "en" ? "en" : "zh");
    window.history.replaceState(window.history.state, "", url);
  }
  return settle();
}

export function getLocale(): Locale {
  return shown;
}

/** Has a listener called whenever texts read differently than before. Returns how to stop. */
export function subscribeLocale(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** A number that is another one after every such change. */
export function localeRevision(): number {
  return revision;
}

/** A catalogue by itself, if its language is shown and it has been loaded. */
export function catalogueOf(name: CatalogueName): Catalogue | undefined {
  return shown === "en" ? undefined : catalogues.get(name);
}

/** A text in the language shown, without its values. */
export function textOf(source: string): string {
  const translation = shown === "en" ? undefined : texts.get(source);
  return translation ?? (source.includes("|") ? source.replace(/\|[a-z]+$/, "") : source);
}

export function fill(text: string, params: Params | undefined): string {
  return params
    ? text.replace(/\{(\w+)\}/g, (slot, name: string) => String(params[name] ?? slot))
    : text;
}

/** A text in the language shown. */
export function t(source: string, params?: Params): string {
  return fill(textOf(source), params);
}

/** A text about `n` of something. Chinese has one form, whose key is `other`. */
export function tn(n: number, one: string, other: string, params?: Params): string {
  const source = shown === "en" && n === 1 ? one : other;
  return t(source, { n: fmtInt(n), ...params });
}

const numberFormats = new Map<string, Intl.NumberFormat>();

function numberFormat(options: Intl.NumberFormatOptions): Intl.NumberFormat {
  const key = shown + JSON.stringify(options);
  let format = numberFormats.get(key);
  if (!format) {
    format = new Intl.NumberFormat(INTL_TAG[shown], options);
    numberFormats.set(key, format);
  }
  return format;
}

const digits = (count: number) => ({ minimumFractionDigits: count, maximumFractionDigits: count });

/** A number with a fixed number of decimals. */
export function fmtNumber(value: number, decimals: number): string {
  return numberFormat(digits(decimals)).format(value);
}

export function fmtInt(value: number): string {
  return fmtNumber(value, 0);
}

/** A duration in milliseconds, with its unit. */
export function fmtMs(ms: number): string {
  return `${fmtNumber(ms, 1)} ${MILLISECONDS[shown]}`;
}

/** A size in bytes, with its unit, which is the same in both languages. */
export function fmtBytes(bytes: number): string {
  const steps = ["B", "KB", "MB", "GB"];
  let value = Number.isFinite(bytes) ? Math.max(0, bytes) : 0;
  let step = 0;
  while (value >= 1024 && step < steps.length - 1) {
    value /= 1024;
    step++;
  }
  const format = numberFormat({ useGrouping: false, ...digits(Math.min(step, 2)) });
  return `${format.format(value)} ${steps[step]}`;
}

const DATE_AND_TIME: Intl.DateTimeFormatOptions = {
  year: "numeric",
  month: "numeric",
  day: "numeric",
  hour: "numeric",
  minute: "numeric",
  second: "numeric",
};

export function fmtDateTime(
  date: Date | number,
  options: Intl.DateTimeFormatOptions = DATE_AND_TIME,
): string {
  return new Intl.DateTimeFormat(INTL_TAG[shown], options).format(date);
}
