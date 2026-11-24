import { defineConfig, devices } from "@playwright/test";

const port = 5278;
const onCi = Boolean(process.env.CI);

// The app drawn with WebGPU, by a software adapter that is there on every
// machine with this browser. It is slow, hence the small window. Nothing here
// needs the server or its database.
export default defineConfig({
  testDir: "tests/e2e",
  testMatch: "webgpu.spec.ts",
  // Not the directory of the other end-to-end tests, which they empty.
  outputDir: "test-results/webgpu",
  forbidOnly: onCi,
  fullyParallel: true,
  workers: 2,
  reporter: onCi ? [["github"], ["list"]] : "list",
  use: { baseURL: `http://localhost:${port}`, locale: "en-US" },
  webServer: {
    command: `pnpm --filter @reticula/web exec vite preview --port ${port}`,
    url: `http://localhost:${port}`,
    reuseExistingServer: false,
  },
  projects: [
    {
      name: "webgpu",
      use: {
        ...devices["Desktop Chrome"],
        viewport: { width: 800, height: 500 },
        // The headless shell, which is the default, has no WebGPU.
        channel: "chromium",
        launchOptions: {
          args: ["--enable-unsafe-webgpu", "--use-webgpu-adapter=swiftshader"],
        },
      },
    },
  ],
});
