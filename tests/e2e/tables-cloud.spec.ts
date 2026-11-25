import { randomUUID } from "node:crypto";
import { type Browser, type BrowserContext, expect, type Page, test } from "@playwright/test";
import { type DevAccount, editCell, settled } from "./collab-helpers.ts";
import {
  canKeepTables,
  columnMenu,
  content,
  dropFiles,
  goTo,
  importTable,
  openPalette,
  ordersCsv,
  runCommand,
  selectedCell,
  tab,
  tableGrid,
  view,
} from "./tables-helpers.ts";
import {
  api,
  copies,
  enter,
  putTable,
  queue,
  type RemoteServer,
  startRemoteServer,
} from "./tables-remote-helpers.ts";

// Data tables of workbooks on the server, as they are worked with: who sees them,
// who may change them, what is said of uploads, and what others do meanwhile.

test.describe.configure({ mode: "serial" });
test.setTimeout(120_000);

// Whose tables others upload at the same time has to wait: these are nobody else's.
const OWNER: DevAccount = "bob";
const MEMBER: DevAccount = "alice";

let server: RemoteServer;
/** The workbooks there are now. */
const made: string[] = [];

test.beforeAll(async () => {
  server = await startRemoteServer();
});

test.beforeEach(async ({ page }) => {
  server.mishap = null;
  await enter(page, server, OWNER);
  test.skip(!(await canKeepTables(page)), "tables cannot be kept here");
});

test.afterAll(async ({ browser }) => {
  // What its owner may store is for other tests as well.
  server.mishap = null;
  const page = await browser.newPage();
  await enter(page, server, OWNER);
  for (const id of made) {
    await api(page, server, "DELETE", `/workbooks/${id}`);
  }
  await page.close();
  await server.stop();
});

/** A workbook on the server that belongs to whoever is signed in. */
async function create(page: Page, title: string): Promise<string> {
  const { id } = await api<{ id: string }>(page, server, "POST", "/workbooks", { title });
  made.push(id);
  return id;
}

async function open(page: Page, docId: string): Promise<void> {
  await page.goto(`${server.baseUrl}/app?hooks&collab=${docId}`);
  await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout: 15_000 });
  await settled(page);
}

/** Somebody else who takes part in a workbook, and has it open. */
async function withMember(
  browser: Browser,
  owner: Page,
  docId: string,
  role: "viewer" | "editor",
  run: (page: Page, userId: string, context: BrowserContext) => Promise<void>,
): Promise<void> {
  const { url } = await api<{ url: string }>(
    owner,
    server,
    "POST",
    `/workbooks/${docId}/share-links`,
    { role },
  );
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const userId = await enter(page, server, MEMBER);
    await api(page, server, "POST", `/join/${/[^/=]+$/.exec(url)?.[0]}`);
    await api(owner, server, "PATCH", `/workbooks/${docId}/members/${userId}`, { role });
    await open(page, docId);
    await run(page, userId, context);
  } finally {
    await context.close();
  }
}

/** Has the page look at the tables there are, as it does when somebody comes back to it. */
function lookAgain(page: Page): Promise<boolean> {
  return page.evaluate(() => document.dispatchEvent(new Event("visibilitychange")));
}

async function statuses(page: Page, docId: string): Promise<string[]> {
  const { tables } = await api<{ tables: { status: string }[] }>(
    page,
    server,
    "GET",
    `/workbooks/${docId}/tables`,
  );
  return tables.map(({ status }) => status);
}

const showTable = async (page: Page, name: string) => {
  await tab(page, name).click();
  await expect(view(page)).toHaveAttribute("data-status", "open");
};

