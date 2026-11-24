import { defineConfig, devices } from "@playwright/test";

const port = 5274;
const spikesPort = 5275;
const onCi = Boolean(process.env.CI);

// The tests run against the production build, served by `vite preview`. The
// pages that experiments are run on are built apart from the app and served
// on a port of their own; the tests of the cell editor use one of them.
export default defineConfig({
  testDir: "tests/e2e",
  // Needs a WebGPU adapter: see playwright.webgpu.config.ts.
  testIgnore: "webgpu.spec.ts",
  forbidOnly: onCi,
  // Half the logical processors (Playwright's default, 16 on the development
  // machine) overloads it once the collaborative tests add a server and two
  // browser contexts each: the million-row fill test then timed out in
  // Firefox. CI runners have two processors.
  ...(onCi ? {} : { workers: "25%" }),
  reporter: onCi ? [["github"], ["list"]] : "list",
  // The app's language and its numbers follow the browser's locale, which would
  // otherwise be the machine's.
  use: { baseURL: `http://localhost:${port}`, locale: "en-US" },
  webServer: [
    {
      command: "pnpm --filter @reticula/web exec vite preview",
      url: `http://localhost:${port}`,
      reuseExistingServer: !onCi,
    },
    {
      command: `pnpm --filter @reticula/web exec vite preview --outDir dist-spikes --port ${spikesPort}`,
      url: `http://localhost:${spikesPort}/ime.html`,
      reuseExistingServer: !onCi,
    },
  ],
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
  ],
});
