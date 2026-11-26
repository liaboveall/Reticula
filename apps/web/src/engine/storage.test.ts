import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import {
  RECOVERED_TITLE,
  readCatalog,
  removeDocStorage,
  upsertCatalogEntry,
  type WorkbookCatalogEntry,
  writeDocState,
} from "./storage.ts";

// Storage that takes its time, as that of a browser does: whoever reads and then
// writes has others in between, unless there is a lock.

const pause = () => new Promise<void>((resolve) => setTimeout(resolve, 1));

const missing = () => Object.assign(new Error("no such file"), { name: "NotFoundError" });

class FakeDirectory {
  readonly files = new Map<string, Uint8Array>();
  readonly directories = new Map<string, FakeDirectory>();

  async getDirectoryHandle(name: string, options?: { create?: boolean }): Promise<FakeDirectory> {
    let found = this.directories.get(name);
    if (!found) {
      if (!options?.create) {
        throw missing();
      }
      found = new FakeDirectory();
      this.directories.set(name, found);
    }
    return found;
  }

  async getFileHandle(name: string, options?: { create?: boolean }) {
    const { files } = this;
    if (!files.has(name)) {
      if (!options?.create) {
        throw missing();
      }
      files.set(name, new Uint8Array());
    }
    return {
      getFile: async () => {
        await pause();
        const bytes = files.get(name) ?? new Uint8Array();
        return {
          size: bytes.byteLength,
          lastModified: 1_000,
          arrayBuffer: async () => bytes.slice().buffer,
        };
      },
      createWritable: async () => {
        let written = new Uint8Array();
        return {
          write: async (bytes: Uint8Array) => {
            await pause();
            written = bytes.slice();
          },
          close: async () => {
            files.set(name, written);
          },
        };
      },
    };
  }

  async *keys(): AsyncIterable<string> {
    yield* [...this.files.keys(), ...this.directories.keys()];
  }

  async removeEntry(name: string): Promise<void> {
    if (!this.directories.delete(name)) {
      throw missing();
    }
  }
}

/** Locks that are waited for in turn. */
class FakeLocks {
  readonly names: string[] = [];
  #turns = new Map<string, Promise<unknown>>();