test("what others do to the worksheets is nothing to a table in view, and they see nothing of it", async ({
  browser,
  page,
}) => {
  const docId = await create(page, "Table in view");
  await putTable(page, docId, 20_000);
  await open(page, docId);
  await editCell(page, 1, 1, "B2", "before");
  await withMember(browser, page, docId, "editor", async (other) => {
    const presence = () =>
      other.evaluate(() =>
        window.__reticula
          ?.collabStatus()
          .remotePresences.map(({ cursorRow, cursorCol }) => `${cursorRow} ${cursorCol}`),
      );
    await page.evaluate(() => window.__reticula?.select(3, 2));
    await expect.poll(presence).toEqual(["3 2"]);

    await showTable(page, "orders");
    await expect(page.getByTestId("table-here")).toBeHidden({ timeout: 30_000 });
    await page.keyboard.press("ControlOrMeta+End");
    for (let step = 0; step < 5; step++) {
      await page.keyboard.press("ArrowUp");
    }
    await page.keyboard.press("ArrowLeft");
    await expect.poll(async () => (await selectedCell(page)).row).toBe("19995");
    const cell = await selectedCell(page);
    const before = await content(page);

    await editCell(other, 0, 0, "A1", "from another");
    await expect.poll(async () => (await content(page)).digest).not.toBe(before.digest);
    await other.getByTestId("add-sheet-btn").click();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1", "Sheet2", "orders"]);
    await settled(other);
    await editCell(other, 4, 4, "E5", "on the other sheet");
    // The worksheet that the table took the place of goes.
    await tab(other, "Sheet1").click();
    await runCommand(other, "deleteSheet");
    await other.getByTestId("delete-sheet-confirm").click();
    await expect(page.getByRole("tab")).toHaveText(["Sheet2", "orders"]);

    await expect(view(page)).toHaveAttribute("data-status", "open");
    await expect(tab(page, "orders")).toHaveAttribute("aria-selected", "true");
    await expect(tableGrid(page)).toBeFocused();
    expect(await selectedCell(page)).toEqual(cell);
    await page.keyboard.press("ArrowUp");
    await expect.poll(async () => (await selectedCell(page)).row).toBe("19994");
    // Where somebody is in a table is told to nobody.
    const seen = (await presence()) ?? [];
    expect(seen.filter((place) => place !== "3 2")).toEqual([]);

    await tab(page, "Sheet2").click();
    await expect
      .poll(() => page.evaluate(() => window.__reticula?.textAt(4, 4)))
      .toBe("on the other sheet");
  });
});

test("a table is imported, uploaded, and read by those who may not change it", async ({
  browser,
  page,
}) => {
  const docId = await create(page, "Imported");
  await open(page, docId);
  const before = await content(page);
  await importTable(page, "sales", ordersCsv(3_000));
  await expect(page.getByTestId("table-count")).toHaveText("3,000 rows");
  await expect.poll(() => statuses(page, docId), { timeout: 30_000 }).toEqual(["ready"]);
  await expect(page.getByTestId("table-upload")).toBeHidden();
  expect(await queue(page)).toEqual([]);
  // It is still the same one, and nothing of it is in the workbook.
  await expect(view(page)).toHaveAttribute("data-status", "open");
  await goTo(page, "amount 1");
  await columnMenu(page, "tableSortDescending");
  await expect.poll(async () => (await selectedCell(page)).text).toBe("999.04");
  expect(await content(page)).toEqual(before);

  await withMember(browser, page, docId, "viewer", async (other) => {
    await showTable(other, "sales");
    await expect(other.getByTestId("table-count")).toHaveText("3,000 rows");
    await goTo(other, "amount 1");
    await columnMenu(other, "tableSortDescending");
    await expect.poll(async () => (await selectedCell(other)).text).toBe("999.04");
    expect(await copies(other)).toEqual([]);
    await openPalette(other);
    for (const command of [
      "importTable",
      "tableRename",
      "tableDelete",
      "tableMoveLeft",
      "tableMoveRight",
    ]) {
      await expect(other.getByTestId(`command-${command}`)).toHaveAttribute(
        "aria-disabled",
        "true",
      );
    }
    await expect(other.getByTestId("command-tableExport")).not.toHaveAttribute(
      "aria-disabled",
      "true",
    );
    await other.keyboard.press("Escape");
    await dropFiles(tableGrid(other), [{ name: "a.csv", text: "a\n1\n" }]);
    await expect(other.getByTestId("rejected-banner")).toContainText("read-only");

    // What the owner does to it is there for others, and closes it in the end.
    await runCommand(page, "tableRename");
    await page.getByTestId("table-rename-input").fill("sold");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("table-rename")).toBeHidden();
    await expect(tab(page, "sold")).toBeVisible();
    await lookAgain(other);
    await expect(tab(other, "sold")).toHaveAttribute("aria-selected", "true");
    await runCommand(page, "tableDelete");
    await page.getByTestId("table-delete-confirm").click();
    await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
    // It is gone here before it is there.
    await expect.poll(() => statuses(page, docId)).toEqual([]);
    await lookAgain(other);
    await expect(other.getByTestId("table-problem")).toContainText("is not there any more");
    await expect(other.getByRole("tab")).toHaveText(["Sheet1"]);
    await tab(other, "Sheet1").click();
    await expect(view(other)).toBeHidden();
  });
});

