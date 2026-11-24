import { fileURLToPath } from "node:url";
import type { Plugin } from "vite";

/** The module with what Vite turns every `import()` of a chunk into. */
const VITE_HELPER = /^\0vite\/preload-helper\.js$/;

const OURS = fileURLToPath(new URL("../src/load-chunk.ts", import.meta.url));

/**
 * Has chunks loaded by the app's own module. Most of Vite's is about preloading
 * scripts, which the document does here, and about addresses this app has not.
 */
export function ownChunkLoader(): Plugin {
  return {
    name: "reticula:chunk-loader",
    apply: "build",
    enforce: "pre",
    load: {
      filter: { id: VITE_HELPER },
      handler: () => `export { loadChunk as __vitePreload } from ${JSON.stringify(OURS)};`,
    },
  };
}
