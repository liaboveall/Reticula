import { readdirSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "vite";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { NAMES, namedReact } from "./react.ts";

const SRC = fileURLToPath(new URL("../src", import.meta.url));
const ENTRY = `${SRC.replaceAll("\\", "/")}/made-up.js`;
const require = createRequire(import.meta.url);

/** Builds a module that is made up, next to the ones of the app. */
async function built(code: string): Promise<string> {
  const output = await build({
    configFile: false,
    root: fileURLToPath(new URL("..", import.meta.url)),
    logLevel: "silent",
    plugins: [
      namedReact(),
      {
        name: "made-up",
        enforce: "pre",
        resolveId: (id) => (id === ENTRY ? id : null),
        load: (id) => (id === ENTRY ? code : null),
      },
    ],
    build: {
      write: false,
      minify: false,
      rolldownOptions: { input: ENTRY, preserveEntrySignatures: "strict" },
    },
  });
  const [first] = [output].flat();
  return first && "output" in first ? first.output[0].code : "";
}

beforeEach(() => {
  vi.stubEnv("NODE_ENV", "production");
});

afterEach(() => {
  vi.unstubAllEnvs();
});

describe("React by name", () => {
  it("has the names the app imports", () => {
    const sources = readdirSync(SRC, { recursive: true, encoding: "utf8" }).filter(
      (file) => /\.tsx?$/.test(file) && !/\.test\.|harness/.test(file),
    );
    const imported = sources.flatMap((file) => {
      const source = readFileSync(`${SRC}/${file}`, "utf8");
      const [, names = ""] = /^import \{([^}]*)\} from "react";/m.exec(source) ?? [];
      return names
        .split(",")
        .map((name) => name.trim())
        .filter((name) => name !== "" && !name.startsWith("type "));
    });
    expect(imported.length).toBeGreaterThan(20);
    expect(imported.filter((name) => !NAMES.react?.includes(name))).toEqual([]);
  });

  it("has nothing React has not", () => {
    for (const [source, names] of Object.entries(NAMES)) {
      const module: object = require(source);
      expect(
        names.filter((name) => !(name in module)),
        source,
      ).toEqual([]);
    }
  });

  it("is called without an object in between", async () => {
    const code = await built('import { useId } from "react";\nexport const x = () => useId();');
    expect(code).toMatch(/(?:const|let|var) x = \(\) => useId\(\)/);
  });

  it("is an error where the list is incomplete", async () => {
    const building = built('import { cache } from "react";\nexport const x = cache;');
    await expect(building).rejects.toThrow("cache");
  });

  it("does not announce itself", async () => {
    const code = await built('export { createRoot } from "react-dom/client";');
    expect(code).toContain("createRoot");
    expect(code).not.toContain("__REACT_DEVTOOLS_GLOBAL_HOOK__");
  });
});
