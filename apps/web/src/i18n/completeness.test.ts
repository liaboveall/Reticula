import { readdirSync, readFileSync } from "node:fs";
import { join, posix, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import type { AppRoute } from "../routes.ts";
import { ALLOWED, PENDING } from "./allowlist.ts";
import type { Catalogue } from "./index.ts";
import {
  CATALOGUES,
  type CatalogueName,
  KEYED_OTHERWISE,
  LATER_CATALOGUES,
} from "./locale-rule.ts";
import { placeholders, type ScannedModule, scanModule, slots } from "./scan.ts";
import zhCore from "./zh-core.ts";
import zhEngine from "./zh-engine.ts";
import zhFunctions from "./zh-functions.ts";
import zhLabs from "./zh-labs.ts";
import zhPages from "./zh-pages.ts";

// Every text of the interface is translated, every translation is used, and
// nothing is shown in English past them. The modules are read, not run.

const ROOT = fileURLToPath(new URL("../../../..", import.meta.url));
const SRC = "apps/web/src";

const TRANSLATIONS: Readonly<Record<CatalogueName, Catalogue>> = {
  "zh-core": zhCore,
  "zh-pages": zhPages,
  "zh-labs": zhLabs,
  "zh-engine": zhEngine,
  "zh-functions": zhFunctions,
};

const ROUTES = Object.keys(CATALOGUES) as AppRoute[];

/** What is generated, the texts themselves, and what this test is made of. */
const NOT_OF_THE_APP = /\/generated\/|\/i18n\/(?:zh-\w+|scan|scan-engine|allowlist)\.ts$/;

/** Pages to experiment on, which the app does not contain. */
const EXPERIMENTS = /^apps\/web\/src\/(?:bench|ime|visual)\//;
/** The engine writes English, and so does what carries its messages. They are translated when shown. */
const ENGINE = /^apps\/web\/src\/engine\//;
/** Draws what the cells contain, without any words of its own. */
const RENDERER = /^packages\/grid\/src\/render\//;

function sourcesIn(directory: string): string[] {
  return readdirSync(join(ROOT, directory), { recursive: true, encoding: "utf8" })
    .map((name) => posix.join(directory, name.split(sep).join("/")))
    .filter((name) => /\.tsx?$/.test(name) && !/\.test\.tsx?$|\.d\.ts$/.test(name))
    .filter((name) => !NOT_OF_THE_APP.test(name))
    .sort();
}

const read = (file: string) => readFileSync(join(ROOT, file), "utf8");

const modules = new Map<string, ScannedModule>(
  [...sourcesIn(SRC), ...sourcesIn("packages/grid/src")].map((file) => [
    file,
    scanModule(file, read(file)),
  ]),
);

/** The scanned module a specifier means, if it is one. */
function resolveFrom(file: string, specifier: string): string | null {
  if (specifier.startsWith("@reticula/grid")) {
    const entry = specifier === "@reticula/grid" ? "index" : specifier.split("/")[2];
    return `packages/grid/src/${entry}.ts`;
  }
  return specifier.startsWith(".") ? posix.join(posix.dirname(file), specifier) : null;
}

/** The modules loaded with some, now or later. What only the named ones load later is left out. */
function closure(roots: readonly string[], notLater: readonly string[] = []): Set<string> {
  const seen = new Set<string>();
  const open = [...roots];
  for (let file = open.pop(); file !== undefined; file = open.pop()) {
    const module = modules.get(file);
    if (!module || seen.has(file)) {
      continue;
    }
    seen.add(file);
    const later = notLater.includes(file) ? [] : module.dynamicImports;
    for (const specifier of [...module.staticImports, ...later]) {
      const found = resolveFrom(file, specifier);
      if (found !== null) {
        open.push(found);
      }
    }
  }
  return seen;
}

/** The module of each page, as the app finds it. */
const pages = new Map<string, string>(
  [...read(`${SRC}/routes-load.ts`).matchAll(/(\w+): \(\) => import\("\.\/([^"]+)"\)/g)].map(
    ([, route, file]) => [route ?? "", `${SRC}/${file}`],
  ),
);

const frame = closure([`${SRC}/main.tsx`], [`${SRC}/routes-load.ts`]);
const reachedFrom = new Map(
  ROUTES.map((route) => [route, new Set([...frame, ...closure([pages.get(route) ?? ""])])]),
);

function cataloguesOf(route: AppRoute): CatalogueName[] {
  return [...CATALOGUES[route], ...(LATER_CATALOGUES[route] ?? [])];
}

const cataloguesWith = (key: string) =>
  (Object.keys(TRANSLATIONS) as CatalogueName[]).filter(
    (name) => !KEYED_OTHERWISE.includes(name) && Object.hasOwn(TRANSLATIONS[name], key),
  );

const calls = [...modules].flatMap(([file, module]) =>
  module.calls.map((call) => ({ ...call, file, at: `${file}:${call.line}` })),
);

describe("texts", () => {
  it("can be read from the sources", () => {
    const misuses = [...modules].flatMap(([file, module]) =>
      module.misuses.map(({ line, text }) => `${file}:${line}: ${text}`),
    );
    expect(misuses).toEqual([]);
    expect(calls.length).toBeGreaterThan(100);
  });

  it("are on pages that are known", () => {
    expect([...pages.keys()].sort()).toEqual([...ROUTES].sort());
    for (const file of pages.values()) {
      expect(modules.has(file), file).toBe(true);
    }
    const unreached = calls
      .filter(({ file }) => ROUTES.every((route) => !reachedFrom.get(route)?.has(file)))
      .map(({ at }) => at);
    expect(unreached).toEqual([]);
  });

  it("have one translation each", () => {
    const missing = calls
      .filter(({ key }) => cataloguesWith(key).length !== 1)
      .map(({ at, key }) => `${at}: ${JSON.stringify(key)} is in [${cataloguesWith(key)}]`);
    expect(missing).toEqual([]);
  });

  it("have their translation where the page has loaded it", () => {
    const far = calls.flatMap(({ file, at, key }) =>
      ROUTES.filter((route) => reachedFrom.get(route)?.has(file))
        .filter((route) => !cataloguesWith(key).some((name) => cataloguesOf(route).includes(name)))
        .map((route) => `${at}: ${JSON.stringify(key)} on ${route}`),
    );
    expect(far).toEqual([]);
  });

  it("name what they are given", () => {
    const wrong = calls.flatMap(({ at, kind, key, singular, params, slots: given }) => {
      const wanted = placeholders(`${key} ${singular ?? ""}`);
      const implied = kind === "tn" ? ["n"] : [];
      const problems: string[] = [];
      if (params !== null) {
        const all = [...new Set([...params, ...implied])].sort();
        if (wanted.some((name) => !all.includes(name)) || params.some((n) => !wanted.includes(n))) {
          problems.push(`${at}: {${wanted}} is given {${all}}`);
        }
      }
      const wantedSlots = [...new Set(slots(key).map((tag) => tag.replace(/\D/g, "")))];
      if (given !== null && wantedSlots.join() !== [...given].sort().join()) {
        problems.push(`${at}: <${wantedSlots}> is given <${given}>`);
      }
      return problems;
    });
    expect(wrong).toEqual([]);
  });
});

describe("translations", () => {
  const entries = Object.entries(TRANSLATIONS).flatMap(([name, catalogue]) =>
    Object.entries(catalogue).map(([key, translation]) => ({ name, key, translation })),
  );

  it("are all used", () => {
    const used = new Set(calls.map(({ key }) => key));
    const unused = entries
      .filter(({ name }) => !KEYED_OTHERWISE.includes(name as CatalogueName))
      .filter(({ key }) => !used.has(key))
      .map(({ name, key }) => `${name}: ${key}`);
    expect(unused).toEqual([]);
  });

  it("have the placeholders and the slots of their texts", () => {
    const different = entries
      .filter(
        ({ key, translation }) =>
          placeholders(key).join() !== placeholders(translation).join() ||
          slots(key).join() !== slots(translation).join(),
      )
      .map(({ name, key, translation }) => `${name}: ${key} → ${translation}`);
    expect(different).toEqual([]);
  });

  it("are tidy", () => {
    const untidy = entries
      .filter(({ translation }) => translation === "" || translation !== translation.trim())
      .map(({ name, key }) => `${name}: ${key}`);
    expect(untidy).toEqual([]);
  });
});

describe("English on the screen", () => {
  const bare = [...modules]
    .filter(([file]) => [EXPERIMENTS, ENGINE, RENDERER].every((one) => !one.test(file)))
    .flatMap(([file, module]) => module.bareTexts.map((finding) => ({ file, ...finding })));

  it("comes out of t(), tn() or tx()", () => {
    const untranslated = bare
      .filter(({ file }) => !PENDING.includes(file))
      .filter(({ file, text }) => !ALLOWED.some((one) => one.file === file && one.text === text))
      .map(({ file, line, text }) => `${file}:${line}: ${text}`);
    expect(untranslated).toEqual([]);
  });

  it("has no exceptions that are not needed", () => {
    const stale = ALLOWED.filter(
      (one) => !bare.some(({ file, text }) => file === one.file && text === one.text),
    ).map(({ file, text }) => `${file}: ${text}`);
    expect(stale).toEqual([]);
    const done = PENDING.filter((file) => !bare.some((found) => found.file === file));
    expect(done).toEqual([]);
  });
});

describe("the language", () => {
  const users = [...modules]
    .filter(([, module]) =>
      [...module.staticImports, ...module.dynamicImports].some((one) => one.includes("i18n/")),
    )
    .map(([file]) => file);

  it("is no concern of the workers", () => {
    const workers = [...modules.keys()].flatMap((file) =>
      [...read(file).matchAll(/new URL\("(\.[^"]+\.ts)", import\.meta\.url\)/g)].map(
        ([, specifier]) => posix.join(posix.dirname(file), specifier ?? ""),
      ),
    );
    expect(workers).toContain(`${SRC}/engine/worker.ts`);
    const roots = [...workers, `${SRC}/sw.ts`];
    for (const root of roots) {
      expect(modules.has(root), root).toBe(true);
    }
    const inWorkers = closure(roots);
    expect(users.filter((file) => inWorkers.has(file))).toEqual([]);
  });

  it("is no concern of the grid, which is handed its texts", () => {
    expect(users.filter((file) => file.startsWith("packages/"))).toEqual([]);
    expect(users.every((file) => file.startsWith(`${SRC}/`))).toBe(true);
  });
});
