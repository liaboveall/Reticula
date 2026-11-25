// Tables of a workbook on the server: reading them in parts, copies of them in
// this browser, and uploading them. Nothing of it is on the screen here.
import { type BrowserContext, chromium, expect, type Page, test } from "@playwright/test";
import { decodePage, type Page as TablePage } from "../../packages/grid/src/page.ts";
import type { TableRecord } from "../../packages/protocol/src/generated/collab-api.ts";
import { createAndShareSheet, joinViaSharePath, signInDev } from "./collab-helpers.ts";
import { canKeepTables, runCommand } from "./tables-helpers.ts";
import {
  api,
  ask,
  connect,
  copies,
  empty,
  enter,
  type Passing,
  putTable,
  queue,
  type RemoteServer,
  refused,
  startRemoteServer,
  type Whose,
} from "./tables-remote-helpers.ts";

const ROWS = 600_000;
const BLOCK = 64 << 10;
/** The most that is sent at a time. */
const PIECE = 8 << 20;

test.describe.configure({ mode: "serial" });
test.setTimeout(120_000);

let server: RemoteServer;
/** The workbook with the table that most tests read. */
let docId = "";
let table: TableRecord;
let alice = "";
/** The workbooks that were created. Those of other tests are theirs to delete. */
const workbooks: string[] = [];

async function create(page: Page, title: string): Promise<string> {
  const { id } = await api<{ id: string }>(page, server, "POST", "/workbooks", { title });
  workbooks.push(id);
  return id;
}

test.beforeAll(async ({ browser, browserName }) => {
  server = await startRemoteServer();
  // Reading a table takes less of a browser than making one.
  const maker = browserName === "webkit" ? await chromium.launch().catch(() => null) : browser;
  const context = await maker?.newContext();
  const page = await context?.newPage();
  if (page) {
    alice = await enter(page, server, "alice");
    docId = await create(page, "Remote tables");
    table = await putTable(page, docId, ROWS);
  }
  await context?.close();
  if (maker !== browser) {
    await maker?.close();
  }
});

test.afterAll(async ({ browser }) => {
  if (docId) {
    // What its owner may store is for other tests as well.
    server.mishap = null;
    const page = await browser.newPage();
    await enter(page, server, "alice");
    for (const id of workbooks) {
      await api(page, server, "DELETE", `/workbooks/${id}`);
    }
    await page.close();
  }
  await server.stop();
});

test.beforeEach(() => {
  test.skip(docId === "", "no browser here makes tables");
  server.mishap = null;
});

/** Skips a test where nothing can be kept on the disk. */
async function needsDisk(page: Page): Promise<void> {
  test.skip(!(await canKeepTables(page)), "no synchronous access handles");
}

const owner = (): Whose => ({ docId, userId: alice, canWrite: true });

const subscription = (first: number, epoch = 1) => ({
  epoch,
  firstRow: first,
  rowCount: 128,
  firstCol: 0,
  colCount: 13,
  visible: { firstRow: first, endRow: first + 40, firstCol: 0, endCol: 13 },
  resend: true,
});

/** Opens the table. */
async function open(page: Page): Promise<void> {
  await ask(page, (client, id) => client.open(id).then(() => null), table.id);
}

/** Looks at rows, and gives their pages once they are here. */
async function look(page: Page, first: number, epoch = 1, count = 2): Promise<TablePage[]> {
  await page.evaluate(() => window.__remote?.pages.splice(0));
  await ask(page, async (client, cells) => client.subscribe(cells), subscription(first, epoch));
  await page.waitForFunction((n) => (window.__remote?.pages.length ?? 0) >= n, count, {
    timeout: 30_000,
  });
  const buffers = await page.evaluate(() =>
    (window.__remote?.pages.splice(0) ?? []).map((buffer) => Array.from(new Uint8Array(buffer))),
  );
  return buffers.map((bytes) => decodePage(Uint8Array.from(bytes).buffer));
}

const texts = (pages: readonly TablePage[]) => pages.map(({ texts: all }) => all);

const part = (page: Page) =>
  page.evaluate(() => window.__remote?.parts.at(-1) ?? { have: 0, total: 1, isStalled: false });

