import { importWorkbook, type WorkbookKey } from "@reticula/protocol/collab-api";
import { startEngine } from "./client.ts";

export async function uploadLocalDocToCloud(docId: string, title: string): Promise<WorkbookKey> {
  const bytes = await new Promise<ArrayBuffer>((resolve, reject) => {
    let requestedExport = false;
    const client = startEngine({
      initialDocId: docId,
      onPage: () => {},
      onMessage: (msg) => {
        switch (msg.kind) {
          case "ready": {
            if (!requestedExport) {
              requestedExport = true;
              client.sendHost({ kind: "exportRtcl" });
            }
            break;
          }
          case "fileExported": {
            if (msg.format === "rtcl") {
              client.stop();
              resolve(msg.bytes);
            }
            break;
          }
          case "docBlocked": {
            client.stop();
            reject(new Error(`Workbook "${docId}" is currently open in another browser tab.`));
            break;
          }
          case "safeMode": {
            client.stop();
            reject(
              new Error(
                `Workbook "${docId}" requires Safe Mode recovery before uploading to cloud.`,
              ),
            );
            break;
          }
          case "rejected": {
            client.stop();
            reject(new Error(msg.reason));
            break;
          }
          case "failed": {
            client.stop();
            reject(new Error(msg.message));
            break;
          }
        }
      },
    });
  });
  return importWorkbook(encodeURIComponent(title), new Uint8Array(bytes));
}
