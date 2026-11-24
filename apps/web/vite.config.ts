import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import { packedCatalogues } from "./build/catalogues.ts";
import { ownChunkLoader } from "./build/load-chunk.ts";
import { namedReact } from "./build/react.ts";

const apiTarget = process.env.RETICULA_SERVER_URL ?? "http://127.0.0.1:3200";

// The names of files are part of every chunk that loads them and of the service
// worker, and a hash is the one part of them that does not compress. Six digits
// of it tell the builds of a file apart.
const HASHED = "assets/[name]-[hash:6]";
const FILE_NAMES = {
  entryFileNames: `${HASHED}.js`,
  chunkFileNames: `${HASHED}.js`,
  assetFileNames: `${HASHED}.[ext]`,
};

// The grid's WebGPU renderer is a chunk of its own with a size budget, which
// finds it by this name (`crates/xtask/src/web/bundle.rs`). Any chunk with a
// part of it gets the name: a static import of one then fails the build, and
// code the renderer alone uses does not hide it from its budget.
const WEBGPU_MODULE = /\/grid\/src\/render\/webgpu\//;

// Small modules that no first screen needs and that most of what is loaded later
// shares. Left alone they are a dozen files of some hundred bytes each. The entry
// must not import any of them: they import the entry.
const SHARED_MODULE =
  /[\\/]src[\\/](?:ui[\\/](?:Dialog|drawer|focus|Popup|roving|Tabs)|(?:collab|pages)[\\/]labels|pages[\\/]helpers|i18n[\\/](?:engine-text|formats)|generated[\\/]collab-api|download)\.tsx?$/;
/** What every page but the landing page and the editor starts with. */
const PAGE_MODULE = /[\\/]src[\\/]pages[\\/](?:errors|PageNav)\.tsx?$/;
/** What the editor and the pages with workbooks start with. */
const CLIENT_MODULE = /[\\/]src[\\/](?:engine[\\/]client|ui[\\/]ActionButton)\.tsx?$/;
const LANGUAGE_MODULE = /[\\/]src[\\/](?:i18n[\\/]languages|pages[\\/]LanguageSwitch)\.tsx?$/;
// What is opened in the editor is loaded when it is opened. These belong together.
const DIALOG_MODULE = /[\\/]src[\\/](?:LocalModals|M3aToolsModal)\.tsx$/;
const CLOUD_MODULE = /[\\/]src[\\/]collab[\\/](?:CollabHost|CollabShell|HistoryDrawer)\.tsx$/;

// The style sheet of the document, which no chunk has to load.
const DOCUMENT_STYLES = /^assets\/index-[^/]*\.css$/;

export default defineConfig(({ mode }) => ({
  plugins: [react(), namedReact(), packedCatalogues(), ownChunkLoader()],
  server: {
    port: 5273,
    strictPort: true,
    proxy: {
      "/api": {
        target: apiTarget,
        ws: true,
      },
    },
  },
  preview: { port: 5274, strictPort: true },
  worker: { format: "es", rolldownOptions: { output: { ...FILE_NAMES, comments: false } } },
  // `--mode spikes` builds the experiment pages apart from the app.
  // `--mode sw` compiles the hand-written Service Worker (`src/sw.ts`) into
  // `dist/sw.js` without clearing the app build output (`14-m1b.md` 7.4).
  build:
    mode === "spikes"
      ? {
          outDir: "dist-spikes",
          // The worker's names, so that the engine is one file for both.
          rolldownOptions: {
            input: ["bench.html", "ime.html", "visual.html"],
            output: FILE_NAMES,
          },
        }
      : mode === "sw"
        ? {
            outDir: "dist",
            emptyOutDir: false,
            rolldownOptions: {
              input: "src/sw.ts",
              output: {
                entryFileNames: "sw.js",
                format: "iife",
              },
            },
          }
        : {
            // `cargo xtask build` reads the manifest for the pages' static
            // closures and deletes it.
            manifest: true,
            // What a page needs is preloaded by the document's inline script, and
            // the names of everything else's parts would be a part of every page.
            // Style sheets are loaded by way of this list.
            modulePreload: {
              polyfill: false,
              resolveDependencies: (_, files) =>
                files.filter((file) => file.endsWith(".css") && !DOCUMENT_STYLES.test(file)),
            },
            rolldownOptions: {
              output: {
                ...FILE_NAMES,
                codeSplitting: {
                  // What they depend on, such as React, stays where it is.
                  includeDependenciesRecursively: false,
                  groups: [
                    { name: "shared", test: SHARED_MODULE },
                    { name: "page", test: PAGE_MODULE },
                    { name: "client", test: CLIENT_MODULE },
                    { name: "languages", test: LANGUAGE_MODULE },
                    { name: "dialogs", test: DIALOG_MODULE },
                    { name: "cloud", test: CLOUD_MODULE },
                  ],
                },
                chunkFileNames: ({ moduleIds }) =>
                  moduleIds.some((id) => WEBGPU_MODULE.test(id))
                    ? "assets/webgpu-[hash:6].js"
                    : FILE_NAMES.chunkFileNames,
              },
            },
          },
}));
