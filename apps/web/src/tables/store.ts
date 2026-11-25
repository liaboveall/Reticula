import {
  type Catalog,
  checksum,
  newest,
  readSlots,
  SLOTS,
  type TableEntry,
  tablesDir,
} from "./catalog.ts";
import { TABLE_EXTENSION as EXTENSION } from "./delete.ts";
import { TableError } from "./failure.ts";
import { fileFailure, openStored, type StoredFile } from "./files.ts";

/** Files that nobody else should have open. */
const PATIENCE_MS = 500;

/** What is known of a file by its directory, or null when it has none. */
export type Inspect = (file: StoredFile) => { readonly rows: number; readonly cols: number } | null;

type Directory = FileSystemDirectoryHandle & {
  keys(): AsyncIterable<string>;
};

/**
 * The tables of a workbook in the browser's storage. Only whoever holds the lock
 * may have one.
 *
 * A table is there once the catalogue says so. A file that the catalogue does not
 * name is what is left of an import that did not get that far.
 */
export class TableStore {
  readonly #dir: Directory;
  #catalog: Catalog;
  /** The slot with the catalogue. */
  #slot: number;
  /** How long a file may be in use by a tab that had to give up the tables. */
  #patienceMs: number;
  #closed = false;

  private constructor(dir: Directory, catalog: Catalog, slot: number, patienceMs: number) {
    this.#dir = dir;
    this.#catalog = catalog;
    this.#slot = slot;
    this.#patienceMs = patienceMs;
  }

  static async open(
    docId: string,
    inspect: Inspect,
    patienceMs: number = PATIENCE_MS,
  ): Promise<TableStore> {
    const dir = (await tablesDir(docId, true)) as Directory;
    const slots = await readSlots(dir);
    const catalog = newest(slots);
    const store = new TableStore(
      dir,
      catalog ?? { generation: 0, tables: [] },
      Math.max(0, slots.indexOf(catalog)),
      patienceMs,
    );
    const names: string[] = [];
    for await (const name of dir.keys()) {
      names.push(name);
    }
    const files = names.filter((name) => name.endsWith(EXTENSION));
    if (!catalog) {
      // There is a catalogue before there is a file. Where it cannot be read,
      // whatever file is whole is a table.
      const found = await store.#find(files.sort(), inspect);
      await store.change(() => found);
      return store;
    }
    const known = new Set(catalog.tables.map(({ id }) => id + EXTENSION));
    for (const name of files) {
      if (!known.has(name)) {
        // One that is in use belongs to a tab that has yet to notice that the
        // tables were taken from it. It goes the next time.
        await store.#remove(name).catch(() => {});
      }
    }
    return store;
  }

  /** Nothing is changed any more: somebody else may have the tables. */
  close(): void {
    this.#closed = true;
  }

  get tables(): readonly TableEntry[] {
    return this.#catalog.tables;
  }

  /** The table with an identifier. */
  entry(id: string): TableEntry {
    const found = this.tables.find((table) => table.id === id);
    if (!found) {
      throw new TableError("notFound");
    }
    return found;
  }

  /** A name of a table, as close to the one wanted as no other table has. */
  freeName(wanted: string): string {
    const taken = new Set(this.tables.map(({ name }) => name.toLowerCase()));
    let name = wanted;
    for (let count = 2; taken.has(name.toLowerCase()); count++) {
      name = `${wanted} ${count}`;
    }
    return name;
  }

  /** Replaces the list of tables, for good. */
  async change(
    update: (tables: readonly TableEntry[]) => readonly TableEntry[],
  ): Promise<readonly TableEntry[]> {
    const catalog: Catalog = {
      generation: this.#catalog.generation + 1,
      tables: update(this.tables),
    };
    const slot = 1 - this.#slot;
    const json = JSON.stringify(catalog);
    const file = await openStored(this.#dir, SLOTS[slot] as string, true, this.#patienceMs);
    try {
      if (this.#closed) {
        throw new TableError("yielded");
      }
      file.restart();
      if (!file.append(new TextEncoder().encode(`${checksum(json)}\n${json}`))) {
        throw new TableError(file.failure ?? "io");
      }
      file.flush();
    } finally {
      file.close();
    }
    this.#catalog = catalog;
    this.#slot = slot;
    return catalog.tables;
  }

  /** A new file, which is not a table yet. */
  async create(id: string): Promise<StoredFile> {
    const file = await openStored(this.#dir, id + EXTENSION, true, PATIENCE_MS);
    file.restart();
    return file;
  }

  /** The file of a table. */
  async openFile(id: string): Promise<StoredFile> {
    const file = await openStored(
      this.#dir,
      this.entry(id).id + EXTENSION,
      false,
      this.#patienceMs,
    );
    // Whoever had the tables before has let go by now.
    this.#patienceMs = PATIENCE_MS;
    return file;
  }

  /** The file of a table for whoever reads it like any other file. */
  async blob(id: string): Promise<File> {
    const handle = await this.#dir.getFileHandle(this.entry(id).id + EXTENSION);
    return handle.getFile();
  }

  /** Adds a table, whose file is there. */
  async add(entry: TableEntry): Promise<void> {
    await this.change((tables) => [...tables, entry]);
  }

  /** Removes the file of something that did not become a table. */
  async discard(id: string): Promise<void> {
    await this.#remove(id + EXTENSION);
  }

  /** Deletes a table. */
  async delete(id: string): Promise<void> {
    this.entry(id);
    const unlist = () => this.change((tables) => tables.filter((table) => table.id !== id));
    let isUnlisted = true;
    try {
      await unlist();
    } catch (error) {
      // There may be no room for the catalogue until the file is gone.
      if (this.#closed || fileFailure(error).code !== "quota") {
        throw error;
      }
      isUnlisted = false;
    }
    await this.#remove(id + EXTENSION);
    if (!isUnlisted) {
      await unlist();
    }
  }

  /** Puts a table where so many others are before it. */
  async move(id: string, position: number): Promise<void> {
    const entry = this.entry(id);
    await this.change((tables) =>
      tables.filter((table) => table !== entry).toSpliced(position, 0, entry),
    );
  }

  async rename(id: string, name: string): Promise<void> {
    const wanted = name.trim();
    this.entry(id);
    if (wanted === "" || wanted.length > 255) {
      throw new TableError("invalidName");
    }
    if (
      this.tables.some(
        (table) => table.id !== id && table.name.toLowerCase() === wanted.toLowerCase(),
      )
    ) {
      throw new TableError("duplicateName");
    }
    await this.change((tables) =>
      tables.map((table) => (table.id === id ? { ...table, name: wanted } : table)),
    );
  }

  async #find(files: readonly string[], inspect: Inspect): Promise<TableEntry[]> {
    const found: TableEntry[] = [];
    for (const name of files) {
      const file = await openStored(this.#dir, name, false, this.#patienceMs);
      try {
        const size = inspect(file);
        if (size) {
          found.push({
            id: name.slice(0, -EXTENSION.length),
            name: `Table ${found.length + 1}`,
            rows: size.rows,
            cols: size.cols,
            bytes: file.size,
          });
        }
      } finally {
        file.close();
      }
    }
    return found;
  }

  async #remove(name: string): Promise<void> {
    try {
      await this.#dir.removeEntry(name);
    } catch (error) {
      if ((error as Error).name !== "NotFoundError") {
        throw error;
      }
    }
  }
}