/** Waits until all of the table is here. */
async function filled(page: Page): Promise<void> {
  await expect
    .poll(
      async () => {
        const { have, total } = await part(page);
        return have === total;
      },
      { timeout: 60_000 },
    )
    .toBe(true);
}

const failures = (page: Page) =>
  page.evaluate(() => (window.__remote?.failures ?? []).map(({ code }) => code));

const isData = ({ method, path }: Passing) => method === "GET" && path.endsWith("/data");
const isPatch = ({ method, path }: Passing) => method === "PATCH" && path.endsWith("/data");

/** Nothing is had but what is asked for. */
const noFill = (passing: Passing) =>
  isData(passing) && passing.range && passing.range[1] - passing.range[0] > 40 * BLOCK
    ? ({ status: 503 } as const)
    : undefined;

async function withShare(page: Page, role: "viewer" | "editor"): Promise<string> {
  const { url } = await api<{ url: string }>(
    page,
    server,
    "POST",
    `/workbooks/${docId}/share-links`,
    { role },
  );
  return /[^/=]+$/.exec(url)?.[0] ?? "";
}

/** Another account that takes part in the workbook. */
async function member(
  context: BrowserContext,
  ownerPage: Page,
  account: "bob" | "carol",
  role: "viewer" | "editor",
): Promise<{ readonly page: Page; readonly userId: string }> {
  const token = await withShare(ownerPage, role);
  const page = await context.newPage();
  const userId = await enter(page, server, account);
  await api(page, server, "POST", `/join/${token}`);
  await api(ownerPage, server, "PATCH", `/workbooks/${docId}/members/${userId}`, { role });
  return { page, userId };
}

test("what is on screen comes in ranges of the file, whole blocks of it", async ({
  browser,
  page,
}) => {
  await enter(page, server, "alice");
  const shareToken = await withShare(page, "viewer");
  await api(page, server, "PATCH", `/workbooks/${docId}`, { visibility: "link_view" });
  const context = await browser.newContext();
  try {
    // Who was given a link has nothing come that is not looked at.
    const visitor = await context.newPage();
    await enter(visitor, server, null);
    const whose = { docId, userId: "", canWrite: false, shareToken };
    const [record] = await connect(visitor, whose);
    expect(record).toMatchObject({ id: table.id, rows: ROWS, status: "ready" });
    server.bytes = 0;
    const before = server.passed.length;
    const opened = await ask(visitor, (client, id) => client.open(id), table.id);
    expect(opened.info.rows).toBe(ROWS);
    const first = await look(visitor, 0);
    expect(first.map(({ firstRow }) => firstRow)).toEqual([0, 64]);
    const ranges = server.passed.slice(before).filter(isData);
    expect(ranges.length).toBeLessThan(20);
    for (const { range } of ranges) {
      const [from, until] = range ?? [1, 1];
      expect(from % BLOCK).toBe(0);
      expect(until === table.bytes || until % BLOCK === 0).toBe(true);
    }
    // The directory, and what the first rows have: a small part of the file.
    expect(server.bytes).toBeGreaterThan(0);
    expect(server.bytes).toBeLessThan(table.bytes / 5);
    test.info().annotations.push({
      type: "bytes",
      description: `first screen ${server.bytes} of ${table.bytes}`,
    });
    // Far down, where the texts of rows may be anywhere among all texts.
    const { bytes } = server;
    const far = await look(visitor, 450_048, 2);
    // Those that wait for less are there first.
    expect(far.map(({ firstRow }) => firstRow).sort()).toEqual([450_048, 450_112]);
    expect(server.bytes - bytes).toBeLessThan((table.bytes * 2) / 3);
    // Nothing else comes, and nothing that was here comes again.
    const sofar = server.bytes;
    await visitor.waitForTimeout(1_000);
    expect(texts(await look(visitor, 0, 3))).toEqual(texts(first));
    expect(server.bytes).toBe(sofar);
    expect(await copies(visitor)).toEqual([]);
    // Unless something is asked about all of it.
    expect((await ask(visitor, (client) => client.summary([0]))).columns[0]?.rows).toBe(ROWS);
    expect(server.bytes).toBeGreaterThanOrEqual(table.bytes);
  } finally {
    await context.close();
  }
});

