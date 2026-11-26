import { readFile } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import { extname, join, resolve } from "node:path";
import { expect, type Page, test } from "@playwright/test";
import { fillFormulaBar } from "./formula-bar-helpers.ts";
import { listen } from "./listen.ts";

const DIST_DIR = resolve(process.cwd(), process.env.RETICULA_E2E_DIST ?? "apps/web/dist");

const MIME_TYPES: Readonly<Record<string, string>> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".txt": "text/plain; charset=utf-8",
};

interface StaticTestServer {
  readonly origin: string;
  setSwOverride(overrideBuildId: string | null): void;
  stop(): Promise<void>;
}

async function startDistServer(): Promise<StaticTestServer> {
  let swOverrideId: string | null = null;
  const rawSwText = await readFile(join(DIST_DIR, "sw.js"), "utf8");
  const idMatch = rawSwText.match(/["'`]([0-9a-f]{16})["'`]/);
  const originalBuildId = idMatch?.[1] ?? null;
  if (!originalBuildId) {
    throw new Error("Could not locate 16-hex BUILD_ID in apps/web/dist/sw.js");
  }

  const server: Server = createServer(async (req, res) => {
    const pathname = new URL(req.url ?? "/", "http://localhost").pathname;
    if (pathname === "/sw.js") {
      const body = swOverrideId ? rawSwText.replaceAll(originalBuildId, swOverrideId) : rawSwText;
      res
        .writeHead(200, {
          "content-type": "text/javascript; charset=utf-8",
          "cache-control": "no-cache",
        })
        .end(body);
      return;
    }

    const isAsset = pathname.startsWith("/assets/");
    const relPath = isAsset ? pathname.slice(1) : "index.html";
    try {
      const data = await readFile(join(DIST_DIR, relPath));
      const mime = MIME_TYPES[extname(relPath)] ?? "application/octet-stream";
      res
        .writeHead(200, {
          "content-type": mime,
          "cache-control": isAsset ? "public, max-age=31536000, immutable" : "no-cache",
        })
        .end(data);
    } catch {
      res.writeHead(404).end("not found");
    }
  });

  const port = await listen(server);

  return {
    origin: `http://localhost:${port}`,
    setSwOverride: (overrideBuildId) => {
      swOverrideId = overrideBuildId;
    },
    stop: async () => {
      server.closeAllConnections();
      await new Promise<void>((resolveClosed) => server.close(() => resolveClosed()));
    },
  };
}

async function settled(page: Page): Promise<void> {
  await page.waitForFunction(() => {
    const stats = window.__reticula?.stats();
    return stats !== undefined && stats.pagesHeld > 0 && stats.missingCells === 0;
  });
}

async function editCellViaBar(
  page: Page,
  row: number,
  col: number,
  address: string,
  raw: string,
  expectedText: string,
): Promise<void> {
  await settled(page);
  const beforeSeq = await page.evaluate(() => Number(window.__reticula?.docStatus().walSeq ?? "0"));
  await page.evaluate(
    ([r, c]) => {
      window.__reticula?.select(r, c);
    },
    [row, col] as const,
  );
  await expect(page.getByTestId("name-box")).toHaveText(address);
  await fillFormulaBar(page, raw);
  await page.waitForFunction(
    (seq) => Number(window.__reticula?.docStatus().walSeq ?? "0") > seq,
    beforeSeq,
  );
  await expect
    .poll(() => page.evaluate(([r, c]) => window.__reticula?.textAt(r, c), [row, col] as const))
    .toBe(expectedText);
  await settled(page);
}

async function waitForSwControlled(page: Page): Promise<string> {
  let buildId: string | null = null;
  await expect
    .poll(
      async () => {
        const status = await page.evaluate(() => window.__reticula?.swStatus());
        if (status?.controlled && status.buildId) {
          buildId = status.buildId;
          return true;
        }
        return false;
      },
      { timeout: 15_000 },
    )
    .toBe(true);
  if (!buildId) {
    throw new Error("Service Worker became controlled without reporting a buildId");
  }
  return buildId;
}

test("offline reload serves precached shell and continues editing OPFS workbooks with server stopped", async ({
  page,
}) => {
  const srv = await startDistServer();
  let stopped = false;
  try {
    await page.goto(`${srv.origin}/app?hooks&doc=offline-e2e`);
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await settled(page);

    const backend = await page.evaluate(() => window.__reticula?.docStatus().storageBackend);
    test.skip(
      backend === "memory",
      "Browser lacks persistent OPFS in this environment (e.g. WinCairo WebKit)",
    );

    const buildId = await waitForSwControlled(page);
    expect(buildId).toMatch(/^[0-9a-f]{16}$/);

    await editCellViaBar(page, 0, 0, "A1", "BeforeOffline", "BeforeOffline");
    await editCellViaBar(page, 0, 1, "B1", "=10+20", "30");
    await page.waitForTimeout(80);

    // Completely shut down the origin HTTP server to prove zero network dependence!
    await srv.stop();
    stopped = true;

    // Reload while server is completely offline -> SW precached shell + OPFS restore seamlessly.
    await page.reload();
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 15_000 });
    await settled(page);
    await expect
      .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0)))
      .toBe("BeforeOffline");
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(0, 1))).toBe("30");

    // Continue editing and calculating formulas while offline, then reload a second time offline!
    await editCellViaBar(page, 1, 0, "A2", "EditedOffline", "EditedOffline");
    await editCellViaBar(page, 1, 1, "B2", "=B1*2", "60");
    await page.waitForTimeout(80);

    await page.reload();
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 15_000 });
    await settled(page);
    await expect
      .poll(() => page.evaluate(() => window.__reticula?.textAt(0, 0)))
      .toBe("BeforeOffline");
    await expect
      .poll(() => page.evaluate(() => window.__reticula?.textAt(1, 0)))
      .toBe("EditedOffline");
    await expect.poll(() => page.evaluate(() => window.__reticula?.textAt(1, 1))).toBe("60");
  } finally {
    if (!stopped) {
      await srv.stop();
    }
  }
});

