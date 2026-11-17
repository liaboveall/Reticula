import { defineConfig, devices } from "@playwright/test";

const port = 5274;
const spikesPort = 5275;
const onCi = Boolean(process.env.CI);

// The tests run against the production build, served by `vite preview`. The
// pages that experiments are run on are built apart from the app and served
// on a port of their own; the tests of the cell editor use one of them.
export default defineConfig({
  testDir: "tests/e2e",
  forbidOnly: onCi,
  reporter: onCi ? [["github"], ["list"]] : "list",
  use: { baseURL: `http://localhost:${port}` },
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
