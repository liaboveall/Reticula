import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import {
  readCatalog,
  removeDocStorage,
  upsertCatalogEntry,
  type WorkbookCatalogEntry,
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
        return { arrayBuffer: async () => (files.get(name) ?? new Uint8Array()).slice().buffer };
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
