// What is behind tables: the worker, its files in the browser's storage and the
// client that talks to it. Nothing of it is on the screen here.
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { extname, join, resolve } from "node:path";
import { type BrowserContext, expect, type Page, test } from "@playwright/test";
import { decodePage, type Page as TablePage } from "../../packages/grid/src/page.ts";
import { listen } from "./listen.ts";

type Client = InstanceType<
  Awaited<ReturnType<NonNullable<Window["__reticula"]>["tables"]>>["TableClient"]
>;

interface Harness {
  client: Client;
  readonly pages: ArrayBuffer[];
  readonly catalogs: string[][];
  readonly yields: boolean[];
  readonly failures: { code: string; detail?: string | undefined }[];
}

declare global {
  interface Window {
    __tables?: Harness;
  }
}

const ROWS = 100_000;
const REGIONS = ["North", "south", "East", "West", "North"];

/** The rows of the file that is imported, without the one that the header is. */
function cellsOf(row: number): string[] {
  const day = new Date(Date.UTC(2020, 0, 1 + (row % 1_000))).toISOString().slice(0, 10);
  const amount = ((row * 7_919) % 100_000) / 100;
  return [
    String(row + 1),
    REGIONS[row % REGIONS.length] as string,
    amount.toFixed(2),
    day,
    row % 10 === 0 ? "" : `note ${row % 97}, "quoted"`,
  ];
}

