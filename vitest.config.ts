import { defineConfig } from "vitest/config";

// Unit tests of the front-end packages. They run in Node: nothing here needs
// a browser or the engine.
export default defineConfig({
  test: {
    include: ["packages/*/src/**/*.test.ts", "apps/*/src/**/*.test.ts"],
  },
});
