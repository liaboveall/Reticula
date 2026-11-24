import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const apiTarget = process.env.RETICULA_SERVER_URL ?? "http://127.0.0.1:3200";

// The grid's WebGPU renderer is a chunk of its own with a size budget, which
// finds it by this name (`crates/xtask/src/web/bundle.rs`). Any chunk with a
// part of it gets the name: a static import of one then fails the build, and
// code the renderer alone uses does not hide it from its budget.
const WEBGPU_MODULE = /\/grid\/src\/render\/webgpu\//;

export default defineConfig(({ mode }) => ({
  plugins: [react()],
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
  worker: { format: "es" },
  // `--mode spikes` builds the experiment pages apart from the app.
  // `--mode sw` compiles the hand-written Service Worker (`src/sw.ts`) into
  // `dist/sw.js` without clearing the app build output (`14-m1b.md` 7.4).
  build:
    mode === "spikes"
      ? { outDir: "dist-spikes", rolldownOptions: { input: ["bench.html", "ime.html"] } }
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
            rolldownOptions: {
              output: {
                chunkFileNames: ({ moduleIds }) =>
                  moduleIds.some((id) => WEBGPU_MODULE.test(id))
                    ? "assets/webgpu-[hash].js"
                    : "assets/[name]-[hash].js",
              },
            },
          },
}));
