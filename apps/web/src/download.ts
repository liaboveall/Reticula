/** Has the browser save a file. */
export function downloadBlob(filename: string, file: Blob): void {
  const url = URL.createObjectURL(file);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  link.style.display = "none";
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 2_000);
}

/** Has the browser save some bytes as a file. */
export function download(filename: string, bytes: ArrayBuffer | Uint8Array<ArrayBuffer>): void {
  downloadBlob(filename, new Blob([bytes], { type: "application/octet-stream" }));
}
