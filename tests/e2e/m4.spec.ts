import { expect, type Page, test } from "@playwright/test";
import {
  type CollabServerHandle,
  createAndShareSheet,
  editCell,
  joinViaSharePath,
  settled,
  signInDev,
  startCollabServer,
} from "./collab-helpers.ts";

async function synced(page: Page, timeout = 20_000): Promise<void> {
  await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout });
}

test.describe("M4 AI Analyst — visitor pure-client replays (no sign-in)", () => {
  test("lists all 10 demos, replays Regional Sales Summary, and enforces 3-way conflict + reference closure guard on demo #10", async ({
    page,
  }) => {
    await page.goto("/app?hooks");
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });

    await page.getByTestId("ai-panel-btn").click();
    await expect(page.getByTestId("ai-panel")).toBeVisible();
    await expect(page.getByTestId("ai-demo-list").locator("button")).toHaveCount(10, {
      timeout: 10_000,
    });

    // 1. Demo #1: Regional Sales Summary -> isolated draft preview -> apply to local sheet.
    await page.getByTestId("ai-demo-regional-sales-summary").click();
    await expect(page.getByTestId("ai-draft-banner")).toContainText("Viewing AI Draft");
    await page.getByTestId("ai-replay-step").click();
    await page.getByTestId("ai-replay-review-btn").click();
    await expect(page.getByTestId("ai-diff-review")).toBeVisible();
    await expect(page.getByTestId("ai-merge-apply")).toBeEnabled();
    await page.getByTestId("ai-merge-apply").click();
    await expect(page.getByTestId("ai-draft-banner")).toBeHidden({ timeout: 10_000 });
    await expect
      .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 4)), { timeout: 10_000 })
      .toBe("Region");
    await expect
      .poll(() => page.evaluate(() => window.__reticula?.textAt(1, 4)), { timeout: 10_000 })
      .toBe("South");

    // 2. Demo #10: Conflict & Reference Closure Lab. The merged one is put away.
    await expect(page.getByTestId("ai-replay-play")).toHaveCount(0);
    await page.getByTestId("ai-demo-conflict-and-closure-demo").click();
    await expect(page.getByTestId("ai-draft-banner")).toBeVisible();
    await page.getByTestId("ai-replay-review-btn").click();
    await expect(page.getByTestId("ai-diff-review")).toBeVisible();

    // Unresolved conflict disables Apply until Keep Main / Use AI Draft is chosen.
    await expect(page.getByTestId("ai-merge-apply")).toBeDisabled();
    await page.getByTestId("conflict-take-draft-B3").click();
    await expect(page.getByTestId("ai-merge-apply")).toBeEnabled();

    // Unchecking the inserted Tax row triggers the static Reference Closure Alert.
    await page.getByTestId("hunk-check-struct-1").uncheck();
    await expect(page.getByTestId("ai-closure-warning")).toBeVisible();
    await expect(page.getByTestId("ai-merge-apply")).toBeDisabled();

    // One-click "Accept insert together" resolves closure cleanly.
    await page.getByTestId("ai-closure-accept-together").click();
    await expect(page.getByTestId("ai-closure-warning")).toBeHidden();
    await expect(page.getByTestId("ai-merge-apply")).toBeEnabled();
    await page.getByTestId("ai-merge-apply").click();
    await expect(page.getByTestId("ai-draft-banner")).toBeHidden({ timeout: 10_000 });

    // Verify merged formula C1 (=B2+B3 = 35+520 = 555) evaluated in real WASM FormulaEngine!
    await expect
      .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 2)), { timeout: 10_000 })
      .toBe("555");
  });
});

test.describe("M4 AI Analyst — collaborative cloud draft, conflict/closure review, and live sync", () => {
  test.describe.configure({ mode: "serial" });

  let server: CollabServerHandle | null = null;

  test.beforeAll(async () => {
    server = await startCollabServer();
  });

  test.afterAll(async () => {
    if (server) {
      await server.stop();
      server = null;
    }
  });

  test("cloud draft run streams steps, catches concurrent main edit conflict & closure guard, and syncs merged changes to second collaborator", async ({
    browser,
  }) => {
    test.setTimeout(60_000);
    if (!server) throw new Error("server not started");

    const aliceCtx = await browser.newContext();
    const bobCtx = await browser.newContext();
    try {
      const alice = await aliceCtx.newPage();
      const bob = await bobCtx.newPage();

      await signInDev(alice, server.baseUrl, "alice");
      const { joinPath } = await createAndShareSheet(alice, "M4 Cloud Draft");
      await synced(alice);

      await editCell(alice, 0, 1, "B1", "10");
      await synced(alice);

      await joinViaSharePath(bob, server.baseUrl, joinPath, "bob");
      await synced(bob);
      await settled(bob);
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(0, 1)), { timeout: 10_000 })
        .toBe("10");

      // Alice runs `[closure]` prompt on an isolated draft branch.
      await alice.getByTestId("ai-panel-btn").click();
      await alice
        .getByTestId("ai-prompt-input")
        .fill("[closure] Insert row 2 with A2=25 and set B1 = =A2*2");
      await alice.getByTestId("ai-run-btn").click();
      await expect(alice.getByTestId("ai-draft-banner")).toContainText("Viewing AI Draft", {
        timeout: 15_000,
      });
      await expect(alice.getByTestId("ai-diff-review")).toBeVisible({ timeout: 20_000 });

      // While Alice reviews, Bob concurrently edits B1 = 999 on main!
      await editCell(bob, 0, 1, "B1", "999");
      await synced(bob);

      // Verify Alice's isolated AI Draft Preview canvas was NOT overwritten by Bob's live main edit!
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(0, 1)), { timeout: 10_000 })
        .toBe("50");
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(1, 0)), { timeout: 10_000 })
        .toBe("25");

      // Alice clicks Apply -> server detects concurrent conflict (409) and updates Alice's Diff drawer!
      await alice.getByTestId("ai-merge-apply").click();
      await expect(alice.getByTestId("conflict-take-draft-B1")).toBeVisible({ timeout: 10_000 });

      // Alice picks Use AI Draft on B1, tests closure guard by unchecking structural hunk, then accepts together!
      await alice.getByTestId("conflict-take-draft-B1").click();
      const structCheck = alice.locator('[data-testid^="hunk-check-struct-"]').first();
      await structCheck.uncheck();
      await expect(alice.getByTestId("ai-closure-warning")).toBeVisible({ timeout: 10_000 });
      await expect(alice.getByTestId("ai-merge-apply")).toBeDisabled();

      await alice.getByTestId("ai-closure-accept-together").click();
      await expect(alice.getByTestId("ai-closure-warning")).toBeHidden({ timeout: 10_000 });
      await expect(alice.getByTestId("ai-merge-apply")).toBeEnabled({ timeout: 10_000 });
      await alice.getByTestId("ai-merge-apply").click();

      // Draft preview closes and Bob's live window immediately receives the merged row & formula (=A2*2 = 50)!
      await expect(alice.getByTestId("ai-draft-banner")).toBeHidden({ timeout: 15_000 });
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(1, 0)), { timeout: 15_000 })
        .toBe("25");
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(0, 1)), { timeout: 15_000 })
        .toBe("50");
    } finally {
      await aliceCtx.close();
      await bobCtx.close();
    }
  });
});