test("a table is whole before it is sorted, and is then read without the server", async ({
  page,
}) => {
  await enter(page, server, "alice");
  await needsDisk(page);
  await connect(page, owner());
  await open(page);
  const first = await look(page, 0);
  const query = { filters: [], order: [{ column: 2, descending: true }] };
  const progress = await ask(
    page,
    async (client, asked) => {
      const haves: (number | undefined)[] = [];
      const { rows, pages } = await client.query(asked, {
        onProgress: ({ have }) => haves.push(have),
      });
      return { rows, pages: pages.length, haves };
    },
    query,
  );
  expect(progress.rows).toBe(ROWS);
  await filled(page);
  expect(server.bytes).toBeGreaterThanOrEqual(table.bytes);
  const summary = await ask(page, (client) => client.summary([0]));
  expect(summary.columns[0]?.rows).toBe(ROWS);

  // The same as where all of it has been from the start.
  const sorted = await look(page, 300_032, 5);
  const locally = await page.evaluate(
    async ([id, asked, cells]) => {
      const { TableClient } = await (
        window.__reticula as NonNullable<Window["__reticula"]>
      ).tables();
      const pages: ArrayBuffer[] = [];
      const client = new TableClient(
        { docId: "compare", tabId: crypto.randomUUID() },
        { onPage: (buffer) => pages.push(buffer) },
      );
      await client.acquire();
      const { entry } = await client.sample("orders", id, 7);
      await client.open(entry.id);
      await client.query(asked);
      client.subscribe(cells);
      while (pages.length < 2) {
        await new Promise((later) => setTimeout(later, 20));
      }
      await client.delete(entry.id);
      await client.close();
      return pages.map((buffer) => Array.from(new Uint8Array(buffer)));
    },
    [ROWS, query, subscription(300_032, 5)] as const,
  );
  expect(texts(sorted)).toEqual(
    texts(locally.map((bytes) => decodePage(Uint8Array.from(bytes).buffer))),
  );
  expect(await copies(page)).toEqual([table.sha256]);

  // There is no connection any more.
  await connect(page, owner());
  server.mishap = () => ({ status: 503 });
  server.bytes = 0;
  await open(page);
  expect(texts(await look(page, 0))).toEqual(texts(first));
  expect((await ask(page, (client, asked) => client.query(asked), query)).rows).toBe(ROWS);
  expect(server.bytes).toBe(0);
});

test("waiting for all of a table can be given up, and rows are looked at meanwhile", async ({
  page,
}) => {
  await enter(page, server, "alice");
  await connect(page, owner());
  server.mishap = noFill;
  await open(page);
  await look(page, 0);
  await page.evaluate(() => {
    const state = window as unknown as { __given: AbortController; __asked: Promise<string> };
    state.__given = new AbortController();
    state.__asked = (window.__remote as NonNullable<Window["__remote"]>).client
      .query(
        { filters: [], order: [{ column: 0, descending: true }] },
        { signal: state.__given.signal },
      )
      .then(
        () => "answered",
        (error: { code: string }) => error.code,
      );
  });
  await expect.poll(async () => (await part(page)).isStalled).toBe(true);
  // What is looked at comes in between.
  server.mishap = (passing) =>
    passing.range && passing.range[0] < 0 ? undefined : noFill(passing);
  expect(await look(page, 200_064, 2)).toHaveLength(2);
  const code = await page.evaluate(() => {
    const state = window as unknown as { __given: AbortController; __asked: Promise<string> };
    state.__given.abort();
    return state.__asked;
  });
  expect(code).toBe("cancelled");
  expect(await look(page, 0, 3)).toHaveLength(2);
});

