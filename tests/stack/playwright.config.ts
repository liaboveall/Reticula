import { defineConfig, devices } from "@playwright/test";

// A run-through of the whole service as `infra/compose.yml` starts it: the
// release image, PostgreSQL 18 and the snapshot volume, under the memory and
// CPU limits of the target host. `cargo xtask stack` brings the stack up,
// runs this and takes it down again.
export default defineConfig({
  testDir: ".",
  forbidOnly: Boolean(process.env.CI),
  reporter: "list",
  timeout: 180_000,
  use: {
    baseURL: process.env.RETICULA_STACK_URL ?? "http://127.0.0.1:3200",
    locale: "en-US",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
