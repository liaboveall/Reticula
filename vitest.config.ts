import { defineConfig } from "vitest/config";

// Unit tests of the front-end packages, and of what the visual regression tests
// are made with. They run in Node: nothing here needs a browser or the engine.
export default defineConfig({
  test: {
    include: ["packages/*/src/**/*.test.ts", "apps/*/src/**/*.test.ts", "tests/visual/*.test.ts"],
  },
});
