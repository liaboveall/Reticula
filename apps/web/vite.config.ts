import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig(({ mode }) => ({
  plugins: [react()],
  server: { port: 5273, strictPort: true },
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
        : {},
}));
