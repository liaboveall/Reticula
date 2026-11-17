import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig(({ mode }) => ({
  plugins: [react()],
  server: { port: 5273, strictPort: true },
  preview: { port: 5274, strictPort: true },
  worker: { format: "es" },
  // `--mode spikes` builds, instead of the app, the pages that experiments
  // are run on: the one that is measured and the one for checking input
  // methods. They go into a directory of their own, so that nothing of them
  // ships.
  build:
    mode === "spikes"
      ? { outDir: "dist-spikes", rolldownOptions: { input: ["bench.html", "ime.html"] } }
      : {},
}));
