import {
  deleteWorkbook,
  importWorkbook,
  me,
  type WorkbookKey,
} from "@reticula/protocol/collab-api";
import { readCatalog, type TableEntry } from "../tables/catalog.ts";
import { dequeue, enqueue } from "../tables/delete.ts";
import { startEngine } from "./client.ts";

/** Puts a workbook in this browser on the server, where its data tables go in their time. */
export async function uploadLocalDocToCloud(docId: string, title: string): Promise<WorkbookKey> {
  // What may fail does before there is a workbook.
  const { id: userId } = await me();
  const tables = await readCatalog(docId);
  // Opening the workbook repairs it.
  if (!tables) {
    throw new Error(`Workbook "${docId}" has to be opened once to repair its data tables.`);
  }
  const bytes = await exportLocalDoc(docId);
  const key = await importWorkbook(encodeURIComponent(title), new Uint8Array(bytes));
  try {
    await queueTables(userId, docId, key.id, tables);
  } catch (error) {
    // Not without its tables: it would be there once more the next time.
    await deleteWorkbook(key.id).catch(() => {});
    throw error;
  }
  return key;
}

function exportLocalDoc(docId: string): Promise<ArrayBuffer> {
  return new Promise((resolve, reject) => {
    let requestedExport = false;
    const fail = (reason: string) => {
      client.stop();
      reject(new Error(reason));
    };
    const client = startEngine({
      initialDocId: docId,
      onPage: () => {},
      onMessage: (msg) => {
        switch (msg.kind) {
          case "ready":
            if (!requestedExport) {
              requestedExport = true;
              client.sendHost({ kind: "exportRtcl" });
            }
            break;
          case "fileExported":
            if (msg.format === "rtcl") {
              client.stop();
              resolve(msg.bytes);
            }
            break;
          case "docBlocked":
            fail(`Workbook "${docId}" is currently open in another browser tab.`);
            break;
          case "safeMode":
            fail(`Workbook "${docId}" requires Safe Mode recovery before uploading to cloud.`);
            break;
          case "rejected":
            fail(msg.reason);
            break;
          case "failed":
            fail(msg.message);
            break;
        }
      },
    });
  });
}

/**
 * Has the data tables of a workbook in this browser uploaded to one on the server,
 * where they are others from then on. Those here stay. All of them are, or none.
 */
async function queueTables(
  userId: string,
  docId: string,
  cloudId: string,
  tables: readonly TableEntry[],
): Promise<void> {
  const queuedAt = Date.now();
  const ids = tables.map(() => crypto.randomUUID());
  try {
    await Promise.all(
      tables.map((table, at) =>
        enqueue({
          userId,
          docId: cloudId,
          entry: { ...table, id: ids[at] as string },
          sha256: "",
          // In the order they have here.
          queuedAt: queuedAt + at,
          from: { docId, id: table.id },
        }),
      ),
    );
  } catch (error) {
    await Promise.all(ids.map(dequeue));
    throw error;
  }
}
