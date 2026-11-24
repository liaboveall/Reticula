import { defineConfig, devices } from "@playwright/test";

// Not the port the end-to-end tests have these pages on, whose server may be
// one that was left running with an older build.
const port = 5277;
const onCi = Boolean(process.env.CI);

// A software adapter for WebGPU. On Linux without a GPU the GPU process does
// not survive a WebGPU device unless everything else is drawn in software too.
const args = ["--enable-unsafe-webgpu", "--use-webgpu-adapter=swiftshader"];
if (process.platform === "linux") {
  args.push(
    "--use-angle=swiftshader",
    "--enable-unsafe-swiftshader",
    "--enable-features=Vulkan",
    "--use-vulkan=swiftshader",
    "--disable-vulkan-surface",
  );
}

// The visual regression tests: pictures of the grid, drawn by a page that is
// built apart from the app and compared by that page.
export default defineConfig({
  testDir: "tests/visual",
  testMatch: "*.spec.ts",
  // Not the directory of the end-to-end tests, which they empty.
  outputDir: "test-results/visual",
  forbidOnly: onCi,
  fullyParallel: true,
  ...(onCi ? {} : { workers: "25%" }),
  globalTimeout: 180_000,
  reporter: onCi ? [["github"], ["list"]] : "list",
  use: { baseURL: `http://localhost:${port}`, locale: "en-US" },
  webServer: {
    command: `pnpm --filter @reticula/web exec vite preview --outDir dist-spikes --port ${port}`,
    url: `http://localhost:${port}/visual.html`,
    reuseExistingServer: false,
  },
  projects: [
    {
      name: "chromium",
      // The headless shell, which is the default, has no WebGPU.
      use: { ...devices["Desktop Chrome"], channel: "chromium", launchOptions: { args } },
    },
  ],
});
