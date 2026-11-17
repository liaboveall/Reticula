import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig(({ mode }) => ({
  plugins: [react()],
  server: { port: 5273, strictPort: true },
  preview: { port: 5274, strictPort: true },
  worker: { format: "es" },
  // `--mode bench` builds the measurement page instead of the app, into a
  // directory of its own, so that nothing of it ships.
  build: mode === "bench" ? { outDir: "dist-bench", rolldownOptions: { input: "bench.html" } } : {},
}));
