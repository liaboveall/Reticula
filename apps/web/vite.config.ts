import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react()],
  server: { port: 5273, strictPort: true },
  preview: { port: 5274, strictPort: true },
  worker: { format: "es" },
});
