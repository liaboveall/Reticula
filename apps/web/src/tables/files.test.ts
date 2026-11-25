import { describe, expect, it } from "vitest";
import type { SyncAccessHandleLike } from "../engine/storage.ts";
import { FakeDirectory, fault } from "./fake-storage.ts";
import { fileFailure, openStored, StoredFile } from "./files.ts";

const directory = () => new FakeDirectory() as unknown as FileSystemDirectoryHandle;

const bytes = (text: string) => new TextEncoder().encode(text);

describe("a file in the browser's storage", () => {
  it("is written at its end and read anywhere", async () => {
    const file = await openStored(directory(), "x", true, 0);
    expect(file.append(bytes("abcdef"))).toBe(true);
    expect(file.append(bytes("gh"))).toBe(true);
    expect(file.size).toBe(8);
    const into = new Uint8Array(3);
    expect(file.readAt(4, into)).toBe(0);
    expect(new TextDecoder().decode(into)).toBe("efg");
    expect(file.readAt(6, into)).toBe(2);
    expect(file.failure).toMatchObject({ code: "corrupt" });
    file.restart();
    expect(file.size).toBe(0);
    file.close();
    file.close();
  });

  it("is read and written in as many pieces as it takes", () => {
    const stored: number[] = [];
    const handle = {
      getSize: () => 0,
      write: (part: Uint8Array) => {
        stored.push(part[0] ?? 0);
        return 1;
      },
      read: (part: Uint8Array, { at }: { at: number }) => {
        part[0] = at;
        return 1;
      },
    } as unknown as SyncAccessHandleLike;
    const file = new StoredFile(handle);
    expect(file.append(bytes("abc"))).toBe(true);
    expect(String.fromCharCode(...stored)).toBe("abc");
    const into = new Uint8Array(4);
    expect(file.readAt(5, into)).toBe(0);
    expect([...into]).toEqual([5, 6, 7, 8]);
  });

  it("says when there is no room for more", async () => {
    const file = await openStored(directory(), "x", true, 0);
    FakeDirectory.room = 4;
    expect(file.append(bytes("abc"))).toBe(true);
    expect(file.append(bytes("de"))).toBe(false);
    expect(file.failure).toMatchObject({ code: "quota" });
    FakeDirectory.room = Number.POSITIVE_INFINITY;
    const silent = { getSize: () => 0, write: () => 0 } as unknown as SyncAccessHandleLike;
    const other = new StoredFile(silent);
    expect(other.append(bytes("a"))).toBe(false);
    expect(other.failure).toMatchObject({ code: "quota" });
  });

  it("cannot be had where there are no synchronous handles", async () => {
    const dir = { getFileHandle: async () => ({}) } as unknown as FileSystemDirectoryHandle;
    await expect(openStored(dir, "x", true, 0)).rejects.toMatchObject({ code: "unsupported" });
  });
});

describe("what exceptions of files mean", () => {
  it.each([
    ["QuotaExceededError", "quota"],
    ["NoModificationAllowedError", "inUse"],
    ["InvalidStateError", "inUse"],
    ["NotFoundError", "notFound"],
    ["NotReadableError", "changed"],
    ["UnknownError", "io"],
  ])("%s", (name, code) => {
    expect(fileFailure(fault(name)).code).toBe(code);
  });
});
