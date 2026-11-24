import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import zhEngine from "../i18n/zh-engine.ts";
import { connectionName, invariantStatusName, invariants, parseSeed } from "./labels.ts";

const rust = (path: string) =>
  readFileSync(new URL(`../../../../crates/${path}`, import.meta.url), "utf8");

describe("the simulator's page", () => {
  it("lists as many invariants as the simulator numbers", () => {
    const numbers = [...rust("sim/src/world.rs").matchAll(/Invariant #(\d+)/g)].map(([, n]) =>
      Number(n),
    );
    expect(Math.max(...numbers)).toBe(invariants().length);
    expect(new Set(invariants()).size).toBe(invariants().length);
  });

  it("knows the states of connections by the simulator's words", () => {
    const words = [
      ...rust("sim-wasm/src/lib.rs").matchAll(/ConnectionState::\w+.* => "(\w+)",/g),
    ].map(([, word = ""]) => word);
    expect(words).toHaveLength(4);
    expect(words.map(connectionName)).toEqual(words);
    expect(connectionName("something new")).toBe("something new");
    expect(invariantStatusName("verified")).toBe("Verified");
  });

  it.each([
    "Restarted from persisted OPFS state",
    "Rescued {} terminally rejected edit(s)",
    "Server epoch changed {old_epoch} -> {new_epoch}",
    "Digest mismatch detected at #{seq}; resyncing",
    "row index {row} out of range",
    "column index {col} out of range",
    "invalid cell input: {err:?}",
    "local edit rejected: {err:?}",
  ])("has a translation of %j, which the engine writes", (template) => {
    expect(rust("sim-wasm/src/lib.rs")).toContain(`"${template}"`);
    const bare = (text: string) => text.replace(/\{[^}]*\}/g, "{}");
    expect(Object.keys(zhEngine).map(bare)).toContain(bare(template));
  });

  it("takes seeds", () => {
    expect(parseSeed("42")).toBe(42n);
    expect(parseSeed("18446744073709551615")).toBe(2n ** 64n - 1n);
    for (const text of ["18446744073709551616", "-1", "1.5", "seed"]) {
      expect(parseSeed(text), text).toBeNull();
    }
  });
});