/** Opens a workbook, and starts a client of its tables. */
async function start(page: Page, doc: string): Promise<void> {
  // Another workbook than the one with the tables: the editor looks after its own.
  await page.goto(`/app?hooks&doc=${doc}-sheets`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await connect(page, doc);
}

/** Starts another client in place of the one there is. */
async function connect(page: Page, doc: string): Promise<void> {
  await page.evaluate(async (docId) => {
    const { TableClient } = await (window.__reticula as NonNullable<Window["__reticula"]>).tables();
    const harness: Harness = {
      client: undefined as unknown as Client,
      pages: [],
      catalogs: [],
      yields: [],
      failures: [],
    };
    harness.client = new TableClient(
      { docId, tabId: crypto.randomUUID() },
      {
        onPage: (buffer) => harness.pages.push(buffer),
        onCatalog: (tables) => harness.catalogs.push(tables.map(({ name }) => name)),
        onYielded: (evicted) => harness.yields.push(evicted),
        onFailed: (failure) => harness.failures.push(failure),
      },
    );
    window.__tables = harness;
  }, doc);
}

/** What a call of the client comes to: its answer, or why it was refused. */
async function ask<A, T>(
  page: Page,
  run: (client: Client, argument: A) => Promise<T>,
  argument?: A,
): Promise<T> {
  return page.evaluate(
    ([source, value]) => {
      const call = new Function(`return (${source})`)() as (c: Client, a: A) => Promise<T>;
      return call((window.__tables as Harness).client, value as A);
    },
    [run.toString(), argument] as const,
  );
}

async function refused<A>(
  page: Page,
  run: (client: Client, argument: A) => Promise<unknown>,
  argument?: A,
): Promise<{ code: string; row?: number }> {
  return page.evaluate(
    async ([source, value]) => {
      const call = new Function(`return (${source})`)() as (c: Client, a: A) => Promise<unknown>;
      try {
        await call((window.__tables as Harness).client, value as A);
      } catch (error) {
        return (error as { failure: { code: string } }).failure;
      }
      throw new Error("it was not refused");
    },
    [run.toString(), argument] as const,
  );
}

/** Makes the text file in the page, which is much faster than handing it over. */
async function makeCsv(page: Page, rows = ROWS): Promise<void> {
  await page.evaluate(
    ([source, count, regions]) => {
      const cells = new Function("REGIONS", `return (${source})`)(regions) as typeof cellsOf;
      const quote = (text: string) =>
        /[",]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
      const lines = ["id,region,amount,day,note"];
      for (let row = 0; row < count; row++) {
        lines.push(cells(row).map(quote).join(","));
      }
      (window as unknown as { __csv: Blob }).__csv = new Blob([lines.join("\n")]);
    },
    [cellsOf.toString(), rows, REGIONS] as const,
  );
}

const importCsv = (client: Client, name: string) =>
  client.importCsv({ file: (window as unknown as { __csv: Blob }).__csv, options: {} }, name);

const subscription = {
  epoch: 1,
  firstRow: 0,
  rowCount: 128,
  firstCol: 0,
  colCount: 5,
  visible: { firstRow: 0, endRow: 40, firstCol: 0, endCol: 5 },
  resend: true,
};

/** Opens a table, and waits for its first pages. */
async function openTable(page: Page, id: string): Promise<{ rows: number; names: string[] }> {
  return ask(
    page,
    async (client, [table, cells]) => {
      const opened = await client.open(table);
      client.subscribe(cells);
      return { rows: opened.view.rows, names: opened.info.columns.map(({ name }) => name) };
    },
    [id, subscription] as const,
  );
}

/** Takes the pages that came, once there are so many. */
async function takePages(page: Page, count: number): Promise<TablePage[]> {
  await page.waitForFunction((n) => (window.__tables?.pages.length ?? 0) >= n, count);
  const buffers = await page.evaluate(() =>
    (window.__tables as Harness).pages
      .splice(0)
      .map((buffer) => Array.from(new Uint8Array(buffer))),
  );
  return buffers.map((bytes) => decodePage(Uint8Array.from(bytes).buffer));
}

function rowOf(pages: readonly TablePage[], row: number): string[] {
  const page = pages.find(({ firstRow, rowCount }) => row >= firstRow && row < firstRow + rowCount);
  if (!page) {
    throw new Error(`row ${row} is on none of the pages`);
  }
  const at = (row - page.firstRow) * page.colCount;
  return page.texts.slice(at, at + page.colCount);
}

/** The names of the files that the tables of a workbook take. */
async function files(page: Page, doc: string): Promise<string[]> {
  return page.evaluate(async (docId) => {
    const names: string[] = [];
    try {
      let dir = await navigator.storage.getDirectory();
      for (const name of ["reticula-v1", "tables", docId]) {
        dir = await dir.getDirectoryHandle(name);
      }
      for await (const name of (dir as unknown as { keys(): AsyncIterable<string> }).keys()) {
        names.push(name);
      }
    } catch {
      // There is no such directory.
    }
    return names.sort();
  }, doc);
}

async function hasSyncHandles(page: Page): Promise<boolean> {
  const backend = await page.evaluate(() => window.__reticula?.docStatus().storageBackend);
  return backend === "opfs-sync";
}

/** Skips a test where the browser cannot keep tables, having checked that it says so. */
async function needsTables(page: Page): Promise<void> {
  if (!(await hasSyncHandles(page))) {
    expect(await refused(page, (client) => client.acquire())).toMatchObject({
      code: "unsupported",
    });
    test.skip(true, "no synchronous access handles");
  }
}

test("a text file is imported, shown, asked about and still there after a reload", async ({
  page,
}) => {
  const doc = "tables-import";
  await start(page, doc);
  await needsTables(page);
  expect(await ask(page, (client) => client.acquire())).toEqual([]);
  expect(await files(page, doc)).toEqual(["catalog-b"]);
  await makeCsv(page);

  const sniffed = await ask(page, (client) =>
    client.sniffCsv({ file: (window as unknown as { __csv: Blob }).__csv, options: {} }),
  );
  expect(sniffed).toMatchObject({ delimiter: ",", header: true, encoding: "utf-8" });
  expect(sniffed.validUtf8).toBe(true);
  expect(sniffed.preview[0]).toEqual(cellsOf(0));

  const imported = await ask(page, importCsv, "orders");
  expect(imported.entry).toMatchObject({ name: "orders", rows: ROWS, cols: 5 });
  expect(imported.report?.columns.map(({ type }) => type.kind)).toEqual([
    "integer",
    "text",
    "decimal",
    "date",
    "text",
  ]);
  expect(imported.report?.columns[4]?.blanks).toBe(ROWS / 10);
  expect(await page.evaluate(() => window.__tables?.catalogs)).toEqual([["orders"]]);

  // The page reads what there is without the worker.
  const listing = () =>
    page.evaluate(async (docId) => {
      const { readCatalog } = await (
        window.__reticula as NonNullable<Window["__reticula"]>
      ).tables();
      return readCatalog(docId);
    }, doc);
  expect(await listing()).toEqual([imported.entry]);

  const { id } = imported.entry;
  expect(await openTable(page, id)).toEqual({
    rows: ROWS,
    names: ["id", "region", "amount", "day", "note"],
  });
  const pages = await takePages(page, 2);
  expect(pages.map(({ epoch, firstRow }) => [epoch, firstRow])).toEqual([
    [1, 0],
    [1, 64],
  ]);
  for (const row of [0, 1, 63, 64, 127]) {
    expect(rowOf(pages, row)).toEqual(cellsOf(row));
  }

  // The end, and no page twice.
  await ask(
    page,
    async (client, cells) =>
      client.subscribe({ ...cells, epoch: 2, firstRow: 99_900, rowCount: 100, resend: false }),
    { ...subscription, visible: { firstRow: 99_950, endRow: 100_000, firstCol: 0, endCol: 5 } },
  );
  const last = await takePages(page, 3);
  expect(last.map(({ epoch, firstRow }) => [epoch, firstRow])).toEqual([
    [2, 99_904],
    [2, 99_968],
    [2, 99_840],
  ]);
  expect(rowOf(last, 99_999)).toEqual(cellsOf(99_999));

  // What is asked about: the answer has the pages that are on screen then.
  const expected = Array.from({ length: ROWS }, (_, row) => ({ row, cells: cellsOf(row) }))
    .filter(({ cells }) => cells[1] === "North" && Number(cells[2]) >= 500)
    .sort((a, b) => Number(b.cells[2]) - Number(a.cells[2]) || a.row - b.row);
  const view = await ask(page, async (client) => {
    const found = await client.query({
      filters: [
        { column: 1, condition: { kind: "oneOf", keys: ["north"], blank: false } },
        { column: 2, condition: { kind: "number", op: "atLeast", a: "500" } },
      ],
      order: [{ column: 2, descending: true }],
    });
    return {
      rows: found.rows,
      pages: found.pages.map((buffer) => Array.from(new Uint8Array(buffer))),
    };
  });
  expect(view.rows).toBe(expected.length);
  const shown = view.pages.map((bytes) => decodePage(Uint8Array.from(bytes).buffer));
  expect(shown).toHaveLength(1);
  for (const row of [0, 1, 39, 63]) {
    expect(rowOf(shown, row)).toEqual(expected[row]?.cells);
    expect(shown[0]?.rowIds[row]).toBe(BigInt(expected[row]?.row ?? -1));
  }

  const summary = await ask(page, (client) => client.summary([2, 4]));
  const sum = expected.reduce((total, { cells }) => total + Math.round(Number(cells[2]) * 100), 0);
  expect(summary.columns[0]).toMatchObject({
    rows: expected.length,
    blanks: 0,
    sum: { text: (sum / 100).toFixed(2), exact: true },
    max: { text: expected[0]?.cells[2] },
  });
  const values = await ask(page, (client) => client.values(1));
  expect(values.complete).toBe(true);
  expect(values.values.map(({ text }) => text.toLowerCase())).toEqual([
    "east",
    "north",
    "south",
    "west",
  ]);

  expect(await ask(page, (client) => client.text(0, 4))).toBe(expected[0]?.cells[4]);
  const copied = await ask(page, (client) =>
    client.copy({ firstRow: 0, rows: 2, firstCol: 1, cols: 4 }),
  );
  const field = (text: string) => (/["\t]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text);
  expect(copied.tsv).toBe(
    expected
      .slice(0, 2)
      .map(({ cells }) => cells.slice(1).map(field).join("\t"))
      .join("\n"),
  );
  expect(copied.html).toContain(`<td>${expected[0]?.cells[2]}</td>`);
  expect(
    await refused(page, (client) =>
      client.copy({ firstRow: 0, rows: 50_000, firstCol: 0, cols: 5 }),
    ),
  ).toMatchObject({ code: "tooLarge" });
  expect(
    await refused(page, (client) =>
      client.query({
        filters: [{ column: 0, condition: { kind: "contains", text: "1" } }],
        order: [],
      }),
    ),
  ).toMatchObject({ code: "invalid", column: 0 });

  // The newest question wins.
  const race = await page.evaluate(async () => {
    const { client } = window.__tables as Harness;
    const order = (column: number) => ({ filters: [], order: [{ column, descending: false }] });
    const outcomes = await Promise.allSettled([
      client.query(order(4)),
      client.query(order(2)),
      client.query({ filters: [], order: [] }),
    ]);
    return outcomes.map((outcome) =>
      outcome.status === "fulfilled" ? outcome.value.rows : outcome.reason.code,
    );
  });
  expect(race).toEqual(["superseded", "superseded", ROWS]);

  await page.reload();
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await connect(page, doc);
  const listed = await ask(page, (client) => client.acquire());
  expect(listed).toEqual([imported.entry]);
  await openTable(page, id);
  expect(rowOf(await takePages(page, 2), 100)).toEqual(cellsOf(100));
});

test("tables are renamed, put into an order, exported, imported and deleted", async ({ page }) => {
  const doc = "tables-manage";
  await start(page, doc);
  await needsTables(page);
  await ask(page, (client) => client.acquire());
  await makeCsv(page, 5_000);
  const first = (await ask(page, importCsv, "orders")).entry;
  const second = (await ask(page, importCsv, "orders")).entry;
  expect(second.name).toBe("orders 2");
  await ask(page, (client, id) => client.rename(id, "Sales"), first.id);
  expect(await refused(page, (client, id) => client.rename(id, "SALES"), second.id)).toMatchObject({
    code: "duplicateName",
  });
  await ask(page, (client, id) => client.move(id, 0), second.id);
  expect(await page.evaluate(() => window.__tables?.catalogs.at(-1))).toEqual([
    "orders 2",
    "Sales",
  ]);

  // Out of the browser and in again, the open one too.
  await openTable(page, first.id);
  const again = await ask(
    page,
    async (client, id) => {
      const file = await client.export(id);
      const { entry } = await client.importFile(file, "again");
      const damaged = new Uint8Array(await file.arrayBuffer());
      damaged[5_000] = (damaged[5_000] ?? 0) ^ 1;
      (window as unknown as { __damaged: Blob }).__damaged = new Blob([damaged]);
      (window as unknown as { __cut: Blob }).__cut = file.slice(0, file.size - 1);
      return { entry, size: file.size };
    },
    first.id,
  );
  expect(again.entry).toMatchObject({ name: "again", rows: 5_000, cols: 5, bytes: again.size });
  expect(again.size).toBe(first.bytes);
  for (const name of ["__damaged", "__cut"]) {
    expect(
      await refused(
        page,
        (client, key) =>
          client.importFile((window as unknown as Record<string, Blob>)[key] as Blob, "bad"),
        name,
      ),
    ).toMatchObject({ code: "corrupt" });
  }
  await openTable(page, again.entry.id);
  expect(rowOf(await takePages(page, 2), 99)).toEqual(cellsOf(99));

  await ask(page, (client, id) => client.delete(id), again.entry.id);
  await ask(page, (client, id) => client.delete(id), second.id);
  expect(await refused(page, (client, id) => client.open(id), second.id)).toMatchObject({
    code: "notFound",
  });
  expect(await files(page, doc)).toEqual([`${first.id}.rtbl`, "catalog-a", "catalog-b"].sort());
  expect(await ask(page, (client) => client.close())).toBeUndefined();
  expect(await page.evaluate(() => window.__reticula?.tableStatus()?.state)).toBe("closed");
});

test("an import that is given up, fails or is killed leaves nothing", async ({ page }) => {
  const doc = "tables-partial";
  await start(page, doc);
  await needsTables(page);
  await ask(page, (client) => client.acquire());
  const catalogs = ["catalog-b"];

  // Given up in the middle.
  await makeCsv(page, 400_000);
  const given = await page.evaluate(async () => {
    const { client } = window.__tables as Harness;
    const controller = new AbortController();
    const progress: number[] = [];
    const importing = client.importCsv(
      { file: (window as unknown as { __csv: Blob }).__csv, options: {} },
      "big",
      {
        signal: controller.signal,
        onProgress: ({ share }) => {
          progress.push(share);
          controller.abort();
        },
      },
    );
    const busy = client.busy;
    const second = await client
      .importCsv({ file: new Blob(["a,b\n1,2"]), options: {} }, "other")
      .catch((error) => error.code);
    const code = await importing.catch((error) => error.code);
    return { busy, second, code, progress, after: client.busy };
  });
  expect(given).toMatchObject({ busy: true, second: "busy", code: "cancelled", after: false });
  expect(given.progress.length).toBeGreaterThan(0);
  expect(await files(page, doc)).toEqual(catalogs);

  // Not what it should be.
  const csv = (text: string) =>
    refused(
      page,
      (client, content) =>
        client.importCsv({ file: new Blob([content]), options: { header: true } }, "bad"),
      text,
    );
  expect(await csv('a,b\n1,2\n"3,4\n')).toMatchObject({ code: "unclosedQuote", row: 3 });
  expect(await csv("")).toMatchObject({ code: "empty" });
  expect(
    await refused(page, (client) =>
      client.importCsv(
        { file: new Blob([Uint8Array.of(0x61, 0x0a, 0xc3, 0xfb, 0x0a)]), options: {} },
        "bad",
      ),
    ),
  ).toMatchObject({ code: "notUtf8" });
  expect(
    await refused(page, (client) => client.importParquet(new Blob(["no such thing"]), "bad")),
  ).toMatchObject({ code: "notParquet" });
  expect(await files(page, doc)).toEqual(catalogs);

  // Killed: what is left is found the next time.
  await page.evaluate(
    () =>
      new Promise<void>((resolve) => {
        const { client } = window.__tables as Harness;
        client
          .importCsv({ file: (window as unknown as { __csv: Blob }).__csv, options: {} }, "big", {
            onProgress: () => {
              client.stop();
              resolve();
            },
          })
          .catch(() => {});
      }),
  );
  await connect(page, doc);
  await expect.poll(() => ask(page, (client) => client.acquire())).toEqual([]);
  expect(await files(page, doc)).toEqual(catalogs);
});

test("text files in other encodings are read, and columns can be told what to hold", async ({
  page,
}) => {
  await start(page, "tables-encodings");
  await needsTables(page);
  await ask(page, (client) => client.acquire());

  const bytes = {
    // "名,价\n茶,3\n" and "é;€\n1;2\n".
    gb18030: [0xc3, 0xfb, 0x2c, 0xbc, 0xdb, 0x0a, 0xb2, 0xe8, 0x2c, 0x33, 0x0a],
    "windows-1252": [0xe9, 0x3b, 0x80, 0x0a, 0x31, 0x3b, 0x32, 0x0a],
    "utf-16le": [0xff, 0xfe, ...[..."a\tb\n1\t2\n"].flatMap((c) => [c.charCodeAt(0), 0])],
  };
  const read = (encoding: keyof typeof bytes, given: boolean) =>
    ask(
      page,
      async (client, [name, content, say]) => {
        const source = {
          file: new Blob([Uint8Array.from(content)]),
          options: { header: true },
          ...(say ? { encoding: name } : {}),
        };
        const { encoding: found, validUtf8 } = await client.sniffCsv(source);
        const { entry } = await client.importCsv(source, name);
        const { info } = await client.open(entry.id);
        const cells = await Promise.all([client.text(0, 0), client.text(0, 1)]);
        return { found, validUtf8, names: info.columns.map((column) => column.name), cells };
      },
      [encoding, bytes[encoding], given] as const,
    );
  expect(await read("gb18030", true)).toEqual({
    found: "gb18030",
    validUtf8: true,
    names: ["名", "价"],
    cells: ["茶", "3"],
  });
  expect((await read("windows-1252", true)).names).toEqual(["é", "€"]);
  // That one says so itself.
  expect(await read("utf-16le", false)).toMatchObject({
    found: "utf-16le",
    names: ["a", "b"],
    cells: ["1", "2"],
  });

  const typed = await ask(page, async (client) => {
    const { entry, report } = await client.importCsv(
      {
        file: new Blob(["00123|x|NA\n45|7|2.5\n"]),
        options: {
          delimiter: "|",
          header: false,
          blankMarkers: true,
          types: [
            { column: 0, type: { kind: "text" } },
            { column: 1, type: { kind: "integer" } },
            { column: 2, type: { kind: "decimal", scale: 3, fixed: false } },
          ],
        },
      },
      "typed",
    );
    await client.open(entry.id);
    const cells = [];
    for (let row = 0; row < 2; row++) {
      cells.push(await Promise.all([0, 1, 2].map((col) => client.text(row, col))));
    }
    return { cells, misfits: report?.columns.map((column) => column.misfits) };
  });
  expect(typed).toEqual({
    cells: [
      ["00123", "", ""],
      ["45", "7", "2.5"],
    ],
    misfits: [0, 1, 0],
  });
});

test("a column whose type only turns out later is read twice", async ({ page }) => {
  await start(page, "tables-restart");
  await needsTables(page);
  await ask(page, (client) => client.acquire());
  const read = await ask(page, async (client) => {
    const lines = ["n,t"];
    for (let row = 0; row < 300_000; row++) {
      lines.push(`${row},${row % 7}`);
    }
    lines.push("many,8");
    const passes = new Set<number | undefined>();
    const { entry, report } = await client.importCsv(
      { file: new Blob([lines.join("\n")]), options: {} },
      "late",
      { onProgress: ({ pass }) => passes.add(pass) },
    );
    await client.open(entry.id);
    return {
      rows: entry.rows,
      types: report?.columns.map(({ type }) => type.kind),
      first: await client.text(0, 0),
      last: await client.text(300_000, 0),
      bytes: (await client.export(entry.id)).size === entry.bytes,
      passes: [...passes],
    };
  });
  expect(read).toMatchObject({
    rows: 300_001,
    types: ["text", "integer"],
    first: "0",
    last: "many",
    bytes: true,
  });
});

test("another tab takes the tables over", async ({ page, context }) => {
  const doc = "tables-tabs";
  await start(page, doc);
  await needsTables(page);
  await ask(page, (client) => client.acquire());
  await makeCsv(page, 2_000);
  const { entry } = await ask(page, importCsv, "shared");
  await openTable(page, entry.id);

  const other = await context.newPage();
  await other.goto("/app?hooks&doc=tables-tabs-other");
  await expect(other.getByTestId("engine-status")).toHaveText("ready");
  await connect(other, doc);
  expect(await refused(other, (client) => client.acquire())).toMatchObject({ code: "blocked" });
  // While they are open they are not deleted from elsewhere either.
  const deleted = () =>
    other.evaluate(async (docId) => {
      const { deleteTables } = await (
        window.__reticula as NonNullable<Window["__reticula"]>
      ).tables();
      return deleteTables(docId);
    }, doc);
  expect(await deleted()).toBe(false);

  expect(await ask(other, (client) => client.acquire(true))).toEqual([entry]);
  await expect.poll(() => page.evaluate(() => window.__tables?.yields)).toEqual([false]);
  expect(await page.evaluate(() => window.__reticula?.tableStatus()?.state)).toBe("yielded");
  expect(await refused(page, (client, id) => client.open(id), entry.id)).toMatchObject({
    code: "closed",
  });
  await openTable(other, entry.id);
  expect(rowOf(await takePages(other, 2), 7)).toEqual(cellsOf(7));

  // And back, while the other one is importing.
  await makeCsv(other, 400_000);
  await other.evaluate(
    () =>
      new Promise<void>((resolve) => {
        const { client } = window.__tables as Harness;
        (window as unknown as { __import: Promise<string> }).__import = client
          .importCsv({ file: (window as unknown as { __csv: Blob }).__csv, options: {} }, "big", {
            onProgress: () => resolve(),
          })
          .then(
            () => "done",
            (error) => error.code,
          );
      }),
  );
  await connect(page, doc);
  expect(await ask(page, (client) => client.acquire(true))).toEqual([entry]);
  expect(
    await other.evaluate(() => (window as unknown as { __import: Promise<string> }).__import),
  ).toBe("yielded");
  expect(await files(page, doc)).toEqual([`${entry.id}.rtbl`, "catalog-a", "catalog-b"].sort());

  await ask(page, (client) => client.close());
  expect(await deleted()).toBe(true);
  expect(await files(page, doc)).toEqual([]);
});

test("the engine goes on when the worker crashes, and the table opens again", async ({ page }) => {
  const doc = "tables-crash";
  await start(page, doc);
  // Where no tables can be kept there is a worker all the same.
  const kept = await hasSyncHandles(page);
  let id = "";
  if (kept) {
    await ask(page, (client) => client.acquire());
    await makeCsv(page, 2_000);
    id = (await ask(page, importCsv, "fragile")).entry.id;
    await openTable(page, id);
    await takePages(page, 2);
  }

  const pending = page.evaluate(() =>
    (window.__tables as Harness).client.summary([0]).then(
      () => "done",
      (error) => error.code,
    ),
  );
  await page.evaluate(() => window.__reticula?.crashDataWorker());
  await expect
    .poll(() => page.evaluate(() => window.__reticula?.tableStatus()))
    .toMatchObject({ state: "failed", failure: { code: "crashed" } });
  expect(["done", "closed", "crashed"]).toContain(await pending);
  expect(await page.evaluate(() => window.__tables?.failures.map(({ code }) => code))).toEqual([
    "crashed",
  ]);

  // The engine answers, and keeps what is typed.
  const digest = await page.evaluate(() => window.__reticula?.stateDigest());
  await page.evaluate(() => window.__reticula?.select(3, 3));
  await page.getByTestId("grid").click({ position: { x: 300, y: 200 } });
  await page.keyboard.type("still here");
  await page.keyboard.press("Enter");
  await expect.poll(() => page.evaluate(() => window.__reticula?.stateDigest())).not.toBe(digest);

  if (kept) {
    await connect(page, doc);
    // The browser may take a moment to give up the lock of a worker that is gone.
    await expect
      .poll(() =>
        ask(page, (client) =>
          client.acquire().then(
            (all) => all.length,
            () => -1,
          ),
        ),
      )
      .toBe(1);
    await openTable(page, id);
    expect(rowOf(await takePages(page, 2), 9)).toEqual(cellsOf(9));
  }
});

test("a question that takes too much memory is refused, and the table stays", async ({ page }) => {
  await start(page, "tables-memory");
  await needsTables(page);
  await ask(page, (client) => client.acquire());
  const { entry } = await ask(page, (client) => client.sample("sample", 1_200_000, 1));
  await openTable(page, entry.id);
  const before = await takePages(page, 2);
  await page.evaluate(() => window.__reticula?.tableMemoryLimit(1 << 20));
  const sort = () =>
    ask(page, (client) =>
      client.query({ filters: [], order: [{ column: 7, descending: false }] }).then(
        ({ rows }) => rows,
        (error) => error.code,
      ),
    );
  expect(await sort()).toBe("memory");
  expect(await page.evaluate(() => window.__reticula?.tableStatus()?.state)).toBe("open");
  await ask(page, async (client, cells) => client.subscribe({ ...cells, epoch: 2 }), subscription);
  const after = await takePages(page, 2);
  expect(after.map(({ texts }) => texts)).toEqual(before.map(({ texts }) => texts));
  await page.evaluate(() => window.__reticula?.tableMemoryLimit(null));
  expect(await sort()).toBe(1_200_000);
});

/** Has the storage of the page's origin run full after so many bytes. Chromium only. */
async function limitStorage(context: BrowserContext, page: Page, bytes: number): Promise<void> {
  const session = await context.newCDPSession(page);
  await session.send("Storage.overrideQuotaForOrigin", {
    origin: new URL(page.url()).origin,
    quotaSize: bytes,
  });
}

test("an import there is no room for is refused, and leaves nothing", async ({
  page,
  context,
  browserName,
}) => {
  test.skip(browserName !== "chromium", "only Chromium lets a test set the limit");
  const doc = "tables-quota";
  await start(page, doc);
  await ask(page, (client) => client.acquire());
  await makeCsv(page, 2_000);
  const { entry } = await ask(page, importCsv, "small");
  const used = await page.evaluate(async () => (await navigator.storage.estimate()).usage ?? 0);
  await limitStorage(context, page, used + (1 << 20));
  expect(await refused(page, (client) => client.sample("large", 2_000_000, 1))).toMatchObject({
    code: "quota",
  });
  expect(await files(page, doc)).toEqual([`${entry.id}.rtbl`, "catalog-a", "catalog-b"].sort());
  await limitStorage(context, page, 1 << 30);
  expect((await ask(page, (client) => client.sample("fits", 2_000, 1))).entry.rows).toBe(2_000);
});

test("Parquet files are read, whatever they are packed with", async ({ page }) => {
  await start(page, "tables-parquet");
  await needsTables(page);
  await ask(page, (client) => client.acquire());
  const samples = resolve("tests/conformance/parquet");
  const [names = [], ...rows] = (await readFile(join(samples, "arrow-types.tsv"), "utf8"))
    .split("\n")
    .map((line) => line.split("\t"));
  const columns = ["truth", "i32", "i64", "dec18", "day", "us", "words", "id"];
  for (const packing of ["snappy", "gzip", "zstd", "lz4"]) {
    const bytes = await readFile(join(samples, `arrow-${packing}.parquet`), "base64");
    const read = await ask(
      page,
      async (client, [content, name, wanted]) => {
        const file = new Blob([Uint8Array.from(atob(content), (c) => c.charCodeAt(0))]);
        const inspected = await client.inspectParquet(file);
        const { entry, report } = await client.importParquet(file, name);
        const { info } = await client.open(entry.id);
        const cells = (row: number) =>
          Promise.all(
            wanted.map((column) =>
              client.text(
                row,
                info.columns.findIndex((found) => found.name === column),
              ),
            ),
          );
        return {
          rows: [inspected.rows, entry.rows, info.rows],
          columns: [inspected.columns.length, entry.cols, report?.columns.length],
          cells: [await cells(1), await cells(5)],
        };
      },
      [bytes, packing, columns] as const,
    );
    expect(new Set(read.rows).size).toBe(1);
    expect(new Set(read.columns).size).toBe(1);
    const expected = (row: number) => columns.map((name) => rows[row]?.[names.indexOf(name)]);
    expect(read.cells).toEqual([expected(1), expected(5)]);
  }

  // Some columns only, and what tables do not have.
  const nested = await readFile(join(samples, "arrow-nested.parquet"), "base64");
  const chosen = await ask(
    page,
    async (client, content) => {
      const file = new Blob([Uint8Array.from(atob(content), (c) => c.charCodeAt(0))]);
      const { columns: found, skipped } = await client.inspectParquet(file);
      const { entry } = await client.importParquet(file, "some", [1]);
      const { info } = await client.open(entry.id);
      return {
        found: found.map(({ name }) => name),
        skipped: skipped.map(({ reason }) => reason),
        taken: info.columns.map(({ name }) => name),
      };
    },
    nested,
  );
  expect(chosen.found).toEqual(["before", "after"]);
  expect(chosen.skipped.length).toBeGreaterThan(0);
  expect(new Set(chosen.skipped)).toEqual(new Set(["nested"]));
  expect(chosen.taken).toEqual(["after"]);
});

const DIST_DIR = resolve(process.env.RETICULA_E2E_DIST ?? "apps/web/dist");
const TYPES: Readonly<Record<string, string>> = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".css": "text/css",
  ".wasm": "application/wasm",
};
const LATER_BUILD = "0123456789abcdef";

/** The production build, with a service worker that can be made one of a later build. */
async function serveBuild(): Promise<{
  readonly origin: string;
  update(): void;
  stop(): Promise<void>;
}> {
  const worker = await readFile(join(DIST_DIR, "sw.js"), "utf8");
  const build = /reticula-shell-.{0,40}?([0-9a-f]{16})|\b([0-9a-f]{16})\b/.exec(worker);
  const id = build?.[1] ?? build?.[2] ?? "";
  let current = worker;
  const server = createServer(async (request, response) => {
    const { pathname } = new URL(request.url ?? "/", "http://localhost");
    const name = pathname.startsWith("/assets/") ? pathname.slice(1) : "index.html";
    try {
      const body = pathname === "/sw.js" ? current : await readFile(join(DIST_DIR, name));
      const type = pathname === "/sw.js" ? ".js" : extname(name);
      response
        .writeHead(200, {
          "content-type": TYPES[type] ?? "application/octet-stream",
          "cache-control": "no-cache",
        })
        .end(body);
    } catch {
      response.writeHead(404).end();
    }
  });
  const port = await listen(server);
  return {
    origin: `http://localhost:${port}`,
    update: () => {
      current = worker.replaceAll(id, LATER_BUILD);
    },
    stop: async () => {
      server.closeAllConnections();
      await new Promise((done) => server.close(done));
    },
  };
}

test("a table opens offline once one was used, and after an update", async ({ page }) => {
  const doc = "tables-offline";
  const build = await serveBuild();
  let running = true;
  const kept = () =>
    page.evaluate(async () => {
      const cache = await caches.open("reticula-optional");
      return (await cache.keys())
        .map(({ url }) => new URL(url).pathname.replace(/-.{6}\./, "."))
        .sort();
    });
  const used = [
    "/assets/reticula_table_wasm_bg.wasm",
    "/assets/table-worker.js",
    "/assets/tables.js",
  ];
  const worker = () => page.evaluate(() => window.__reticula?.swStatus());
  try {
    await page.goto(`${build.origin}/app?hooks&doc=${doc}`);
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await connect(page, doc);
    await needsTables(page);
    await expect.poll(async () => (await worker())?.controlled, { timeout: 15_000 }).toBe(true);
    await page.reload();
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await connect(page, doc);
    await ask(page, (client) => client.acquire());
    const { entry } = await ask(page, (client) => client.sample("sample", 5_000, 1));
    await expect.poll(kept).toEqual(expect.arrayContaining(used));

    // What an older build had is of no use to a later one, and goes.
    await page.evaluate(async () => {
      const cache = await caches.open("reticula-optional");
      await cache.put("/assets/table-worker-gone.js", new Response("old"));
    });
    await ask(page, (client) => client.close());
    build.update();
    await page.evaluate(() => window.__reticula?.checkSwUpdate());
    await expect.poll(async () => (await worker())?.waiting, { timeout: 15_000 }).toBe(true);
    const reloaded = page.waitForEvent("load");
    await page.evaluate(async () => {
      const registration = await navigator.serviceWorker.getRegistration();
      registration?.waiting?.postMessage({ kind: "SKIP_WAITING" });
    });
    await reloaded;
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await expect.poll(async () => (await worker())?.buildId).toBe(LATER_BUILD);
    await expect.poll(kept).not.toContain("/assets/table-worker-gone.js");
    await expect.poll(kept).toEqual(expect.arrayContaining(used));

    await build.stop();
    running = false;
    await page.reload();
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await connect(page, doc);
    expect(await ask(page, (client) => client.acquire())).toEqual([entry]);
    expect((await openTable(page, entry.id)).rows).toBe(5_000);
    expect(rowOf(await takePages(page, 2), 0)[0]).toBe("1");
  } finally {
    if (running) {
      await build.stop();
    }
  }
});