/** Opens a workbook again, where a table may be in view. */
async function reopen(page: Page, docId: string): Promise<void> {
  await page.goto(`${server.baseUrl}/app?hooks&collab=${docId}`);
  await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout: 15_000 });
}

/** Deletes the tables of a workbook, which may be uploaded somewhere. */
async function giveUpAll(page: Page, docId: string): Promise<void> {
  const url = `/workbooks/${docId}/tables`;
  const { tables } = await api<{ tables: { id: string }[] }>(page, server, "GET", url);
  for (const { id } of tables) {
    await api(page, server, "DELETE", `${url}/${id}`);
  }
}

/** Has the server take no bytes of tables for now. */
function holdUploads(): void {
  server.mishap = ({ method }) =>
    method === "PATCH" ? { status: 503, headers: { "Retry-After": "2" } } : undefined;
}

/** The tables there are copies of, or lists, and who they are of. */
function kept(page: Page): Promise<string[]> {
  return page.evaluate(async () => {
    const found: string[] = [];
    try {
      const opfs = await navigator.storage.getDirectory();
      const root = await opfs.getDirectoryHandle("reticula-v1");
      const all = (await root.getDirectoryHandle("tables-remote")) as unknown as AsyncIterable<
        [string, AsyncIterable<[string, unknown]>]
      >;
      for await (const [user, dir] of all) {
        for await (const [name] of dir) {
          found.push(`${user} ${name}`);
        }
      }
    } catch {
      // There are none.
    }
    return found;
  });
}

const signOut = async (page: Page) => {
  await page.goto(`${server.baseUrl}/account`);
  await page.getByRole("button", { name: "Sign Out" }).click();
};

test("a table is all there after its upload, without being had from the server", async ({
  page,
}) => {
  const docId = await create(page, "Uploaded");
  await open(page, docId);
  // More than is read at once of a file that is picked.
  await importTable(page, "sales", ordersCsv(150_000));
  await expect.poll(() => statuses(page, docId), { timeout: 90_000 }).toEqual(["ready"]);
  await expect.poll(() => queue(page), { timeout: 30_000 }).toEqual([]);
  server.bytes = 0;
  await goTo(page, "amount 70000");
  await expect.poll(async () => (await selectedCell(page)).text).toBe("300.00");
  await columnMenu(page, "tableSortDescending");
  await expect
    .poll(async () => (await selectedCell(page)).text, { timeout: 20_000 })
    .toBe("999.99");
  await expect(view(page)).toHaveAttribute("data-status", "open");
  expect(await copies(page)).toHaveLength(1);
  expect(server.bytes).toBe(0);

  // The same when its file is already known by then, and its page is another one.
  holdUploads();
  await importTable(page, "more", ordersCsv(120_000));
  await expect(page.getByTestId("table-upload")).toContainText("Waiting", { timeout: 30_000 });
  await reopen(page, docId);
  await showTable(page, "more");
  server.mishap = null;
  await expect.poll(() => statuses(page, docId), { timeout: 90_000 }).toEqual(["ready", "ready"]);
  await expect.poll(() => queue(page), { timeout: 30_000 }).toEqual([]);
  server.bytes = 0;
  await goTo(page, "amount 70000");
  await expect.poll(async () => (await selectedCell(page)).text).toBe("300.00");
  expect(server.bytes).toBe(0);
});

