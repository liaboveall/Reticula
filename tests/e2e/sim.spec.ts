import { expect, test } from "@playwright/test";

test.describe("M2b sync simulator (/sim) and lazy WASM split", () => {
  test("/app does not eagerly load reticula_sim_wasm", async ({ page }) => {
    const requestedUrls: string[] = [];
    page.on("request", (req) => {
      requestedUrls.push(req.url());
    });

    await page.goto("/app?hooks");
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 15_000 });

    const simWasmRequests = requestedUrls.filter((u) => u.includes("reticula_sim_wasm"));
    expect(simWasmRequests).toEqual([]);
  });

  test("interactive 2/3-replica delayed merge, formula evaluation, and top-row insert", async ({
    page,
  }) => {
    test.setTimeout(45_000);
    const requestedUrls: string[] = [];
    page.on("request", (req) => {
      requestedUrls.push(req.url());
    });

    await page.goto("/sim");
    await expect(page.getByTestId("sim-lab-page")).toBeVisible();
    await expect(page.getByTestId("sim-convergence-badge")).toHaveText("All Replicas Converged", {
      timeout: 15_000,
    });
    expect(requestedUrls.some((u) => u.includes("reticula_sim_wasm"))).toBe(true);

    await expect(page.locator('[data-testid="sim-grid-0"] canvas')).toBeVisible();
    await expect(page.locator('[data-testid="sim-grid-1"] canvas')).toBeVisible();

    // Hold replica 1 on a delayed link while both replicas apply edits + a SUM formula.
    await page.getByTestId("sim-link-1").selectOption("delayed");

    await page.getByTestId("sim-row-0").fill("0");
    await page.getByTestId("sim-col-0").fill("0");
    await page.getByTestId("sim-value-0").fill("10");
    await page.getByTestId("sim-apply-0").click();

    await page.getByTestId("sim-row-1").fill("0");
    await page.getByTestId("sim-col-1").fill("1");
    await page.getByTestId("sim-value-1").fill("20");
    await page.getByTestId("sim-apply-1").click();

    await page.getByTestId("sim-row-0").fill("0");
    await page.getByTestId("sim-col-0").fill("2");
    await page.getByTestId("sim-value-0").fill("=SUM(A1:B1)");
    await page.getByTestId("sim-apply-0").click();

    await expect(page.getByTestId("sim-convergence-badge")).toHaveText("Diverged / Pending Sync");
    await expect(page.getByTestId("sim-wire-inspector")).not.toContainText("0 frames queued");

    await page.getByTestId("sim-deliver-all").click();
    await expect(page.getByTestId("sim-convergence-badge")).toHaveText("All Replicas Converged");
    await page.getByTestId("sim-link-1").selectOption("online");

    // Expand to 3 replicas and deliver a top-row insertion across a delayed third replica.
    await page.getByTestId("sim-replica-count").selectOption("3");
    await expect(page.getByTestId("sim-replica-card-2")).toBeVisible();
    await expect(page.locator('[data-testid="sim-grid-2"] canvas')).toBeVisible();

    await page.getByTestId("sim-link-2").selectOption("delayed");
    await page.getByTestId("sim-insert-row-0").click();
    await expect(page.getByTestId("sim-convergence-badge")).toHaveText("Diverged / Pending Sync");

    await page.getByTestId("sim-deliver-all").click();
    await expect(page.getByTestId("sim-convergence-badge")).toHaveText("All Replicas Converged");
  });

  test("5-replica deterministic auto-run verifies all 10 protocol invariants for seed 42", async ({
    page,
  }) => {
    test.setTimeout(45_000);
    await page.goto("/sim");
    await expect(page.getByTestId("sim-mode-autorun")).toBeVisible({ timeout: 15_000 });
    await page.getByTestId("sim-mode-autorun").click();

    await page.getByTestId("sim-seed-input").fill("42");
    await page.getByTestId("sim-autorun-btn").click();

    await expect(page.getByTestId("sim-autorun-status")).toHaveText("10 Invariants Verified", {
      timeout: 30_000,
    });
    const invariantItems = page.locator('[data-testid="sim-invariants-list"] > li');
    await expect(invariantItems).toHaveCount(10);
    for (let i = 0; i < 10; i += 1) {
      await expect(invariantItems.nth(i)).toContainText("Verified");
    }
  });
});
