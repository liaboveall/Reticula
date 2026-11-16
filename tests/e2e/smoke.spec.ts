import { readFileSync } from "node:fs";
import { expect, test } from "@playwright/test";

// The same value the native and Node runs are checked against. Each browser
// has its own WebAssembly implementation; all of them must agree with it.
const expectedDigest = readFileSync(
  new URL("../determinism/expected.txt", import.meta.url),
  "utf8",
).trim();

test("the engine starts in a worker and passes the determinism self-check", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await expect(page.getByTestId("kernel-version")).toHaveText(/^\d+\.\d+\.\d+/);
  await expect(page.getByTestId("determinism-digest")).toHaveText(expectedDigest);
});