test("a table that is deleted soon after its import is gone, and keeps nothing else from being uploaded", async ({
  page,
}) => {
  const docId = await create(page, "Deleted soon");
  await open(page, docId);
  holdUploads();
  await importTable(page, "wrong", ordersCsv(3_000));
  await expect.poll(() => statuses(page, docId)).toEqual(["uploading"]);
  await runCommand(page, "tableDelete");
  await page.getByTestId("table-delete-confirm").click();
  await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
  await expect.poll(() => statuses(page, docId)).toEqual([]);
  expect(await queue(page)).toEqual([]);
  server.mishap = null;
  await importTable(page, "right", ordersCsv(500));
  await expect.poll(() => statuses(page, docId), { timeout: 20_000 }).toEqual(["ready"]);
  await expect(page.getByRole("tab")).toHaveText(["Sheet1", "right"]);
  // While it is completed, or whatever else it is at.
  await importTable(page, "at once", ordersCsv(3_000));
  await runCommand(page, "tableDelete");
  await page.getByTestId("table-delete-confirm").click();
  await expect(page.getByRole("tab")).toHaveText(["Sheet1", "right"]);
  await page.waitForTimeout(3_000);
  await lookAgain(page);
  await expect.poll(() => statuses(page, docId)).toEqual(["ready"]);
  expect(await queue(page)).toEqual([]);
});

test("a table that somebody else deletes while it is uploaded does not come back by itself", async ({
  page,
}) => {
  const docId = await create(page, "Unwanted");
  await open(page, docId);
  holdUploads();
  await importTable(page, "unwanted", ordersCsv(3_000));
  await expect.poll(() => statuses(page, docId)).toEqual(["uploading"]);
  const { tables } = await api<{ tables: { id: string }[] }>(
    page,
    server,
    "GET",
    `/workbooks/${docId}/tables`,
  );
  await api(page, server, "DELETE", `/workbooks/${docId}/tables/${tables[0]?.id}`);
  server.mishap = null;
  const upload = page.getByTestId("table-upload");
  await expect(upload).toContainText("unwanted was not uploaded.", { timeout: 20_000 });
  expect(await statuses(page, docId)).toEqual([]);
  await reopen(page, docId);
  await expect(upload).toContainText("unwanted was not uploaded.", { timeout: 20_000 });
  expect(await statuses(page, docId)).toEqual([]);
  // Unless that is asked for.
  await page.getByTestId("table-upload-again").click();
  await expect.poll(() => statuses(page, docId), { timeout: 20_000 }).toEqual(["ready"]);
});

test("a table that is not uploaded is exported and renamed all the same", async ({ page }) => {
  const docId = await create(page, "Not uploaded");
  await open(page, docId);
  server.mishap = ({ method, path }) =>
    method === "POST" && path.endsWith("/tables") ? { status: 507 } : undefined;
  await importTable(page, "here", ordersCsv(3_000));
  await expect(page.getByTestId("table-upload")).toContainText("here was not uploaded.");
  const download = page.waitForEvent("download");
  await runCommand(page, "tableExport");
  expect((await download).suggestedFilename()).toContain("here");
  await runCommand(page, "tableRename");
  await page.getByTestId("table-rename-input").fill("other");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("table-rename")).toBeHidden();
  await expect(page.getByRole("tab")).toHaveText(["Sheet1", "other"]);
  server.mishap = null;
  await page.getByTestId("table-upload-again").click();
  await expect.poll(() => statuses(page, docId)).toEqual(["ready"]);
  await lookAgain(page);
  await expect(page.getByRole("tab")).toHaveText(["Sheet1", "other"]);
});