test("a viewer reads a table out of memory, and can sort it", async ({ browser, page }) => {
  await enter(page, server, "alice");
  const context = await browser.newContext();
  try {
    const { page: viewer, userId } = await member(context, page, "bob", "viewer");
    await connect(viewer, { docId, userId, canWrite: false });
    await open(viewer);
    expect(await look(viewer, 128)).toHaveLength(2);
    await filled(viewer);
    const { rows } = await ask(viewer, (client) =>
      client.query({ filters: [], order: [{ column: 1, descending: false }] }),
    );
    expect(rows).toBe(ROWS);
    expect(await copies(viewer)).toEqual([]);
    for (const refusal of [
      await refused(viewer, (client, id) => client.rename(id, "mine"), table.id),
      await refused(viewer, (client, id) => client.move(id, 0), table.id),
      await refused(viewer, (client, id) => client.delete(id), table.id),
      await refused(viewer, (client) => client.sample("s", 10, 1)),
    ]) {
      expect(refusal).toBe("denied");
    }
  } finally {
    await context.close();
  }
});

test("access that is taken away closes the table, and leaves no copy of it", async ({
  browser,
  page,
}) => {
  await enter(page, server, "alice");
  const context = await browser.newContext();
  try {
    const { page: editor, userId } = await member(context, page, "carol", "editor");
    await connect(editor, { docId, userId, canWrite: true });
    server.mishap = noFill;
    await open(editor);
    await look(editor, 0);
    expect(await copies(editor)).toEqual((await canKeepTables(editor)) ? [table.sha256] : []);
    await api(page, server, "DELETE", `/workbooks/${docId}/members/${userId}`);
    await ask(editor, async (client, cells) => client.subscribe(cells), subscription(300_032, 2));
    await expect.poll(() => failures(editor)).toEqual(["denied"]);
    await expect.poll(() => copies(editor)).toEqual([]);
    expect(await refused(editor, (client) => client.text(0, 0))).toBe("closed");
    expect(await refused(editor, (client, id) => client.open(id), table.id)).toBe("denied");
  } finally {
    await context.close();
  }
});

test("damage on the way is healed, and damage that stays is reported", async ({
  browser,
  page,
}) => {
  await enter(page, server, "alice");
  // In the first block, which the first rows are in; once.
  const damaged: number[] = [];
  server.mishap = (passing) => {
    if (isData(passing) && passing.range && passing.range[0] === 0 && damaged.length === 0) {
      damaged.push(1);
      return { damage: 5_000 };
    }
    return undefined;
  };
  await connect(page, owner());
  await open(page);
  const healed = await look(page, 0);
  expect(healed).toHaveLength(2);
  await filled(page);
  expect((await ask(page, (client) => client.summary([0, 1, 2, 3]))).columns).toHaveLength(4);
  expect(damaged).toHaveLength(1);
  expect(await failures(page)).toEqual([]);

  // In a browser without a copy, however often it comes.
  const context = await browser.newContext();
  try {
    const fresh = await context.newPage();
    await enter(fresh, server, "alice");
    server.mishap = (passing) => (isData(passing) ? { damage: 5_000 } : undefined);
    await connect(fresh, owner());
    await open(fresh);
    await ask(fresh, async (client, cells) => client.subscribe(cells), subscription(0, 2));
    await expect.poll(() => failures(fresh)).toEqual(["corrupt"]);
  } finally {
    await context.close();
  }
});

test("a server that does not know ranges, and one that has no time, are got along with", async ({
  browser,
  page,
}) => {
  await enter(page, server, "alice");
  const token = await withShare(page, "viewer");
  const context = await browser.newContext();
  try {
    const visitor = await context.newPage();
    await enter(visitor, server, null);
    await connect(visitor, { docId, userId: "", canWrite: false, shareToken: token });
    await open(visitor);
    const first = texts(await look(visitor, 0));
    let busy = 2;
    server.mishap = (passing) =>
      isData(passing) && --busy >= 0 ? { status: 429, headers: { "Retry-After": "1" } } : undefined;
    expect(await look(visitor, 500_032, 2)).toHaveLength(2);
    expect(busy).toBeLessThan(0);
    // The connection breaks off in the middle, and the rest comes afterwards.
    let breaks = 3;
    server.mishap = (passing) =>
      isData(passing) && --breaks >= 0 ? { dropAfter: 100_000 } : undefined;
    expect(await look(visitor, 300_032, 3)).toHaveLength(2);
    expect(breaks).toBeLessThan(0);

    server.mishap = (passing) => (isData(passing) ? "whole" : undefined);
    await connect(visitor, { docId, userId: "", canWrite: false, shareToken: token });
    await open(visitor);
    expect(texts(await look(visitor, 0))).toEqual(first);
    const { rows } = await ask(visitor, (client) =>
      client.query({ filters: [], order: [{ column: 0, descending: true }] }),
    );
    expect(rows).toBe(ROWS);
  } finally {
    await context.close();
  }
});

