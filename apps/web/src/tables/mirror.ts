import { copiesDir, namesIn, remove } from "./delete.ts";
import { openStored, type StoredFile } from "./files.ts";

/** What the copies of all tables may take. */
const MOST_COPIED = 1 << 30;

/** A copy of a table on the server, or of a part of it, that is kept on the disk. */
export interface Mirror {
  readonly file: StoredFile;
  /** Closes it, and deletes it. */
  remove(): Promise<unknown>;
}

/** Deletes the copies that were used longest ago, until there is room for another one. */
export async function makeRoom(
  dir: FileSystemDirectoryHandle,
  name: string,
  bytes: number,
  most: number = MOST_COPIED,
): Promise<void> {
  const files: File[] = [];
  for (const other of await namesIn(dir)) {
    if (other !== name && !other.endsWith(".json")) {
      files.push(await (await dir.getFileHandle(other)).getFile());
    }
  }
  let total = files.reduce((sum, { size }) => sum + size, bytes);
  for (const file of files.sort((a, b) => a.lastModified - b.lastModified)) {
    // One that is in use stays.
    if (total > most && (await remove(dir, file.name))) {
      total -= file.size;
    }
  }
}

/** The copy of a table. Null when there cannot be one, or another tab has it. */
export async function openMirror(
  userId: string,
  sha256: string,
  bytes: number,
): Promise<Mirror | null> {
  try {
    const dir = await copiesDir(userId, true);
    await makeRoom(dir, sha256, bytes);
    const file = await openStored(dir, sha256, true, 0);
    file.touch();
    return {
      file,
      remove: () => {
        file.close();
        return remove(dir, sha256);
      },
    };
  } catch {
    return null;
  }
}