test("a connection that ends while a table is exported, or is not there once, is got along with", async ({
  page,
}) => {
  const docId = await create(page, "Connections");
  await putTable(page, docId, 200_000);
  await open(page, docId);
  let refusals = 0;
  server.mishap = ({ method, path }) =>
    method === "GET" && path.endsWith("/data") && refusals++ === 0 ? { status: 503 } : undefined;
  await showTable(page, "orders");
  let drops = 0;
  server.mishap = ({ method, path, range }) =>
    method === "GET" && path.endsWith("/data") && !range && drops++ === 0
      ? { dropAfter: 100_000 }
      : undefined;
  await runCommand(page, "tableExport");
  await expect(page.getByTestId("rejected-banner")).toBeVisible();
  await expect(view(page)).toHaveAttribute("data-status", "open");
  const download = page.waitForEvent("download");
  await runCommand(page, "tableExport");
  await download;
});

test("what may not be kept any more is gone, also when that happened with the page closed", async ({
  browser,
  page,
}) => {
  const docId = await create(page, "Downgraded");
  await putTable(page, docId, 5_000);
  await withMember(browser, page, docId, "editor", async (other, userId) => {
    const leave = () => other.goto(`${server.baseUrl}/privacy`);
    const role = (to: string) =>
      api(page, server, "PATCH", `/workbooks/${docId}/members/${userId}`, { role: to });
    const keep = async () => {
      await showTable(other, "orders");
      await expect.poll(() => kept(other)).toHaveLength(2);
      await leave();
    };
    await keep();
    await role("viewer");
    await reopen(other, docId);
    await expect.poll(() => kept(other)).toEqual([]);
    await showTable(other, "orders");
    expect(await kept(other)).toEqual([]);

    await leave();
    await role("editor");
    await reopen(other, docId);
    await keep();
    await api(page, server, "DELETE", `/workbooks/${docId}/members/${userId}`);
    await other.goto(`${server.baseUrl}/app?hooks&collab=${docId}`);
    await expect.poll(() => kept(other)).toEqual([]);
  });
});

test("who signs out, in whatever tab, uploads nothing more, and leaves no copies to who comes next", async ({
  browser,
  page,
}) => {
  const docId = await create(page, "Accounts");
  await putTable(page, docId, 5_000);
  // Who may change the tables as well.
  await withMember(browser, page, docId, "editor", async () => {});
  await open(page, docId);
  await showTable(page, "orders");
  await expect.poll(() => copies(page)).toHaveLength(1);
  holdUploads();
  await importTable(page, "sales", ordersCsv(20_000));
  await expect.poll(() => statuses(page, docId)).toEqual(["ready", "uploading"]);
  await showTable(page, "orders");
  // A tab that nobody sees does not look at anything.
  await page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", { get: () => "hidden" });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  const second = await page.context().newPage();
  await signOut(second);
  await expect.poll(() => kept(second)).toEqual([]);
  await enter(second, server, MEMBER);
  server.mishap = null;
  server.passed.length = 0;
  await second.waitForTimeout(5_000);
  const sent = server.passed.filter(({ method, path }) => method !== "GET" && path.includes(docId));
  expect(sent).toEqual([]);
  expect(await kept(second)).toEqual([]);
  expect(await queue(second)).toHaveLength(2);
  // Whoever is back goes on.
  await enter(second, server, OWNER);
  await expect.poll(() => statuses(second, docId), { timeout: 30_000 }).toEqual(["ready", "ready"]);
  await second.close();
});