test("a second tab reads the table as well, out of memory", async ({ page }) => {
  await enter(page, server, "alice");
  await needsDisk(page);
  const second = await page.context().newPage();
  await enter(second, server, "alice");
  for (const tab of [page, second]) {
    await connect(tab, owner());
    await open(tab);
  }
  expect(texts(await look(second, 1_024))).toEqual(texts(await look(page, 1_024)));
  await Promise.all([filled(page), filled(second)]);
  const { rows } = await ask(second, (client) =>
    client.query({ filters: [], order: [{ column: 3, descending: true }] }),
  );
  expect(rows).toBe(ROWS);
  expect(await copies(page)).toEqual([table.sha256]);
  // The other one takes the copy once it is free.
  await ask(page, (client) => client.close());
  await connect(second, owner());
  server.mishap = () => ({ status: 503 });
  await open(second);
  expect(await look(second, 1_024, 9)).toHaveLength(2);
});

test("a table is renamed, moved and deleted on the server", async ({ page }) => {
  await enter(page, server, "alice");
  await needsDisk(page);
  const id = await create(page, "Changes");
  const a = await putTable(page, id, 1_000);
  const b = await putTable(page, id, 2_000);
  const whose = { docId: id, userId: alice, canWrite: true };
  const names = async () => (await connect(page, whose)).map(({ name, rows }) => `${name} ${rows}`);
  expect(await names()).toEqual(["orders 1000", "orders 2000"]);
  await ask(page, (client, table) => client.rename(table, "renamed"), b.id);
  await ask(page, (client, table) => client.move(table, 0), b.id);
  expect(await names()).toEqual(["renamed 2000", "orders 1000"]);
  const size = await ask(page, async (client, table) => (await client.export(table)).size, b.id);
  expect(size).toBe(b.bytes);
  await ask(page, (client, table) => client.delete(table), a.id);
  expect(await names()).toEqual(["renamed 2000"]);
  expect(await refused(page, (client, table) => client.open(table), a.id)).toBe("notFound");
});

/** Waits until nothing is to be uploaded from this browser any more. */
async function uploaded(page: Page): Promise<void> {
  await expect.poll(async () => (await queue(page)).length, { timeout: 60_000 }).toBe(0);
}

/** Has what is in the queue uploaded, as the editor does. */
function upload(page: Page): Promise<void> {
  return page.evaluate(async () => {
    const hooks = window.__reticula as NonNullable<Window["__reticula"]>;
    const { resumeUploads } = await hooks.tables();
    const { id } = await (await fetch("/api/auth/me")).json();
    const uploads: NonNullable<Window["__remote"]>["uploads"] = [];
    (window as unknown as { __uploads: typeof uploads }).__uploads = uploads;
    void resumeUploads(id, (progress) => uploads.push(progress));
  });
}

const uploadStates = (page: Page) =>
  page.evaluate(() =>
    (
      (window as unknown as { __uploads?: NonNullable<Window["__remote"]>["uploads"] }).__uploads ??
      []
    ).map(({ state, failure }) => failure?.code ?? state),
  );

