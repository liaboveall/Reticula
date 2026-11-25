import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readCatalog, type TableEntry } from "./catalog.ts";
import { FakeDirectory } from "./fake-storage.ts";
import { TableStore } from "./store.ts";

let root: FakeDirectory;

beforeEach(() => {
  root = new FakeDirectory();
  FakeDirectory.room = Number.POSITIVE_INFINITY;
  vi.stubGlobal("navigator", { storage: { getDirectory: async () => root } });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

/** Files that begin with a number are tables of so many rows. */
const inspect = (file: { readAt(at: number, into: Uint8Array): number }) => {
  const head = new Uint8Array(1);
  return file.readAt(0, head) === 0 && head[0] !== undefined && head[0] > 0x30 && head[0] < 0x3a
    ? { rows: head[0] - 0x30, cols: 2 }
    : null;
};

const open = () => TableStore.open("doc", inspect);

async function directory(): Promise<FakeDirectory> {
  const top = await root.getDirectoryHandle("reticula-v1");
  return (await top.getDirectoryHandle("tables")).getDirectoryHandle("doc");
}

async function add(store: TableStore, id: string, name = id): Promise<void> {
  const file = await store.create(id);
  file.append(new TextEncoder().encode("7 rows"));
  file.close();
  const entry: TableEntry = { id, name: store.freeName(name), rows: 7, cols: 2, bytes: 6 };
  await store.change((tables) => [...tables, entry]);
}

const names = (store: TableStore) => store.tables.map(({ name }) => name);

describe("the tables in the browser's storage", () => {
  it("are there again when they are opened again", async () => {
    const store = await open();
    expect(store.tables).toEqual([]);
    await add(store, "a");
    await add(store, "b");
    expect(names(await open())).toEqual(["a", "b"]);
    expect((await readCatalog("doc"))?.map(({ id }) => id)).toEqual(["a", "b"]);
  });

  it("are written to the two slots in turn", async () => {
    const store = await open();
    const dir = await directory();
    const before = [dir.text("catalog-a"), dir.text("catalog-b")];
    await add(store, "a");
    const after = [dir.text("catalog-a"), dir.text("catalog-b")];
    expect(after.filter((text, slot) => text !== before[slot])).toHaveLength(1);
    // Whichever was written is torn: what was there before is.
    const changed = after[0] === before[0] ? "catalog-b" : "catalog-a";
    dir.put(changed, (dir.text(changed) ?? "").slice(0, -3));
    expect(await readCatalog("doc")).toEqual([]);
  });

  it("do not include what an import left behind, which goes", async () => {
    const store = await open();
    await add(store, "a");
    const file = await store.create("half");
    file.append(new TextEncoder().encode("7"));
    file.close();
    expect(names(await open())).toEqual(["a"]);
    expect((await directory()).files.has("half.rtbl")).toBe(false);
    expect((await directory()).files.has("a.rtbl")).toBe(true);
  });

  it("open even while another tab still has such a file in use", async () => {
    const store = await open();
    await add(store, "a");
    await store.create("half");
    expect(names(await open())).toEqual(["a"]);
    expect((await directory()).files.has("half.rtbl")).toBe(true);
  });

  it("are found by their files when both slots are damaged", async () => {
    const store = await open();
    await add(store, "a", "Orders");
    await add(store, "b", "Shops");
    const dir = await directory();
    dir.put("broken.rtbl", "none");
    dir.put("catalog-a", "nonsense");
    dir.put("catalog-b", "");
    const found = await open();
    expect(found.tables).toEqual([
      { id: "a", name: "Table 1", rows: 7, cols: 2, bytes: 6 },
      { id: "b", name: "Table 2", rows: 7, cols: 2, bytes: 6 },
    ]);
    expect(dir.files.has("broken.rtbl")).toBe(true);
    expect((await readCatalog("doc"))?.length).toBe(2);
  });

  it("are renamed, put into an order and deleted", async () => {
    const store = await open();
    await add(store, "a");
    await add(store, "b");
    await store.rename("a", "  Orders ");
    await add(store, "c");
    await store.move("a", 1);
    expect(names(store)).toEqual(["b", "Orders", "c"]);
    await store.move("c", 0);
    expect(names(store)).toEqual(["c", "b", "Orders"]);
    await store.move("c", 9);
    expect(names(store)).toEqual(["b", "Orders", "c"]);
    await store.delete("c");
    await expect(store.rename("b", "ORDERS")).rejects.toMatchObject({ code: "duplicateName" });
    await expect(store.rename("b", " ")).rejects.toMatchObject({ code: "invalidName" });
    await expect(store.rename("c", "x")).rejects.toMatchObject({ code: "notFound" });
    await expect(store.move("c", 0)).rejects.toMatchObject({ code: "notFound" });
    await store.delete("b");
    expect(names(await open())).toEqual(["Orders"]);
    expect((await directory()).files.has("b.rtbl")).toBe(false);
  });

  it("have names of their own", async () => {
    const store = await open();
    await add(store, "a", "Orders");
    await add(store, "b", "orders");
    await add(store, "c", "Orders");
    expect(names(store)).toEqual(["Orders", "orders 2", "Orders 3"]);
  });

  it("stay as they were when there is no room for the catalogue", async () => {
    const store = await open();
    await add(store, "a");
    FakeDirectory.room = 0;
    await expect(store.rename("a", "b")).rejects.toMatchObject({ code: "quota" });
    expect(names(store)).toEqual(["a"]);
    FakeDirectory.room = Number.POSITIVE_INFINITY;
    expect(names(await open())).toEqual(["a"]);
  });

  it("can be deleted when there is no room for the catalogue", async () => {
    const store = await open();
    await add(store, "a");
    await add(store, "b");
    // Room comes with the file that goes.
    FakeDirectory.room = 0;
    const real = FakeDirectory.prototype.removeEntry;
    const removed = vi.spyOn(FakeDirectory.prototype, "removeEntry");
    removed.mockImplementation(function (this: FakeDirectory, name: string) {
      FakeDirectory.room = Number.POSITIVE_INFINITY;
      return real.call(this, name);
    });
    await store.delete("a");
    expect(names(store)).toEqual(["b"]);
    expect((await directory()).files.has("a.rtbl")).toBe(false);
    expect(names(await open())).toEqual(["b"]);

    // It is said when there is no room even then, and the table is gone for good.
    removed.mockImplementation(function (this: FakeDirectory, name: string) {
      return real.call(this, name);
    });
    FakeDirectory.room = 0;
    await expect(store.delete("b")).rejects.toMatchObject({ code: "quota" });
    expect((await directory()).files.has("b.rtbl")).toBe(false);
    removed.mockRestore();
  });

  it("are not changed by one that no longer has them", async () => {
    const store = await open();
    await add(store, "a");
    store.close();
    await expect(store.rename("a", "b")).rejects.toMatchObject({ code: "yielded" });
    expect(names(await open())).toEqual(["a"]);
  });

  it("wait for a file that is being let go of", async () => {
    const store = await open();
    await add(store, "a");
    const held = await store.openFile("a");
    const other = await TableStore.open("doc", inspect, 1_000);
    const pending = other.openFile("a");
    setTimeout(() => held.close(), 60);
    (await pending).close();
    const again = await store.openFile("a");
    await expect(
      TableStore.open("doc", inspect, 0).then((late) => late.openFile("a")),
    ).rejects.toMatchObject({ name: "NoModificationAllowedError" });
    again.close();
  });

  it("are handed out as files", async () => {
    const store = await open();
    await add(store, "a");
    expect(await (await store.blob("a")).text()).toBe("7 rows");
    await expect(store.blob("b")).rejects.toMatchObject({ code: "notFound" });
  });
});
