import { readdir, readFile } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import { extname, join, resolve } from "node:path";
import { expect, type Page, test } from "@playwright/test";
import { listen } from "./listen.ts";
import {
  canKeepTables,
  filledCell,
  goTo,
  importTable,
  ordersCsv,
  selectedCell,
  view,
} from "./tables-helpers.ts";

// Data tables without a connection, and updates of the app while one is imported.

const DIST_DIR = resolve(process.cwd(), process.env.RETICULA_E2E_DIST ?? "apps/web/dist");

const TYPES: Readonly<Record<string, string>> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
};

const isOfTables = (name: string) => /^(?:tables|table-worker|reticula_table_wasm_bg)-/.test(name);

/**
 * Serves the build, and as a later one when it is asked to: what shows tables has
 * other names then. Its service worker is held back until it may be had.
 */
async function serveBuild(workerMayBeHad: Promise<void> = Promise.resolve()) {
  const worker = await readFile(join(DIST_DIR, "sw.js"), "utf8");
  const [, buildId = ""] = /["'`]([0-9a-f]{16})["'`]/.exec(worker) ?? [];
  const names = (await readdir(join(DIST_DIR, "assets"))).filter(isOfTables);
  const later = (name: string) => name.replace(".", "later.");
  let isLater = false;
  const server: Server = createServer(async (request, response) => {
    const { pathname } = new URL(request.url ?? "/", "http://localhost");
    if (pathname === "/sw.js") {
      await workerMayBeHad;
    }
    const isKnown = pathname === "/sw.js" || pathname.startsWith("/assets/");
    let path = isKnown ? pathname.slice(1) : "index.html";
    if (isLater) {
      path = names.map((name) => `assets/${name}`).find((name) => later(name) === path) ?? path;
    }
    try {
      let data: string | Buffer = await readFile(join(DIST_DIR, path));
      if (isLater && path.endsWith(".js")) {
        data = names.reduce(
          (text, name) => text.replaceAll(name, later(name)),
          data.toString().replaceAll(buildId, "0123456789abcdef"),
        );
      }
      response
        .writeHead(200, {
          "content-type": TYPES[extname(path)] ?? "application/octet-stream",
          "cache-control": "no-cache",
        })
        .end(data);
    } catch {
      response.writeHead(404).end("not found");
    }
  });
  const port = await listen(server);
  return {
    origin: `http://localhost:${port}`,
    update: () => {
      isLater = true;
    },
    stop: async () => {
      server.closeAllConnections();
      await new Promise<void>((closed) => server.close(() => closed()));
    },
  };
}

async function open(page: Page, origin: string, doc = "offline-doc"): Promise<void> {
  await page.goto(`${origin}/app?hooks&doc=${doc}`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
  await controlled(page);
}

async function controlled(page: Page): Promise<void> {
  await page.waitForFunction(
    async () => (await window.__reticula?.swStatus())?.controlled === true,
  );
}

/** How many files there are that tables are shown with, and whether they are those of the later build. */
function kept(page: Page): Promise<{ readonly files: number; readonly later: number }> {
  return page.evaluate(async () => {
    const cache = await caches.open("reticula-optional");
    const names = (await cache.keys()).map(({ url }) => url);
    return { files: names.length, later: names.filter((name) => /later\./.test(name)).length };
  });
}

let build: Awaited<ReturnType<typeof serveBuild>>;

test.beforeEach(async ({ page }) => {
  build = await serveBuild();
  await open(page, build.origin);
  test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
});

test.afterEach(async () => {
  await build.stop();
});

test("a table that was open before is there without a connection", async ({ page }) => {
  await importTable(page, "orders", ordersCsv(1_000));
  await goTo(page, "amount 1000");
  const before = await filledCell(page);
  // What a table needs was kept when it was first used, by a worker that was there by then.
  await page.reload();
  await expect(view(page)).toHaveAttribute("data-status", "open");
  await build.stop();
  await page.reload();
  await expect(view(page)).toHaveAttribute("data-status", "open", { timeout: 15_000 });
  await expect.poll(() => selectedCell(page)).toEqual(before);
  await expect(page.getByTestId("table-count")).toHaveText("1,000 rows");
});

test("a table is there without a connection after the first visit", async ({ page }) => {
  const { promise, resolve: release } = Promise.withResolvers<void>();
  const first = await serveBuild(promise);
  try {
    await page.goto(`${first.origin}/app?hooks&doc=offline-doc`);
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
    await importTable(page, "orders", ordersCsv(1_000));
    // All of it was loaded before there was anything to keep it.
    expect((await kept(page)).files).toBe(0);
    release();
    await controlled(page);
    await expect.poll(async () => (await kept(page)).files).toBeGreaterThanOrEqual(3);
  } finally {
    release();
    await first.stop();
  }
  await page.reload();
  await expect(view(page)).toHaveAttribute("data-status", "open", { timeout: 15_000 });
  await expect(page.getByTestId("table-count")).toHaveText("1,000 rows");
});

test("what tables are shown with is kept as a whole, whichever part of it is seen", async ({
  page,
}) => {
  // A page that has its worker from the start does not tell it what was loaded before.
  await page.reload();
  await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
  await controlled(page);
  expect((await kept(page)).files).toBe(0);
  const names = (await readdir(join(DIST_DIR, "assets"))).filter(isOfTables);
  const name = names.find((one) => one.startsWith("table-worker-") && one.endsWith(".js"));
  await page.evaluate((path) => fetch(path).then((answer) => answer.text()), `/assets/${name}`);
  await expect.poll(async () => (await kept(page)).files).toBeGreaterThanOrEqual(3);
});

test("a table is there without a connection after an update", async ({ page }) => {
  await importTable(page, "orders", ordersCsv(1_000));
  await expect.poll(async () => (await kept(page)).files).toBeGreaterThanOrEqual(3);
  build.update();
  await page.evaluate(() => window.__reticula?.checkSwUpdate());
  const reloaded = page.waitForEvent("load");
  await page.getByTestId("sw-update-btn").click();
  await build.stop();
  await reloaded;
  await expect(view(page)).toHaveAttribute("data-status", "open", { timeout: 15_000 });
  await expect(page.getByTestId("table-count")).toHaveText("1,000 rows");
  const { files, later } = await kept(page);
  expect(later).toBe(files);
});

test("an update in another tab is not made while a table is made, unless that may be given up", async ({
  page,
  context,
}) => {
  await page.goto(`${build.origin}/app?hooks&doc=offline-doc&sample=90000000`);
  await expect(page.getByTestId("table-importing")).toBeVisible({ timeout: 20_000 });
  const other = await context.newPage();
  await open(other, build.origin, "other-doc");
  build.update();
  await other.evaluate(() => window.__reticula?.checkSwUpdate());
  await other.getByTestId("sw-update-btn").click();
  const dialog = other.getByTestId("table-give-up");
  await expect(dialog).toContainText("the import is given up");
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(other.getByTestId("sw-update-btn")).toBeEnabled();
  await expect(page.getByTestId("table-importing")).toBeVisible();
  await other.getByTestId("sw-update-btn").click();
  const reloaded = Promise.all([page.waitForEvent("load"), other.waitForEvent("load")]);
  await other.getByTestId("table-give-up-confirm").click();
  await reloaded;
  await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
  await expect(page.getByTestId("table-importing")).toBeHidden();
});

test("an update is not made while a table is made, unless that may be given up", async ({
  page,
}) => {
  // One that takes longer than this test.
  await page.goto(`${build.origin}/app?hooks&doc=offline-doc&sample=90000000`);
  await expect(page.getByTestId("table-importing")).toBeVisible({ timeout: 20_000 });
  build.update();
  await page.evaluate(() => window.__reticula?.checkSwUpdate());
  await page.getByTestId("sw-update-btn").click();
  const dialog = page.getByTestId("table-give-up");
  await expect(dialog).toContainText("the import is given up");
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(page.getByTestId("table-importing")).toBeVisible();
  await expect(page.getByTestId("sw-update-btn")).toBeEnabled();
  await page.getByTestId("sw-update-btn").click();
  const reloaded = page.waitForEvent("load");
  await page.getByTestId("table-give-up-confirm").click();
  await reloaded;
  await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
  await expect(page.getByTestId("table-importing")).toBeHidden();
  await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
});
