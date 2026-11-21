import { expect, test } from "@playwright/test";
import {
  type CollabServerHandle,
  createAndShareSheet,
  editCell,
  joinViaSharePath,
  signInDev,
  startCollabServer,
} from "./collab-helpers.ts";

test.describe("M2a collaborative workbooks", () => {
  test.describe.configure({ mode: "serial" });

  let server: CollabServerHandle | null = null;

  test.beforeAll(async () => {
    server = await startCollabServer();
  });

  test.afterAll(async () => {
    await server?.stop();
    server = null;
  });

  test("a slow sign-in check still finishes while the page keeps starting up", async ({
    browser,
  }) => {
    if (!server) {
      throw new Error("server not started");
    }
    const ctx = await browser.newContext();
    try {
      const page = await ctx.newPage();
      // Hold the answer back while the engine starts and the page re-renders.
      await page.route("**/api/auth/me", async (route) => {
        await new Promise((resolve) => setTimeout(resolve, 3_000));
        await route.continue();
      });
      await page.goto(`${server.baseUrl}/app?hooks`);
      await page.getByTestId("cloud-menu-btn").click();
      await expect(page.getByTestId("dev-login-alice")).toBeVisible({ timeout: 10_000 });
    } finally {
      await ctx.close();
    }
  });

  test("dual-window real-time sync, presence tracking across row insert, and conditional undo", async ({
    browser,
  }) => {
    if (!server) {
      throw new Error("server not started");
    }
    const aliceCtx = await browser.newContext();
    const bobCtx = await browser.newContext();
    try {
      const alice = await aliceCtx.newPage();
      const bob = await bobCtx.newPage();

      await signInDev(alice, server.baseUrl, "alice");
      const { joinPath } = await createAndShareSheet(alice, "Q4 Planning");
      await joinViaSharePath(bob, server.baseUrl, joinPath, "bob");

      // Alice writes values + SUM formula; Bob sees them in < 2s.
      await editCell(alice, 1, 1, "B2", "10");
      await editCell(alice, 2, 1, "B3", "20");
      await editCell(alice, 3, 1, "B4", "=SUM(B2:B3)");
      await expect(alice.getByTestId("collab-status-pill")).toHaveText("Synced");

      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(3, 1)), { timeout: 2_000 })
        .toBe("30");

      // Remote presence cursor & name tag appear and shift cleanly across Insert Row Above.
      await bob.evaluate(() => window.__reticula?.select(2, 2)); // C3
      await expect(alice.getByTestId("collab-peers")).toContainText("Bob", { timeout: 3_000 });
      await expect
        .poll(() =>
          alice.evaluate(() => window.__reticula?.collabStatus().remotePresences[0]?.cursorRow),
        )
        .toBe(2);

      // Insert row above row 2 (A2 -> row=1); Bob's cursor C3 moves to C4 (row 3), SUM moves to B5 (row 4).
      await alice.evaluate(() => window.__reticula?.select(1, 0));
      await alice.getByTestId("insert-row-above").click();
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(4, 1)), { timeout: 3_000 })
        .toBe("30");
      await expect
        .poll(() =>
          alice.evaluate(() => window.__reticula?.collabStatus().remotePresences[0]?.cursorRow),
        )
        .toBe(3);

      // Conditional undo (`SetCellsIf`) protects Bob's overwrite on A1.
      await editCell(alice, 0, 0, "A1", "alice-v1");
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 3_000 })
        .toBe("alice-v1");

      await editCell(bob, 0, 0, "A1", "bob-v2");
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 3_000 })
        .toBe("bob-v2");
      await expect(alice.getByTestId("collab-status-pill")).toHaveText("Synced");

      await alice.getByTestId("undo-btn").click();
      await expect(alice.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 5_000,
      });
      await expect.poll(() => alice.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("bob-v2");
      await expect.poll(() => bob.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("bob-v2");
    } finally {
      await aliceCtx.close();
      await bobCtx.close();
    }
  });

  test("offline edits with concurrent structural row insert converge on reconnect", async ({
    browser,
  }) => {
    if (!server) {
      throw new Error("server not started");
    }
    const aliceCtx = await browser.newContext();
    const bobCtx = await browser.newContext();
    try {
      const alice = await aliceCtx.newPage();
      const bob = await bobCtx.newPage();

      await signInDev(alice, server.baseUrl, "alice");
      const { joinPath } = await createAndShareSheet(alice, "Offline Merge");
      await joinViaSharePath(bob, server.baseUrl, joinPath, "bob");

      // Take Alice offline cleanly.
      await alice.evaluate(() => window.__reticula?.setCollabOffline(true));
      await aliceCtx.setOffline(true);
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.collabStatus().connection))
        .toBe("offline");

      // Alice queues 2 edits offline.
      await editCell(alice, 0, 0, "A1", "offline-a1");
      await editCell(alice, 4, 3, "D5", "99");
      await expect(alice.getByTestId("collab-status-pill")).toHaveText("Offline · 2 unsent");

      // Meanwhile online Bob inserts 1 row above row 2 and edits D1.
      await bob.evaluate(() => window.__reticula?.select(1, 0));
      await bob.getByTestId("insert-row-above").click();
      await editCell(bob, 0, 3, "D1", "bob-online");
      await expect(bob.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout: 5_000 });

      // Reconnect Alice.
      await aliceCtx.setOffline(false);
      await alice.evaluate(() => window.__reticula?.setCollabOffline(false));
      await expect(alice.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 10_000,
      });
      await expect(bob.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout: 10_000 });

      // Old row 5 (rowId=4, D5) shifted down to row 6 (index 5, D6) on both clients!
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 5_000 })
        .toBe("offline-a1");
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(0, 3)), { timeout: 5_000 })
        .toBe("bob-online");
      await expect
        .poll(() => bob.evaluate(() => window.__reticula?.textAt(5, 3)), { timeout: 5_000 })
        .toBe("99");
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(5, 3)), { timeout: 5_000 })
        .toBe("99");

      const aliceDigest = await alice.evaluate(() => window.__reticula?.stateDigest());
      const bobDigest = await bob.evaluate(() => window.__reticula?.stateDigest());
      expect(aliceDigest).toBeTruthy();
      expect(aliceDigest).toBe(bobDigest);
    } finally {
      await aliceCtx.close();
      await bobCtx.close();
    }
  });

  test("multi-tab allocates distinct replicas and reopened window takes over unsent WAL", async ({
    browser,
  }) => {
    if (!server) {
      throw new Error("server not started");
    }
    const aliceCtx = await browser.newContext();
    try {
      const tab1 = await aliceCtx.newPage();
      await signInDev(tab1, server.baseUrl, "alice");
      const { collabUrl } = await createAndShareSheet(tab1, "Replica Takeover");

      const status1 = await tab1.evaluate(() => window.__reticula?.collabStatus());
      expect(status1?.slot).toBe("1");
      expect(status1?.replica).not.toBeNull();

      const tab2 = await aliceCtx.newPage();
      await tab2.goto(collabUrl);
      await expect(tab2.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 10_000,
      });
      const status2 = await tab2.evaluate(() => window.__reticula?.collabStatus());
      expect(status2?.slot).not.toBe(status1?.slot);
      expect(status2?.replica).not.toBe(status1?.replica);
      await tab2.close();

      // Verify persistent closed-window takeover when OPFS is active (`storageBackend !== "memory"`).
      const backend = await tab1.evaluate(() => window.__reticula?.docStatus().storageBackend);
      if (backend === "memory") {
        // Playwright's WebKit on Windows cannot open sync access handles.
        test.info().annotations.push({
          type: "not covered",
          description: "closed-window takeover needs OPFS; this browser keeps replicas in memory",
        });
        await tab1.close();
        return;
      }

      await tab1.evaluate(() => window.__reticula?.setCollabOffline(true));
      await expect
        .poll(() => tab1.evaluate(() => window.__reticula?.collabStatus().connection))
        .toBe("offline");
      await editCell(tab1, 3, 2, "C4", "unsent-rescue-42");
      await expect(tab1.getByTestId("collab-status-pill")).toHaveText("Offline · 1 unsent");
      await tab1.close();

      const tab3 = await aliceCtx.newPage();
      await tab3.goto(collabUrl);
      await expect(tab3.getByTestId("collab-status-pill")).toHaveText("Synced", {
        timeout: 10_000,
      });
      const status3 = await tab3.evaluate(() => window.__reticula?.collabStatus());
      expect(status3?.slot).toBe(status1?.slot);
      expect(status3?.replica).toBe(status1?.replica);
      await expect
        .poll(() => tab3.evaluate(() => window.__reticula?.textAt(3, 2)))
        .toBe("unsent-rescue-42");
      await expect(tab3.getByTestId("collab-status-pill")).toHaveText("Synced");

      // A browser with no local copy gets the rescued edit from the server.
      const freshCtx = await browser.newContext();
      try {
        const fresh = await freshCtx.newPage();
        await signInDev(fresh, server.baseUrl, "alice");
        await fresh.goto(collabUrl);
        await expect(fresh.getByTestId("collab-status-pill")).toHaveText("Synced", {
          timeout: 10_000,
        });
        await expect
          .poll(() => fresh.evaluate(() => window.__reticula?.textAt(3, 2)), { timeout: 5_000 })
          .toBe("unsent-rescue-42");
      } finally {
        await freshCtx.close();
      }
    } finally {
      await aliceCtx.close();
    }
  });

  test("enforces 401 unauthenticated, 403 non-member, and single-use share links", async ({
    browser,
  }) => {
    if (!server) {
      throw new Error("server not started");
    }
    const aliceCtx = await browser.newContext();
    const bobCtx = await browser.newContext();
    try {
      const alice = await aliceCtx.newPage();
      const bob = await bobCtx.newPage();

      await signInDev(alice, server.baseUrl, "alice");
      const { docId, joinPath } = await createAndShareSheet(alice, "Private Sheet", true);

      // Unauthenticated request gets 401.
      const unauth = await fetch(`${server.baseUrl}/api/workbooks/${docId}`);
      expect(unauth.status).toBe(401);

      // Signed-in non-member (Bob) gets access-denied on direct ?collab=<id>.
      await signInDev(bob, server.baseUrl, "bob");
      await bob.goto(`${server.baseUrl}/app?collab=${encodeURIComponent(docId)}&hooks`);
      await expect(bob.getByTestId("collab-access-denied")).toBeVisible({ timeout: 10_000 });

      // Single-use share link lets Bob join once cleanly.
      await bob.goto(`${server.baseUrl}${joinPath}?hooks`);
      await expect(bob.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout: 10_000 });
      await editCell(bob, 0, 0, "A1", "single-use-ok");
      await expect
        .poll(() => alice.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 5_000 })
        .toBe("single-use-ok");

      // Another user (Alice) attempting the already-consumed single-use link is rejected.
      await alice.goto(`${server.baseUrl}${joinPath}?hooks`);
      await expect(alice.getByTestId("collab-join-error")).toBeVisible({ timeout: 10_000 });
    } finally {
      await aliceCtx.close();
      await bobCtx.close();
    }
  });
});
