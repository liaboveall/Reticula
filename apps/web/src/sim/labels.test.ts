import { readFileSync } from "node:fs";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fill, type Params } from "../i18n/index.ts";
import { connectionName, invariantStatusName, invariants, labText, parseSeed } from "./labels.ts";

const rust = (path: string) =>
  readFileSync(new URL(`../../../../crates/${path}`, import.meta.url), "utf8");

/** The module with a translation that shows where it was used. */
async function marking() {
  const t = (text: string, params?: Params) => `«${fill(text, params)}»`;
  vi.resetModules();
  vi.doMock("../i18n/index.ts", async (original) => ({
    ...(await original<typeof import("../i18n/index.ts")>()),
    t,
  }));
  return import("./labels.ts");
}

afterEach(() => {
  vi.doUnmock("../i18n/index.ts");
});

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
    ["Restarted from persisted OPFS state", "Restarted from persisted OPFS state"],
    ["Rescued {} terminally rejected edit(s)", "Rescued 3 terminally rejected edit(s)"],
    ["Server epoch changed {old_epoch} -> {new_epoch}", "Server epoch changed 1 -> 2"],
    ["Digest mismatch detected at #{seq}; resyncing", "Digest mismatch detected at #7; resyncing"],
    ["row index {row} out of range", "row index 9000000 out of range"],
    ["column index {col} out of range", "column index 70000 out of range"],
    ["invalid cell input: {err:?}", "invalid cell input: Parse {\n at: 1 }"],
    ["local edit rejected: {err:?}", "local edit rejected: ReadOnly"],
  ])("translates %j, which the engine writes", async (template, message) => {
    expect(rust("sim-wasm/src/lib.rs")).toContain(`"${template}"`);
    expect(labText(message)).toBe(message);
    const marked = await marking();
    expect(marked.labText(message)).toBe(`«${message}»`);
    expect(marked.labText(`? ${message}`)).toBe(`? ${message}`);
  });

  it("takes seeds", () => {
    expect(parseSeed("42")).toBe(42n);
    expect(parseSeed("18446744073709551615")).toBe(2n ** 64n - 1n);
    for (const text of ["18446744073709551616", "-1", "1.5", "seed"]) {
      expect(parseSeed(text), text).toBeNull();
    }
  });
});
