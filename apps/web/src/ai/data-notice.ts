/** Whether whoever uses this browser has been told what a cloud run sends, and to whom. */

const KEY = "reticula:ai-data-notice";

export function hasSeenAiDataNotice(): boolean {
  try {
    return localStorage.getItem(KEY) === "1";
  } catch {
    return false;
  }
}

export function noteAiDataNotice(): void {
  try {
    localStorage.setItem(KEY, "1");
  } catch {
    // It is asked again next time.
  }
}