test("who signs out in a workbook of this browser uploads nothing more", async ({ page }) => {
  const docId = await create(page, "Signed out elsewhere");
  await open(page, docId);
  holdUploads();
  await importTable(page, "sales", ordersCsv(20_000));
  await expect.poll(() => statuses(page, docId)).toEqual(["uploading"]);
  await page.goto(`${server.baseUrl}/app?hooks&doc=local-${randomUUID()}`);
  const upload = page.getByTestId("table-upload");
  await expect(upload).toContainText("Waiting", { timeout: 20_000 });
  await page.getByTestId("cloud-menu-btn").click();
  await page.getByTestId("collab-logout-btn").click();
  await expect(upload).toBeHidden();
  server.passed.length = 0;
  await page.waitForTimeout(4_000);
  expect(server.passed.filter(({ path }) => path.includes("/tables"))).toEqual([]);
  expect(await queue(page)).toHaveLength(2);
  server.mishap = null;
  // What is being uploaded keeps its owner from uploading anything else.
  await enter(page, server, OWNER);
  await giveUpAll(page, docId);
});

test("what is to be uploaded is no workbook, whatever the address says", async ({ page }) => {
  const docId = await create(page, "Queue");
  await open(page, docId);
  server.mishap = ({ method, path }) =>
    method === "POST" && path.endsWith("/tables") ? { status: 507 } : undefined;
  await importTable(page, "precious", ordersCsv(500));
  await expect(page.getByTestId("table-upload")).toContainText("was not uploaded");
  const before = await queue(page);
  expect(before).toHaveLength(2);
  for (const name of ["upload-queue", "tables-queue"]) {
    await page.goto(`${server.baseUrl}/app?hooks&doc=${name}`);
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await page.waitForFunction(() => window.__reticula !== undefined);
    await page.waitForTimeout(1_000);
    await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
    await page.evaluate(async (id) => {
      const { deleteTables } = await (
        window.__reticula as NonNullable<Window["__reticula"]>
      ).tables();
      await deleteTables(id);
    }, name);
    expect(await queue(page)).toEqual(before);
  }
});

