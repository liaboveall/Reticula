import { build } from "vite";
import { expect, it } from "vitest";
import { ownChunkLoader } from "./load-chunk.ts";

it("takes the place of Vite's module", async () => {
  const modules: Readonly<Record<string, string>> = {
    "virtual:entry": 'export const load = () => import("virtual:page");',
    "virtual:page": "export default 1;",
  };
  const output = await build({
    configFile: false,
    logLevel: "silent",
    plugins: [
      ownChunkLoader(),
      {
        name: "modules",
        resolveId: (id) => (id in modules ? id : null),
        load: (id) => modules[id] ?? null,
      },
    ],
    build: {
      write: false,
      minify: false,
      rolldownOptions: { input: "virtual:entry", preserveEntrySignatures: "strict" },
    },
  });
  const [first] = [output].flat();
  const code =
    first && "output" in first
      ? first.output.map((o) => ("code" in o ? o.code : "")).join("\n")
      : "";
  expect(code).toContain("loadChunk(() => import(");
  expect(code).not.toContain("modulepreload");
});