test("what is imported is uploaded, also after the tab was closed and the server was killed", async ({
  browser,
  page,
}) => {
  await enter(page, server, "alice");
  await needsDisk(page);
  const id = await create(page, "Uploads");
  const whose = { docId: id, userId: alice, canWrite: true };
  await connect(page, whose);
  // Two pieces, and a part of a third.
  const made = await ask(page, (client) => client.sample("made", 900_000, 3));
  const { entry } = made;
  expect(entry.bytes).toBeGreaterThan(16 << 20);

  // It breaks off in the first piece, and the server dies after the second, of
  // which it had all.
  let pieces = 0;
  let refusals = 0;
  server.mishap = (passing) => {
    if (!isPatch(passing)) {
      return undefined;
    }
    if (pieces++ === 0) {
      return { dropAfter: 3 << 20 };
    }
    return passing.offset >= PIECE && refusals++ === 0 ? { status: 503 } : undefined;
  };
  await upload(page);
  await expect.poll(() => refusals, { timeout: 60_000 }).toBeGreaterThan(0);
  await server.kill();
  await expect.poll(() => uploadStates(page)).toContain("waiting");
  await page.close();
  await server.start();
  server.mishap = null;
  const sofar = server.passed.length;

  const later = await page.context().newPage();
  await enter(later, server, "alice");
  await upload(later);
  await uploaded(later);
  const tables = await connect(later, whose);
  expect(tables).toMatchObject([
    { id: entry.id, name: "made", rows: 900_000, bytes: entry.bytes, status: "ready" },
  ]);
  // What had arrived was not sent again.
  const offsets = server.passed
    .slice(sofar)
    .filter(isPatch)
    .map(({ offset }) => offset);
  expect(Math.min(...offsets)).toBeGreaterThanOrEqual(PIECE);
  // It is among the copies, and need not be had again.
  expect(await copies(later)).toEqual([tables[0]?.sha256]);
  server.bytes = 0;
  await ask(later, (client, table) => client.open(table).then(() => null), entry.id);
  expect(await look(later, 0)).toHaveLength(2);
  expect(server.bytes).toBe(0);

  // Somebody else has it from the server.
  const context = await browser.newContext();
  try {
    const other = await context.newPage();
    await enter(other, server, "alice");
    await connect(other, whose);
    await ask(other, (client, table) => client.open(table).then(() => null), entry.id);
    const { rows } = await ask(other, (client) =>
      client.query({ filters: [], order: [{ column: 0, descending: true }] }),
    );
    expect(rows).toBe(900_000);
  } finally {
    await context.close();
  }
});

test("a table is open while it is uploaded, and stays so afterwards", async ({ page }) => {
  await enter(page, server, "alice");
  await needsDisk(page);
  const id = await create(page, "Open");
  const whose = { docId: id, userId: alice, canWrite: true };
  await connect(page, whose);
  const { entry } = await ask(page, (client) => client.sample("made", 100_000, 3));
  // What the server lists is all there is here: it is listed once it is declared.
  let isHeld = true;
  server.mishap = (passing) => (isPatch(passing) && isHeld ? { status: 503 } : undefined);
  await upload(page);
  await expect
    .poll(async () => (await connect(page, whose)).map(({ status }) => status))
    .toEqual(["uploading"]);
  await ask(page, (client, table) => client.open(table).then(() => null), entry.id);
  const before = await look(page, 64);
  expect(server.passed.some((passing) => isData(passing) && passing.path.includes(entry.id))).toBe(
    false,
  );
  isHeld = false;
  await uploaded(page);
  expect(texts(await look(page, 64, 2))).toEqual(texts(before));
  expect(await look(page, 50_048, 3)).toHaveLength(2);
  expect(await failures(page)).toEqual([]);
});

test("what the server has no room for stays in the queue, and can be given up", async ({
  page,
}) => {
  await enter(page, server, "alice");
  await needsDisk(page);
  const id = await create(page, "Full");
  const whose = { docId: id, userId: alice, canWrite: true };
  await connect(page, whose);
  const { entry } = await ask(page, (client) => client.sample("made", 10_000, 3));
  server.mishap = ({ method, path }) =>
    method === "POST" && path.endsWith("/tables") ? { status: 507 } : undefined;
  await upload(page);
  await expect.poll(() => uploadStates(page)).toEqual(["serverFull"]);
  server.mishap = null;
  await upload(page);
  await page.waitForTimeout(300);
  expect(await uploadStates(page)).toEqual([]);
  expect(await queue(page)).toEqual([`${entry.id}.json`, `${entry.id}.rtbl`]);
  await empty(page);
  expect(await connect(page, whose)).toEqual([]);
});