test("a workbook of this browser becomes one on the server with all its tables, or none at all", async ({
  page,
}) => {
  const local = `conversion-${randomUUID()}`;
  await page.goto(`${server.baseUrl}/app?hooks&doc=${local}`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await importTable(page, "one", ordersCsv(500));
  await importTable(page, "two", ordersCsv(700));
  const owned = async () => {
    const { owned: books } = await api<{ owned: { id: string }[] }>(
      page,
      server,
      "GET",
      "/workbooks",
    );
    return books.map(({ id }) => id);
  };
  const before = await owned();
  await page.goto(`${server.baseUrl}/workbooks`);
  await page.getByTestId("workbooks-tab-local").click();
  let isImported = false;
  server.mishap = ({ method, path }) => {
    isImported ||= method === "POST" && path.endsWith("/import");
    return path.endsWith("/auth/me") ? { status: 503 } : undefined;
  };
  await page.getByTestId(`convert-cloud-${local}`).click();
  await expect(page.getByTestId("workbooks-error")).toBeVisible();
  expect(isImported).toBe(false);
  expect(await owned()).toEqual(before);
  expect(await queue(page)).toEqual([]);
  server.mishap = null;
  await page.getByTestId(`convert-cloud-${local}`).click();
  await expect(page).toHaveURL(/collab=/);
  const made0 = (await owned()).filter((id) => !before.includes(id));
  expect(made0).toHaveLength(1);
  made.push(...made0);
  const [docId = ""] = made0;
  await expect.poll(() => statuses(page, docId), { timeout: 30_000 }).toEqual(["ready", "ready"]);
  await expect(page.getByRole("tab")).toHaveText(["Sheet1", "one", "two"]);
  // From where it was made, and not from the server.
  await showTable(page, "two");
  await expect(page.getByTestId("table-count")).toHaveText("700 rows");
});

test("what the server has no room for is said, tried again, and given up", async ({ page }) => {
  const docId = await create(page, "Full");
  await open(page, docId);
  const refuseAll = () => {
    server.mishap = ({ method, path }) =>
      method === "POST" && path.endsWith("/tables") ? { status: 507 } : undefined;
  };
  refuseAll();
  await importTable(page, "first", ordersCsv(500));
  const upload = page.getByTestId("table-upload");
  await expect(upload).toContainText(
    "first was not uploaded. The server has no room left for data tables.",
  );
  // It is there for whoever has imported it.
  await expect(view(page)).toHaveAttribute("data-status", "open");
  await goTo(page, "id 500");
  await expect.poll(async () => (await selectedCell(page)).text).toBe("500");
  server.mishap = null;
  await page.getByTestId("table-upload-again").click();
  await expect.poll(() => statuses(page, docId)).toEqual(["ready"]);
  await expect(upload).toBeHidden();

  refuseAll();
  await importTable(page, "second", ordersCsv(500));
  await expect(upload).toContainText("second was not uploaded.");
  await page.getByTestId("table-upload-cancel").click();
  await page.getByTestId("table-delete-confirm").click();
  await expect(page.getByRole("tab")).toHaveText(["Sheet1", "first"]);
  await expect(upload).toBeHidden();
  expect(await queue(page)).toEqual([]);
});

test("an upload that the page is left with goes on in the next one, and is told of there", async ({
  page,
}) => {
  const docId = await create(page, "Left");
  await open(page, docId);
  // It is not sent while the server has no time.
  server.mishap = ({ method }) => (method === "PATCH" ? { status: 503 } : undefined);
  await importTable(page, "orders", ordersCsv(2_000));
  const upload = page.getByTestId("table-upload");
  await expect(upload).toContainText("Waiting to upload orders: 0%");
  // A workbook in this browser, which has other tables.
  await page.goto(`${server.baseUrl}/app?hooks&doc=local-${randomUUID()}`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await expect(upload).toContainText("Waiting to upload orders", { timeout: 20_000 });
  await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
  await page.getByTestId("table-upload-cancel").click();
  const question = page.getByTestId("table-upload-give-up");
  await expect(question).toContainText("orders will not be on the server");
  await question.getByRole("button", { name: "Cancel" }).click();
  server.mishap = null;
  await expect.poll(() => statuses(page, docId), { timeout: 30_000 }).toEqual(["ready"]);
  await expect(upload).toBeHidden();
});

test("an upload that was refused is tried again from any workbook", async ({ page }) => {
  const docId = await create(page, "Refused");
  await open(page, docId);
  server.mishap = ({ method, path }) =>
    method === "POST" && path.endsWith("/tables") ? { status: 507 } : undefined;
  await importTable(page, "orders", ordersCsv(500));
  const upload = page.getByTestId("table-upload");
  await expect(upload).toContainText("orders was not uploaded.");
  await page.goto(`${server.baseUrl}/app?hooks&doc=local-${randomUUID()}`);
  await expect(upload).toContainText("orders was not uploaded.", { timeout: 20_000 });
  server.mishap = null;
  await page.getByTestId("table-upload-again").click();
  await expect.poll(() => statuses(page, docId)).toEqual(["ready"]);
  await expect(upload).toBeHidden();
});

test("who signs out leaves no tables behind in this browser", async ({ page }) => {
  const docId = await create(page, "Signed out");
  await putTable(page, docId, 5_000);
  await open(page, docId);
  await showTable(page, "orders");
  await expect.poll(() => copies(page)).toHaveLength(1);
  await page.getByTestId("cloud-menu-btn").click();
  await page.getByTestId("collab-logout-btn").click();
  await expect.poll(() => copies(page)).toEqual([]);
  await expect(page.getByRole("tab", { name: "orders" })).toHaveCount(0);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
});

test("somebody whose access is taken away loses the table, and nothing else", async ({
  browser,
  page,
}) => {
  const docId = await create(page, "Taken away");
  await putTable(page, docId, 5_000);
  await withMember(browser, page, docId, "editor", async (other, userId) => {
    await showTable(other, "orders");
    await expect.poll(() => copies(other)).toHaveLength(1);
    await api(page, server, "DELETE", `/workbooks/${docId}/members/${userId}`);
    await lookAgain(other);
    await expect(other.getByTestId("table-problem")).toContainText(
      "You may not do this with the data tables of this workbook.",
    );
    await expect(
      other.getByTestId("table-problem").getByRole("button", { name: /Delete/ }),
    ).toBeHidden();
    await expect.poll(() => copies(other)).toEqual([]);
    await expect(tab(other, "orders")).toHaveCount(0);
    await tab(other, "Sheet1").click();
    await expect(view(other)).toBeHidden();
    await expect(other.getByTestId("engine-status")).toHaveText("ready");
  });
});

test("a table that somebody else is still uploading is listed, and not there yet", async ({
  page,
}) => {
  const docId = await create(page, "Under way");
  await api(page, server, "POST", `/workbooks/${docId}/tables`, {
    id: randomUUID(),
    name: "coming",
    bytes: 1_000_000,
    sha256: "0".repeat(64),
  });
  await open(page, docId);
  await tab(page, "coming").click();
  await expect(page.getByTestId("table-problem")).toContainText(
    "The data table is still being uploaded.",
  );
  await page.keyboard.press("Tab");
  await runCommand(page, "tableDelete");
  await page.getByTestId("table-delete-confirm").click();
  await expect(page.getByRole("tab")).toHaveText(["Sheet1"]);
  await expect.poll(() => statuses(page, docId)).toEqual([]);
});

test("a table that was had is there without a connection", async ({ page }) => {
  const docId = await create(page, "Offline");
  await putTable(page, docId, 20_000);
  await open(page, docId);
  await page.waitForFunction(
    async () => (await window.__reticula?.swStatus())?.controlled === true,
  );
  await showTable(page, "orders");
  await expect.poll(() => copies(page)).toHaveLength(1);
  await expect(page.getByTestId("table-here")).toBeHidden({ timeout: 30_000 });
  await page.keyboard.press("ControlOrMeta+End");
  await expect.poll(async () => (await selectedCell(page)).row).toBe("20000");
  const cell = await selectedCell(page);
  await page.waitForTimeout(800);
  await server.kill();
  try {
    await page.reload();
    await expect(view(page)).toHaveAttribute("data-status", "open", { timeout: 20_000 });
    await expect(page.getByRole("tab")).toHaveText(["Sheet1", "orders"]);
    await expect.poll(() => selectedCell(page)).toEqual(cell);
    await goTo(page, "order 1");
    await columnMenu(page, "tableSortDescending");
    await expect.poll(async () => (await selectedCell(page)).text).toBe("20000");
  } finally {
    await server.start();
  }
});

test("a file that is dropped becomes a data table, and no workbook, and why is said", async ({
  page,
}) => {
  const docId = await create(page, "Dropped");
  await open(page, docId);
  await dropFiles(page.getByTestId("grid"), [{ name: "orders.csv", text: ordersCsv(50) }]);
  await expect(page.getByTestId("import-as-table")).toBeChecked();
  await expect(page.getByTestId("import-as-workbook")).toBeDisabled();
  await expect(page.getByTestId("table-import-cloud")).toContainText("Open a local workbook first");
  await expect(page.getByTestId("table-import-preview")).toBeVisible();
  await page.getByTestId("table-import-start").click();
  await expect(tab(page, "orders")).toHaveAttribute("aria-selected", "true");
  await dropFiles(tableGrid(page), [{ name: "a.rtcl", text: "x" }]);
  await expect(page.getByTestId("rejected-banner")).toContainText("Open a local workbook first");
  await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced");
});
