import { fileURLToPath } from "node:url";
import { build } from "vite";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Catalogue } from "../src/i18n/index.ts";
import { keyOf, type Packed, unpack } from "../src/i18n/packed.ts";
import zhCore from "../src/i18n/zh-core.ts";
import { checkKeys, pack, packedCatalogues } from "./catalogues.ts";

const ZH_CORE = fileURLToPath(new URL("../src/i18n/zh-core.ts", import.meta.url));

/** The module with the catalogue, as it is in a build. */
async function built(): Promise<string> {
  const output = await build({
    configFile: false,
    logLevel: "silent",
    plugins: [packedCatalogues()],
    build: {
      write: false,
      minify: false,
      rolldownOptions: { input: ZH_CORE, preserveEntrySignatures: "strict" },
    },
  });
  const [first] = [output].flat();
  return first && "output" in first ? first.output[0].code : "";
}

afterEach(() => {
  vi.unstubAllEnvs();
});

/** Two texts with the same key. */
function sameKey(length: number): readonly [string, string] {
  const seen = new Map<string, string>();
  for (let n = 0; ; n++) {
    const text = `Text ${n}`;
    const key = keyOf(text, length);
    const other = seen.get(key);
    if (other !== undefined) {
      return [other, text];
    }
    seen.set(key, text);
  }
}

describe("packed catalogues", () => {
  it("are what the build has in place of the ones that are written", async () => {
    vi.stubEnv("NODE_ENV", "production");
    const code = await built();
    expect(code).not.toContain("Skip to the grid");
    const module: { readonly default: Packed } = await import(
      /* @vite-ignore */ `data:text/javascript,${encodeURIComponent(code)}`
    );
    const lookUp = unpack(module.default);
    for (const [source, translation] of Object.entries(zhCore)) {
      expect(lookUp(source), source).toBe(translation);
    }
  });

  it("are only in a build in which the app expects them", async () => {
    vi.stubEnv("NODE_ENV", "development");
    expect(await built()).toContain("Skip to the grid");
  });

  it("know nothing of other texts", () => {
    const lookUp = unpack(pack("zh-core", { Save: "保存", "Open|verb": "打开" }) as Packed);
    expect([lookUp("Save"), lookUp("Open|verb")]).toEqual(["保存", "打开"]);
    for (const text of ["Open", "save", "", "constructor", keyOf("Save", 3)]) {
      expect(lookUp(text), text).toBeUndefined();
    }
  });

  it("are read by names where they are written by names", () => {
    const lookUp = unpack(pack("zh-functions", { SUM: "求和", number: "数值" }) as Packed);
    expect([lookUp("SUM"), lookUp("number")]).toEqual(["求和", "数值"]);
    expect([lookUp(""), lookUp("SUM number"), lookUp("toString")]).toEqual([
      undefined,
      undefined,
      undefined,
    ]);
  });

  it("have the messages with values as patterns", () => {
    const packed = pack("zh-engine", {
      Paste: "粘贴",
      "Insert {count} rows on {name}": "在 {name} 插入 {count} 行",
      "failed at {pos_hint}, column {letters}: {err}": "{letters} 列 {pos_hint} 处失败：{err}",
    });
    expect(packed.slice(2)).toEqual([
      5,
      "Insert {#} rows on {}\nfailed at {~}, column {^}: {@}",
      "在 {1} 插入 {0} 行\n{1} 列 {0} 处失败：{2}",
    ]);
    expect(unpack(packed as Packed)("Paste")).toBe("粘贴");
  });

  it("have no texts of several lines", () => {
    expect(() => pack("zh-core", { a: "一\n二" })).toThrow("several lines");
    expect(() => pack("zh-engine", { "{n}": "一\n{n}" })).toThrow("several lines");
  });
});

describe("the keys", () => {
  it("are told apart by the build", () => {
    const [one, other] = sameKey(3);
    const texts = (...sources: readonly string[]): Catalogue =>
      Object.fromEntries(sources.map((source) => [source, "译文"]));
    const check = (...catalogues: readonly [string, Catalogue][]) => checkKeys(new Map(catalogues));
    expect(() => check(["zh-core", texts(one, other)])).toThrow(/make \d+ the SEED/);
    // A text of one catalogue is looked up in the others as well.
    expect(() => check(["zh-core", texts(one)], ["zh-labs", texts(other)])).toThrow(other);
    expect(() => check(["zh-core", texts(one)], ["zh-labs", texts(one)])).not.toThrow();
    // Messages and names are looked up elsewhere.
    expect(() => check(["zh-core", texts(one)], ["zh-engine", texts(other)])).not.toThrow();
    expect(() => check(["zh-functions", texts(one, other)])).not.toThrow();
  });

  it("need nothing escaped", () => {
    const digits = new Set(Array.from({ length: 5_000 }, (_, n) => keyOf(`${n}`, 5)).join(""));
    expect(digits.size).toBe(57);
    expect([...digits].filter((digit) => /[\\"`\s]/.test(digit))).toEqual([]);
    expect(keyOf("", 3)).toHaveLength(3);
  });
});