test("two open tabs coordinate OPFS drain and upgrade together from SW v1 to v2 without data loss", async ({
  context,
}) => {
  const srv = await startDistServer();
  const V2_BUILD_ID = "e2ev2build000002";
  try {
    const tab1 = await context.newPage();
    await tab1.goto(`${srv.origin}/app?hooks&doc=sw-upgrade-1`);
    await expect(tab1.getByTestId("engine-status")).toHaveText("ready");
    await settled(tab1);

    const backend = await tab1.evaluate(() => window.__reticula?.docStatus().storageBackend);
    test.skip(
      backend === "memory",
      "Browser lacks persistent OPFS in this environment (e.g. WinCairo WebKit)",
    );

    const v1BuildId = await waitForSwControlled(tab1);
    await editCellViaBar(tab1, 0, 0, "A1", "TabOneV1Data", "TabOneV1Data");

    const tab2 = await context.newPage();
    await tab2.goto(`${srv.origin}/app?hooks&doc=sw-upgrade-2`);
    await expect(tab2.getByTestId("engine-status")).toHaveText("ready");
    await settled(tab2);
    expect(await waitForSwControlled(tab2)).toBe(v1BuildId);
    await editCellViaBar(tab2, 0, 0, "A1", "TabTwoV1Data", "TabTwoV1Data");
    await tab2.waitForTimeout(80);

    // Something is being typed in each tab, and not entered yet: in the formula
    // bar of one, and in a cell of the other.
    await tab2.evaluate(() => window.__reticula?.select(0, 1));
    await expect(tab2.getByTestId("name-box")).toHaveText("B1");
    const bar2 = tab2.getByTestId("formula-input");
    await bar2.click();
    await bar2.fill('=A1&" half"');
    await expect(bar2).toHaveText('=A1&" half"');
    await tab1.bringToFront();
    await tab1.evaluate(() => window.__reticula?.select(0, 1));
    await expect(tab1.getByTestId("name-box")).toHaveText("B1");
    await tab1.keyboard.type("In cell");
    const cell1 = tab1.locator(".reticula-editor .cm-content");
    await expect(cell1).toHaveText("In cell");

    // Publish v2 Service Worker while both tabs remain open and editing.
    srv.setSwOverride(V2_BUILD_ID);
    await tab1.bringToFront();
    await tab1.evaluate(() => window.__reticula?.checkSwUpdate());

    // Both tabs show the gentle update banner and STAY on v1 until the user confirms.
    await expect(tab1.getByTestId("sw-update-banner")).toBeVisible({ timeout: 10_000 });
    await expect(tab2.getByTestId("sw-update-banner")).toBeVisible({ timeout: 10_000 });
    expect((await tab1.evaluate(() => window.__reticula?.swStatus()))?.buildId).toBe(v1BuildId);
    expect((await tab2.evaluate(() => window.__reticula?.swStatus()))?.buildId).toBe(v1BuildId);

    // Confirm update in Tab 1 -> both tabs flush/close OPFS, activate v2, and reload cleanly!
    const tab1Nav = tab1.waitForEvent("load", { timeout: 15_000 });
    const tab2Nav = tab2.waitForEvent("load", { timeout: 15_000 });
    await tab1.getByTestId("sw-update-btn").click();
    await Promise.all([tab1Nav, tab2Nav]);

    await tab1.bringToFront();
    await expect(tab1.getByTestId("engine-status")).toHaveText("ready", { timeout: 15_000 });
    await settled(tab1);
    await expect
      .poll(async () => (await tab1.evaluate(() => window.__reticula?.swStatus()))?.buildId, {
        timeout: 10_000,
      })
      .toBe(V2_BUILD_ID);
    await expect
      .poll(() => tab1.evaluate(() => window.__reticula?.textAt(0, 0)))
      .toBe("TabOneV1Data");
    // What was being typed is there again, in the cell, and not entered.
    await expect(cell1).toHaveText("In cell", { timeout: 10_000 });
    await expect(tab1.getByTestId("name-box")).toHaveText("B1");
    expect(await tab1.evaluate(() => window.__reticula?.textAt(0, 1))).toBe("");
    await tab1.keyboard.press("Enter");
    await expect.poll(() => tab1.evaluate(() => window.__reticula?.textAt(0, 1))).toBe("In cell");

    await tab2.bringToFront();
    await expect(tab2.getByTestId("engine-status")).toHaveText("ready", { timeout: 15_000 });
    await settled(tab2);
    await expect
      .poll(async () => (await tab2.evaluate(() => window.__reticula?.swStatus()))?.buildId, {
        timeout: 10_000,
      })
      .toBe(V2_BUILD_ID);
    await expect
      .poll(() => tab2.evaluate(() => window.__reticula?.textAt(0, 0)))
      .toBe("TabTwoV1Data");
    // And in the formula bar of the other, with the focus.
    await expect(bar2).toHaveText('=A1&" half"', { timeout: 10_000 });
    await expect(tab2.locator(".formula-cm-wrap.focused")).toBeVisible();
    await expect(tab2.getByTestId("name-box")).toHaveText("B1");
    expect(await tab2.evaluate(() => window.__reticula?.textAt(0, 1))).toBe("");
    await tab2.keyboard.press("Escape");
    await expect.poll(() => tab2.evaluate(() => window.__reticula?.textAt(0, 1))).toBe("");
    await expect(bar2).toHaveText("");

    await tab1.close();
    await tab2.close();
  } finally {
    await srv.stop();
  }
});
