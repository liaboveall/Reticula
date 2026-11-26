import {
  errorName,
  getRootDir,
  LOCAL_DIR,
  listMemoryKeys,
  RECOVERED_TITLE,
  readMemoryFile,
  type StorageBackendKind,
  type WorkbookCatalogEntry,
} from "./storage.ts";

/** The files a workbook has, of which at least one is there. */
const DOC_FILES = ["state.json", "wal.log", "snapshot-a.bin", "snapshot-b.bin"];

const found = (id: string, sizeBytes: number, updatedAt: number): WorkbookCatalogEntry => ({
  id,
  title: RECOVERED_TITLE,
  createdAt: updatedAt,
  updatedAt,
  sizeBytes,
});

/**
 * The workbooks that have files in this browser, for a list of them that was
 * lost. Their titles are only in the list, so they are found as {@link RECOVERED_TITLE}.
 */
export async function findDocs(backend: StorageBackendKind): Promise<WorkbookCatalogEntry[]> {
  if (backend === "memory") {
    const sizes = new Map<string, number>();
    for (const key of listMemoryKeys(`${LOCAL_DIR}/`)) {
      const id = key.slice(LOCAL_DIR.length + 1).split("/")[0];
      if (id) {
        sizes.set(id, (sizes.get(id) ?? 0) + (readMemoryFile(key)?.byteLength ?? 0));
      }
    }
    const now = Date.now();
    return [...sizes].map(([id, size]) => found(id, size, now));
  }
  const root = await getRootDir(true);
  let local: FileSystemDirectoryHandle;
  try {
    local = await root.getDirectoryHandle(LOCAL_DIR, { create: false });
  } catch (err) {
    if (errorName(err) === "NotFoundError") {
      return [];
    }
    throw err;
  }
  const docs: WorkbookCatalogEntry[] = [];
  for await (const id of (local as unknown as { keys(): AsyncIterable<string> }).keys()) {
    let dir: FileSystemDirectoryHandle;
    try {
      dir = await local.getDirectoryHandle(id, { create: false });
    } catch {
      continue; // A file, or a workbook that went meanwhile.
    }
    let sizeBytes = 0;
    let updatedAt = 0;
    let files = 0;
    for (const name of DOC_FILES) {
      try {
        const file = await (await dir.getFileHandle(name, { create: false })).getFile();
        files += 1;
        sizeBytes += file.size ?? 0;
        updatedAt = Math.max(updatedAt, file.lastModified ?? 0);
      } catch {
        // Not every workbook has every file.
      }
    }
    if (files > 0) {
      docs.push(found(id, sizeBytes, updatedAt || Date.now()));
    }
  }
  return docs;
}
