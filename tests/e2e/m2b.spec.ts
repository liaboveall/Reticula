import { expect, test } from "@playwright/test";
import {
  type CollabServerHandle,
  createAndShareSheet,
  editCell,
  joinViaSharePath,
  signInDev,
  startCollabServer,
  withFileLock,
} from "./collab-helpers.ts";

test.describe("M2b governance, workbooks, history, and account lifecycle", () => {
  test.describe.configure({ mode: "serial" });

  let server: CollabServerHandle | null = null;

  test.beforeAll(async () => {
    server = await startCollabServer();
  });

  test.afterAll(async () => {
    await server?.stop();
    server = null;
  });

  test("/workbooks create, rename, delete, and convert local workbook to cloud", async ({
    browser,
  }, testInfo) => {
    test.setTimeout(90_000);
    if (!server) {
      throw new Error("server not started");
    }
    const suffix = `${testInfo.project.name}-${Date.now()}`;
    const localDocId = `local-e2e-${suffix}`;
    const createdTitle = `Cloud Created ${suffix}`;
    const renamedTitle = `Cloud Renamed ${suffix}`;

    const ctx = await browser.newContext();
    try {
      const page = await ctx.newPage();

      // Seed a small local workbook so it fits well inside the 2M collab import cap.
      await page.goto(`${server.baseUrl}/app?doc=${encodeURIComponent(localDocId)}&hooks`);
      await expect(page.getByTestId("engine-status")).toHaveText("ready");
      const backend = await page.evaluate(() => window.__reticula?.docStatus().storageBackend);
      await editCell(page, 0, 0, "A1", "LocalSeed");
      await expect
        .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 5_000 })
        .toBe("LocalSeed");
      await page.evaluate(async () => {
        await window.__reticula?.compactSnapshot();
        await window.__reticula?.flushAndClose();
      });

      await page.goto(`${server.baseUrl}/workbooks`);
      await expect(page.getByTestId("workbooks-page")).toBeVisible();
      await page.getByTestId("workbooks-dev-alice").click();
      const createBtn = page.getByTestId("workbooks-create-btn");
      await expect(createBtn).toBeVisible({ timeout: 10_000 });
      await expect(createBtn).toBeEnabled({ timeout: 10_000 });

      const titleInput = page.getByPlaceholder("New cloud workbook title");
      await titleInput.fill(createdTitle);
      await expect(titleInput).toHaveValue(createdTitle);
      await createBtn.click();
      await expect(page).toHaveURL(/\?collab=/, { timeout: 10_000 });
      const createdId = new URL(page.url()).searchParams.get("collab");
      if (!createdId) {
        throw new Error(`Expected ?collab= after create, got ${page.url()}`);
      }

      await page.goto(`${server.baseUrl}/workbooks`);
      const createdRow = page.getByTestId(`cloud-wb-${createdId}`);
      await expect(createdRow).toBeVisible({ timeout: 10_000 });
      await expect(createdRow).toContainText(createdTitle);

      await createdRow.getByRole("button", { name: "Rename" }).click();
      await createdRow.locator("input").fill(renamedTitle);
      await createdRow.getByRole("button", { name: "Save" }).click();
      await expect(createdRow).toContainText(renamedTitle, { timeout: 10_000 });

      await createdRow.getByRole("button", { name: "Delete" }).click();
      await expect(page.getByTestId("confirm-delete")).toContainText(renamedTitle);
      await page.getByTestId("confirm-delete").getByRole("button", { name: "Cancel" }).click();
      await expect(page.getByTestId("confirm-delete")).toHaveCount(0);
      await expect(createdRow).toBeVisible();
      // Cancelling keeps the cloud workbook openable.
      await createdRow.getByRole("button", { name: `Open ${renamedTitle}` }).click();
      await expect(page).toHaveURL(new RegExp(`\\?collab=${createdId}`), { timeout: 10_000 });
      await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 15_000,
      });
      await page.goto(`${server.baseUrl}/workbooks`);
      await expect(createdRow).toBeVisible({ timeout: 10_000 });
      await createdRow.getByRole("button", { name: "Delete" }).click();
      await page.getByTestId("confirm-delete-btn").click();
      await expect(page.getByTestId(`cloud-wb-${createdId}`)).toHaveCount(0, { timeout: 10_000 });

      if (backend === "memory") {
        testInfo.annotations.push({
          type: "note",
          description:
            "local-to-cloud conversion needs persistent OPFS; this browser keeps local docs in memory",
        });
      } else {
        await page.getByTestId("workbooks-tab-local").click();
        const localRow = page.getByTestId(`local-wb-${localDocId}`);
        await expect(localRow).toBeVisible({ timeout: 10_000 });
        // Cancelling local delete keeps the workbook and its saved data openable.
        await localRow.getByRole("button", { name: /^Delete Local/ }).click();
        await expect(page.getByTestId("confirm-delete")).toContainText(localDocId);
        await page.getByTestId("confirm-delete").getByRole("button", { name: "Cancel" }).click();
        await expect(page.getByTestId("confirm-delete")).toHaveCount(0);
        await expect(localRow).toBeVisible();
        await localRow.getByRole("button", { name: "Open Local" }).click();
        await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 15_000 });
        await page.goto(`${server.baseUrl}/app?hooks&doc=${encodeURIComponent(localDocId)}`);
        await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 15_000 });
        await expect
          .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 5_000 })
          .toBe("LocalSeed");
        await page.evaluate(() => window.__reticula?.flushAndClose());
        await page.goto(`${server.baseUrl}/workbooks`);
        await page.getByTestId("workbooks-tab-local").click();
        await expect(localRow).toBeVisible({ timeout: 10_000 });
        await page.getByTestId(`convert-cloud-${localDocId}`).click();
        await expect(page).toHaveURL(/\?collab=/, { timeout: 15_000 });
        const convertedId = new URL(page.url()).searchParams.get("collab");
        if (!convertedId) {
          throw new Error(`Expected ?collab= after local conversion, got ${page.url()}`);
        }

        await page.goto(`${server.baseUrl}/app?collab=${encodeURIComponent(convertedId)}&hooks`);
        await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", {
          timeout: 15_000,
        });
        await expect(page.getByTestId("engine-status")).toHaveText("ready");
        await expect
          .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 5_000 })
          .toBe("LocalSeed");
      }
    } finally {
      await ctx.close();
    }
  });

  test("anonymous viewer link, visibility kick, and live member downgrade or removal", async ({
    browser,
  }, testInfo) => {
    test.setTimeout(90_000);
    if (!server) {
      throw new Error("server not started");
    }
    const aliceCtx = await browser.newContext();
    const anonCtx = await browser.newContext();
    const bobCtx = await browser.newContext();
    try {
      const alice = await aliceCtx.newPage();
      const anon = await anonCtx.newPage();
      const bob = await bobCtx.newPage();

      await signInDev(alice, server.baseUrl, "alice");
      await alice
        .getByTestId("new-cloud-title-input")
        .fill(`ViewerMatrix ${testInfo.project.name}`);
      await alice.getByTestId("new-cloud-sheet-btn").click();
      await expect(alice.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 10_000,
      });
      await editCell(alice, 0, 0, "A1", "42");

      await alice.getByTestId("cloud-menu-btn").click();
      const visToggle = alice.getByTestId("workbook-visibility-toggle");
      await visToggle.click();
      await expect(visToggle).toBeChecked({ timeout: 10_000 });
      await expect(visToggle).toBeEnabled({ timeout: 10_000 });

      await alice.getByTestId("share-role-select").selectOption("viewer");
      await alice.getByTestId("create-share-link-btn").click();
      await expect(alice.getByTestId("share-link-url")).toHaveValue(/share=/, {
        timeout: 10_000,
      });
      const viewerUrl = await alice.getByTestId("share-link-url").inputValue();
      await alice.getByTestId("collab-modal-close").click();
      await expect(alice.getByTestId("collab-modal")).toBeHidden();

      await anon.goto(`${viewerUrl}&hooks`);
      await expect(anon.getByTestId("collab-readonly-badge")).toBeVisible({ timeout: 10_000 });
      await expect(anon.getByTestId("engine-status")).toHaveText("ready");
      await expect(anon.getByTestId("formula-input")).toHaveAttribute("contenteditable", "false");
      await expect
        .poll(() => anon.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 5_000 })
        .toBe("42");

      await editCell(alice, 1, 0, "A2", "99");
      await expect
        .poll(() => anon.evaluate(() => window.__reticula?.textAt(1, 0)), { timeout: 5_000 })
        .toBe("99");

      // Turning link_view back off immediately revokes the anonymous WebSocket connection.
      await alice.getByTestId("cloud-menu-btn").click();
      await visToggle.click();
      await expect(visToggle).not.toBeChecked({ timeout: 10_000 });
      await expect(visToggle).toBeEnabled({ timeout: 10_000 });
      await expect(anon.getByTestId("collab-notice")).toContainText("revoked", { timeout: 10_000 });

      await anon.reload();
      await expect(anon.getByTestId("collab-access-denied")).toBeVisible({ timeout: 10_000 });

      // Generate an editor link for Bob, let Bob edit, then downgrade to viewer and remove live.
      await alice.getByTestId("share-role-select").selectOption("editor");
      await alice.getByTestId("create-share-link-btn").click();
      await expect(alice.getByTestId("share-link-url")).toHaveValue(/\/join\//, {
        timeout: 10_000,
      });
      const editorUrl = await alice.getByTestId("share-link-url").inputValue();
      const editorJoinPath = new URL(editorUrl).pathname;
      await alice.getByTestId("collab-modal-close").click();
      await expect(alice.getByTestId("collab-modal")).toBeHidden();

      await joinViaSharePath(bob, server.baseUrl, editorJoinPath, "bob");
      await editCell(bob, 0, 1, "B1", "77");
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(0, 1)), { timeout: 5_000 })
        .toBe("77");

      await alice.getByTestId("cloud-menu-btn").click();
      await alice.getByLabel("Role for Bob").selectOption("viewer");
      await expect(
        alice.locator("li.collab-member-row", { hasText: "Bob" }).locator(".collab-member-sub"),
      ).toHaveText("viewer", { timeout: 10_000 });

      // Existing open WS rejects the downgraded editor's next Push and rescues the edit locally.
      await editCell(bob, 0, 2, "C1", "Blocked");
      await expect(bob.getByTestId("unsynced-banner")).toBeVisible({ timeout: 10_000 });
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(0, 2)), { timeout: 2_000 })
        .toBe("");

      await bob.reload();
      await expect(bob.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout: 10_000 });
      await expect(bob.getByTestId("collab-readonly-badge")).toBeVisible({ timeout: 10_000 });
      await expect(bob.getByTestId("formula-input")).toHaveAttribute("contenteditable", "false");

      await alice
        .locator("li.collab-member-row", { hasText: "Bob" })
        .getByRole("button", {
          name: "Remove",
        })
        .click();
      await expect(bob.getByTestId("collab-notice")).toContainText("revoked", { timeout: 10_000 });
      await bob.reload();
      await expect(bob.getByTestId("collab-access-denied")).toBeVisible({ timeout: 10_000 });
    } finally {
      await aliceCtx.close();
      await anonCtx.close();
      await bobCtx.close();
    }
  });

  test("history timeline scrubber, named version creation, and return to live", async ({
    browser,
  }, testInfo) => {
    test.setTimeout(90_000);
    if (!server) {
      throw new Error("server not started");
    }
    const ctx = await browser.newContext();
    try {
      const page = await ctx.newPage();
      await signInDev(page, server.baseUrl, "alice");
      await createAndShareSheet(page, `History Timeline ${testInfo.project.name}`);

      await editCell(page, 0, 0, "A1", "100");
      await expect
        .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 5_000 })
        .toBe("100");
      await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 10_000,
      });
      await editCell(page, 0, 0, "A1", "250");
      await expect
        .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 5_000 })
        .toBe("250");
      await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 10_000,
      });

      await page.getByTestId("history-btn").click();
      await expect(page.getByTestId("history-drawer")).toBeVisible({ timeout: 10_000 });
      await expect(page.getByTestId("history-entry-2")).toBeVisible({ timeout: 10_000 });

      await page.getByTestId("history-version-name-input").fill("Milestone 1");
      await page.getByTestId("history-save-version-btn").click();
      await expect(page.getByTestId("history-versions-list")).toContainText("Milestone 1", {
        timeout: 10_000,
      });

      await page.getByTestId("history-step-prev").click();
      await expect(page.getByTestId("history-preview-banner")).toBeVisible({ timeout: 10_000 });
      await expect
        .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 5_000 })
        .toBe("100");

      await page.getByTestId("history-return-live-btn").click();
      await expect(page.getByTestId("history-preview-banner")).toBeHidden({ timeout: 10_000 });
      await expect
        .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 5_000 })
        .toBe("250");
    } finally {
      await ctx.close();
    }
  });

  test("/privacy, /account storage usage, and carol deletion preserving collaborator history", async ({
    browser,
  }, testInfo) => {
    test.setTimeout(90_000);
    if (!server) {
      throw new Error("server not started");
    }
    const activeServer = server;
    await withFileLock("reticula-e2e-carol.lock", async () => {
      const aliceCtx = await browser.newContext();
      const carolCtx = await browser.newContext();
      try {
        const alice = await aliceCtx.newPage();
        const carol = await carolCtx.newPage();

        await signInDev(alice, activeServer.baseUrl, "alice");
        const { joinPath } = await createAndShareSheet(
          alice,
          `Account Delete ${testInfo.project.name}`,
        );
        await joinViaSharePath(carol, activeServer.baseUrl, joinPath, "carol");

        await editCell(carol, 0, 2, "C1", "CarolNote");
        await expect
          .poll(() => alice.evaluate(() => window.__reticula?.textAt(0, 2)), { timeout: 5_000 })
          .toBe("CarolNote");

        await carol.goto(`${activeServer.baseUrl}/privacy`);
        await expect(carol.getByTestId("privacy-page")).toBeVisible();
        await expect(carol.getByTestId("privacy-page")).toContainText("Privacy & Data Notice");

        await carol.goto(`${activeServer.baseUrl}/account`);
        await expect(carol.getByTestId("account-page")).toBeVisible();
        await expect(carol.getByTestId("account-storage-usage")).toContainText("used of", {
          timeout: 10_000,
        });
        await expect(carol.getByTestId("delete-account-btn")).toBeDisabled();

        await carol.getByTestId("delete-account-confirm-input").fill("DELETE");
        await expect(carol.getByTestId("delete-account-btn")).toBeEnabled();
        await carol.getByTestId("delete-account-btn").click();
        await expect(carol).toHaveURL(`${activeServer.baseUrl}/`, { timeout: 10_000 });

        await alice.getByTestId("history-btn").click();
        await expect(alice.getByTestId("history-drawer")).toBeVisible({ timeout: 10_000 });
        await expect(alice.getByTestId("history-entries-list")).toContainText("Deleted user", {
          timeout: 10_000,
        });
        await expect
          .poll(() => alice.evaluate(() => window.__reticula?.textAt(0, 2)), { timeout: 5_000 })
          .toBe("CarolNote");
      } finally {
        await aliceCtx.close();
        await carolCtx.close();
      }
    });
  });
});
