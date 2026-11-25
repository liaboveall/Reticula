// The browser's file system and locks, for tests.

import type { SyncAccessHandleLike } from "../engine/storage.ts";

export function fault(name: string): DOMException {
  return new DOMException(name, name);
}

interface FakeFile {
  bytes: Uint8Array<ArrayBuffer>;
  open: boolean;
  modified: number;
}

export class FakeDirectory {
  readonly files = new Map<string, FakeFile>();
  readonly directories = new Map<string, FakeDirectory>();
  /** Bytes that can still be written, anywhere. */
  static room = Number.POSITIVE_INFINITY;
  /** How often something was written. */
  static writes = 0;

  async getDirectoryHandle(name: string, options?: { create?: boolean }): Promise<FakeDirectory> {
    let found = this.directories.get(name);
    if (!found) {
      if (!options?.create) {
        throw fault("NotFoundError");
      }
      found = new FakeDirectory();
      this.directories.set(name, found);
    }
    return found;
  }

  async getFileHandle(name: string, options?: { create?: boolean }) {
    if (!this.files.has(name)) {
      if (!options?.create) {
        throw fault("NotFoundError");
      }
      this.files.set(name, { bytes: new Uint8Array(), open: false, modified: 0 });
    }
    const file = this.files.get(name) as FakeFile;
    return {
      getFile: async () => new File([file.bytes], name, { lastModified: file.modified }),
      createSyncAccessHandle: async () => openFake(file),
      createWritable: async () => {
        let text = "";
        return {
          write: async (more: string) => {
            text += more;
          },
          close: async () => {
            if (text.length > FakeDirectory.room) {
              throw fault("QuotaExceededError");
            }
            this.put(name, text);
          },
        };
      },
    };
  }

  /** Whether a file here, or further down, is open. */
  get isInUse(): boolean {
    return (
      [...this.files.values()].some(({ open }) => open) ||
      [...this.directories.values()].some(({ isInUse }) => isInUse)
    );
  }

  async removeEntry(name: string): Promise<void> {
    if (this.files.get(name)?.open || this.directories.get(name)?.isInUse) {
      throw fault("NoModificationAllowedError");
    }
    FakeDirectory.room += this.files.get(name)?.bytes.length ?? 0;
    if (!this.files.delete(name) && !this.directories.delete(name)) {
      throw fault("NotFoundError");
    }
  }

  async *keys(): AsyncGenerator<string> {
    yield* [...this.files.keys(), ...this.directories.keys()];
  }

  /** A file by its name. */
  text(name: string): string | undefined {
    const file = this.files.get(name);
    return file && new TextDecoder().decode(file.bytes);
  }

  put(name: string, text: string): void {
    this.putBytes(name, new TextEncoder().encode(text));
  }

  putBytes(name: string, bytes: Uint8Array<ArrayBuffer>): void {
    this.files.set(name, { bytes, open: false, modified: ++FakeDirectory.writes });
  }

  /** A directory further down, which is made when there is none. */
  at(...names: string[]): FakeDirectory {
    let dir: FakeDirectory = this;
    for (const name of names) {
      const next = dir.directories.get(name) ?? new FakeDirectory();
      dir.directories.set(name, next);
      dir = next;
    }
    return dir;
  }
}

function openFake(file: FakeFile): SyncAccessHandleLike {
  if (file.open) {
    throw fault("NoModificationAllowedError");
  }
  file.open = true;
  return {
    getSize: () => file.bytes.length,
    read: (buffer, options) => {
      const part = file.bytes.subarray(options?.at ?? 0).subarray(0, buffer.length);
      buffer.set(part);
      return part.length;
    },
    write: (buffer, options) => {
      if (buffer.length > FakeDirectory.room) {
        throw fault("QuotaExceededError");
      }
      FakeDirectory.room -= buffer.length;
      file.modified = ++FakeDirectory.writes;
      const at = options?.at ?? 0;
      const grown = new Uint8Array(Math.max(file.bytes.length, at + buffer.length));
      grown.set(file.bytes);
      grown.set(buffer, at);
      file.bytes = grown;
      return buffer.length;
    },
    truncate: (size) => {
      file.bytes = file.bytes.slice(0, size);
    },
    flush: () => {},
    close: () => {
      file.open = false;
    },
  };
}

interface Held {
  readonly end: (error?: unknown) => void;
}

/** Locks as several tabs have them. */
export class FakeLocks {
  readonly #held = new Map<string, Held>();
  readonly #queue = new Map<string, (() => void)[]>();

  async request<T>(
    name: string,
    given: LockOptions | ((lock: object | null) => T | Promise<T>),
    then?: (lock: object | null) => T | Promise<T>,
  ): Promise<T> {
    const options = typeof given === "function" ? {} : given;
    const run = (typeof given === "function" ? given : then) as NonNullable<typeof then>;
    if (options.steal) {
      this.#held.get(name)?.end(fault("AbortError"));
      this.#held.delete(name);
    }
    if (this.#held.has(name)) {
      if (options.ifAvailable) {
        return run(null);
      }
      await new Promise<void>((resolve, reject) => {
        this.#queue.set(name, [...(this.#queue.get(name) ?? []), resolve]);
        options.signal?.addEventListener("abort", () => reject(options.signal?.reason));
      });
    }
    let held: Held | undefined;
    const stolen = new Promise<never>((_, reject) => {
      held = { end: reject };
    });
    this.#held.set(name, held as Held);
    try {
      return await Promise.race([run({}), stolen]);
    } finally {
      if (this.#held.get(name) === held) {
        this.#held.delete(name);
        const [next, ...rest] = this.#queue.get(name) ?? [];
        this.#queue.set(name, rest);
        next?.();
      }
    }
  }

  holds(name: string): boolean {
    return this.#held.has(name);
  }
}

/** A channel whose messages arrive at once, at all others of its name. */
export class FakeChannel extends EventTarget {
  static all = new Set<FakeChannel>();

  constructor() {
    super();
    FakeChannel.all.add(this);
  }

  close(): void {
    FakeChannel.all.delete(this);
  }

  postMessage(data: unknown): void {
    for (const other of FakeChannel.all) {
      if (other !== this) {
        queueMicrotask(() => other.dispatchEvent(new MessageEvent("message", { data })));
      }
    }
  }
}
