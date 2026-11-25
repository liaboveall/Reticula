import { t } from "./i18n/index.ts";
import { readCatalog, tablesDir } from "./tables/catalog.ts";
import { giveUp, queuedUploads } from "./tables/delete.ts";

/** How many data tables a workbook has in this browser, and what they take. */
export interface TableSize {
  readonly count: number;
  /** With what is left of imports that were cut short. */
  readonly bytes: number;
}

async function tableSize(docId: string): Promise<TableSize> {
  const dir = await tablesDir(docId).catch(() => null);
  let bytes = 0;
  if (dir) {
    for await (const [name, handle] of dir as unknown as AsyncIterable<
      [string, FileSystemHandle]
    >) {
      if (name.endsWith(".rtbl")) {
        bytes += (await (handle as FileSystemFileHandle).getFile()).size;
      }
    }
  }
  return { count: (await readCatalog(docId))?.length ?? 0, bytes };
}

/** The same for several workbooks. */
export async function tableSizes(
  docs: readonly { readonly id: string }[],
): Promise<ReadonlyMap<string, TableSize>> {
  const none = { count: 0, bytes: 0 };
  return new Map(
    await Promise.all(
      docs.map(async ({ id }) => [id, await tableSize(id).catch(() => none)] as const),
    ),
  );
}

export const uploadsReason = () =>
  t("Data tables of this workbook are still being uploaded. Delete it again to give that up.");

/** The workbook whose uploads somebody was told of. */
let told = "";

/**
 * Whether tables of a workbook that is to be deleted are still to be uploaded. Asked
 * once more, which is after that was told, it gives the uploads up.
 */
export async function keepsUploads(docId: string): Promise<boolean> {
  const uploads = (await queuedUploads()).filter(({ from }) => from?.docId === docId);
  const isNews = uploads.length > 0 && told !== docId;
  told = isNews ? docId : "";
  if (!isNews) {
    await Promise.all(uploads.map((upload) => giveUp(upload.docId, upload.entry.id)));
  }
  return isNews;
}