test("the tables of a local workbook go with it, and it stays until they have", async ({
  page,
}) => {
  await enter(page, server, "alice");
  await needsDisk(page);
  const id = await create(page, "Converted");
  const moved = await page.evaluate(
    async ([cloud, userId]) => {
      const { TableClient, deleteTables, readCatalog } = await (
        window.__reticula as NonNullable<Window["__reticula"]>
      ).tables();
      const client = new TableClient({ docId: "converted", tabId: crypto.randomUUID() }, {});
      await client.acquire();
      const { entry } = await client.sample("local", 5_000, 3);
      // As converting the workbook does.
      const table = crypto.randomUUID();
      let dir = await navigator.storage.getDirectory();
      for (const name of ["reticula-v1", "tables-queue"]) {
        dir = await dir.getDirectoryHandle(name, { create: true });
      }
      const file = await dir.getFileHandle(`${table}.json`, { create: true });
      const writable = await file.createWritable();
      const queued = {
        userId,
        docId: cloud,
        entry: { ...entry, id: table },
        sha256: "",
        queuedAt: Date.now(),
        from: { docId: "converted", id: entry.id },
      };
      await writable.write(JSON.stringify(queued));
      await writable.close();
      const refusal = await client.delete(entry.id).catch((error: { code: string }) => error.code);
      await client.close();
      const isDeleted = await deleteTables("converted");
      return { refusal, isDeleted, left: await readCatalog("converted") };
    },
    [id, alice] as const,
  );
  expect(moved).toMatchObject({
    refusal: "uploading",
    isDeleted: false,
    left: [{ name: "local" }],
  });
  await upload(page);
  await uploaded(page);
  expect(await connect(page, { docId: id, userId: alice, canWrite: true })).toMatchObject([
    { name: "local", rows: 5_000, status: "ready" },
  ]);
  const isDeleted = await page.evaluate(async () => {
    const hooks = window.__reticula as NonNullable<Window["__reticula"]>;
    return (await hooks.tables()).deleteTables("converted");
  });
  expect(isDeleted).toBe(true);
});

test("a table that one adds is seen by another within 20 seconds, and what becomes of it", async ({
  browser,
  page,
}) => {
  await signInDev(page, server.baseUrl, "alice");
  await needsDisk(page);
  const { docId: shared, joinPath } = await createAndShareSheet(page, "Shared tables");
  workbooks.push(shared);
  const context = await browser.newContext();
  try {
    const bob = await context.newPage();
    await joinViaSharePath(bob, server.baseUrl, joinPath, "bob");
    const tab = (on: Page, name: string) => on.getByRole("tab", { name, exact: true });
    const soon = { timeout: 25_000 };
    const list = `/workbooks/${shared}/tables`;
    const added = await putTable(page, shared, 1_000);
    await expect(tab(bob, "orders")).toBeVisible(soon);
    await expect(tab(page, "orders")).toBeVisible(soon);
    // From now on the page is told to look, as it does when somebody comes back to it.
    const lookAgain = () =>
      bob.evaluate(() => document.dispatchEvent(new Event("visibilitychange")));
    await api(page, server, "PATCH", `${list}/${added.id}`, { name: "sold" });
    await lookAgain();
    await expect(tab(bob, "sold")).toBeVisible();
    await expect(tab(bob, "orders")).toHaveCount(0);
    await tab(bob, "sold").click();
    await expect(bob.getByTestId("table-view")).toHaveAttribute("data-status", "open");
    await expect.poll(() => copies(bob)).toEqual([added.sha256]);
    // It is deleted as it is by hand, and not only where that is done.
    await page.evaluate(() => document.dispatchEvent(new Event("visibilitychange")));
    await tab(page, "sold").click();
    await runCommand(page, "tableDelete");
    await page.getByTestId("table-delete-confirm").click();
    await expect(tab(page, "sold")).toHaveCount(0);
    await expect
      .poll(async () => (await api<{ tables: object[] }>(page, server, "GET", list)).tables)
      .toEqual([]);
    await page.reload();
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await lookAgain();
    await expect(tab(bob, "sold")).toHaveCount(0);
    await expect.poll(() => copies(bob)).toEqual([]);
    await expect(tab(page, "sold")).toHaveCount(0);
  } finally {
    await context.close();
  }
});