  request<T>(name: string, run: () => Promise<T>): Promise<T> {
    this.names.push(name);
    const turn = (this.#turns.get(name) ?? Promise.resolve()).then(run, run);
    this.#turns.set(name, turn);
    return turn;
  }
}

const entry = (id: string, updatedAt: number, title = id): WorkbookCatalogEntry => ({
  id,
  title,
  createdAt: updatedAt,
  updatedAt,
  sizeBytes: 1,
});

let locks: FakeLocks;
let root: FakeDirectory;

beforeEach(() => {
  root = new FakeDirectory();
  locks = new FakeLocks();
  vi.stubGlobal("navigator", { storage: { getDirectory: async () => root }, locks });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const ids = async () => (await readCatalog("opfs-writable")).map(({ id }) => id);

describe("the list of workbooks", () => {
  test("keeps the workbooks that two tabs make at the same time", async () => {
    await Promise.all([
      upsertCatalogEntry(entry("a", 1), "opfs-writable"),
      upsertCatalogEntry(entry("b", 2), "opfs-writable"),
      upsertCatalogEntry(entry("c", 3), "opfs-writable"),
    ]);
    expect(await ids()).toEqual(["c", "b", "a"]);
  });

  test("keeps what a tab makes while another one deletes", async () => {
    await upsertCatalogEntry(entry("a", 1), "opfs-writable");
    await Promise.all([
      removeDocStorage("a", "opfs-writable"),
      upsertCatalogEntry(entry("b", 2), "opfs-writable"),
    ]);
    expect(await ids()).toEqual(["b"]);
  });

  test("keeps when a workbook was made as it changes", async () => {
    await upsertCatalogEntry(entry("a", 1), "opfs-writable");
    const list = await upsertCatalogEntry(entry("a", 5, "A"), "opfs-writable");
    expect(list).toEqual([{ ...entry("a", 5, "A"), createdAt: 1 }]);
    expect(await readCatalog("opfs-writable")).toEqual(list);
  });

  test("goes on to be changed after a change has failed", async () => {
    const write = vi.spyOn(FakeDirectory.prototype, "getFileHandle");
    write.mockRejectedValueOnce(new Error("the disk is gone"));
    await expect(upsertCatalogEntry(entry("a", 1), "opfs-writable")).rejects.toThrow("disk");
    write.mockRestore();
    await upsertCatalogEntry(entry("b", 2), "opfs-writable");
    expect(await ids()).toEqual(["b"]);
  });

  test("is changed without locks where there are none", async () => {
    vi.stubGlobal("navigator", { storage: { getDirectory: async () => root } });
    await upsertCatalogEntry(entry("a", 1), "opfs-writable");
    await removeDocStorage("a", "opfs-writable");
    expect(await ids()).toEqual([]);
  });
});

describe("a list of workbooks that a write left damaged", () => {
  const top = async () => root.getDirectoryHandle("reticula-v1");
  const damage = async (name: string) => {
    const bytes = (await top()).files.get(name);
    expect(bytes).toBeDefined();
    (await top()).files.set(name, bytes?.slice(0, 7) ?? new Uint8Array());
  };
  const state = { activeSlot: "a", snapshotSeq: 0, lastFlushedSeq: 0, openingCount: 0 } as const;

  test("is read from its spare, and repairs the main copy before touching the spare", async () => {
    await upsertCatalogEntry(entry("a", 1, "Alpha"), "opfs-writable");
    await upsertCatalogEntry(entry("b", 2, "Beta"), "opfs-writable");
    await damage("catalog.json");
    expect(await ids()).toEqual(["b", "a"]);

    const dir = await top();
    const order: string[] = [];
    const origGetFileHandle = dir.getFileHandle.bind(dir);
    dir.getFileHandle = async (name: string, options?: { create?: boolean }) => {
      const handle = await origGetFileHandle(name, options);
      return {
        ...handle,
        createWritable: async () => {
          order.push(name);
          return handle.createWritable();
        },
      };
    };
    await upsertCatalogEntry(entry("c", 3, "Gamma"), "opfs-writable");
    expect(order).toEqual(["catalog.json", "catalog.spare.json"]);
    expect((await readCatalog("opfs-writable")).map(({ title }) => title)).toEqual([
      "Gamma",
      "Beta",
      "Alpha",
    ]);
  });

  test("is made again from the workbooks that are there", async () => {
    for (const [id, at] of [
      ["a", 1],
      ["b", 2],
    ] as const) {
      await upsertCatalogEntry(entry(id, at), "opfs-writable");
      await writeDocState(id, state, "opfs-writable");
    }
    await damage("catalog.json");
    await damage("catalog.spare.json");
    const list = await readCatalog("opfs-writable");
    expect(list.map(({ id }) => id).sort()).toEqual(["a", "b"]);
    expect(list.every(({ title }) => title === RECOVERED_TITLE)).toBe(true);
    // What is made next does not take the place of what was there.
    await writeDocState("c", state, "opfs-writable");
    await upsertCatalogEntry(entry("c", 5_000, "New"), "opfs-writable");
    expect((await ids()).sort()).toEqual(["a", "b", "c"]);
    const made = (await readCatalog("opfs-writable")).find(({ id }) => id === "c");
    expect(made).toMatchObject({ title: "New", updatedAt: 5_000 });
  });

  test("is not made again while it is whole", async () => {
    // A workbook that is being made has its files before it is on the list.
    await writeDocState("a", state, "opfs-writable");
    await upsertCatalogEntry(entry("a", 1, "Mine"), "opfs-writable");
    await writeDocState("b", state, "opfs-writable");
    expect(await readCatalog("opfs-writable")).toEqual([entry("a", 1, "Mine")]);
    await damage("catalog.json");
    expect(await readCatalog("opfs-writable")).toEqual([entry("a", 1, "Mine")]);
  });

  test("is no list of workbooks where there are none", async () => {
    expect(await readCatalog("opfs-writable")).toEqual([]);
    await (await root.getDirectoryHandle("reticula-v1", { create: true })).getFileHandle(
      "catalog.json",
      { create: true },
    );
    expect(await readCatalog("opfs-writable")).toEqual([]);
  });
});
