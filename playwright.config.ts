import { defineConfig, devices } from "@playwright/test";

const port = 5274;
const onCi = Boolean(process.env.CI);

// The tests run against the production build, served by `vite preview`.
export default defineConfig({
  testDir: "tests/e2e",
  forbidOnly: onCi,
  reporter: onCi ? [["github"], ["list"]] : "list",
  use: { baseURL: `http://localhost:${port}` },
  webServer: {
    command: "pnpm --filter @reticula/web exec vite preview",
    url: `http://localhost:${port}`,
    reuseExistingServer: !onCi,
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
  ],
});
