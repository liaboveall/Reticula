// What tables come to in a browser, for comparison with what they come to
// natively (tests/determinism/table-app.txt).

/**
 * Has the app make its example and import a text, and orders both: SHA-256 of
 * the files and of what is copied, a line each. Runs in a page of the app that
 * was opened with its hooks, and is handed over as text, so that it can refer to
 * nothing outside.
 */
export async function tableDigest(text: string): Promise<string> {
  const hooks = window.__reticula;
  if (!hooks) {
    throw new Error("the test hooks are not installed");
  }
  const { TableClient } = await hooks.tables();
  const client = new TableClient(
    { docId: `table-digest-${Date.now()}`, tabId: crypto.randomUUID() },
    { onPage: () => {} },
  );
  const sha256 = async (bytes: ArrayBuffer) =>
    Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)), (byte) =>
      byte.toString(16).padStart(2, "0"),
    ).join("");
  const asc = (column: number) => ({ column, descending: false });
  const desc = (column: number) => ({ column, descending: true });
  let lines = "";
  const digest = async (
    name: string,
    id: string,
    query: Parameters<typeof client.query>[0],
    cols: number,
  ) => {
    const file = await client.export(id);
    lines += `${name} ${await sha256(await file.arrayBuffer())}\n`;
    await client.open(id);
    const { rows } = await client.query(query);
    const { tsv } = await client.copy({ firstRow: 0, rows, firstCol: 0, cols });
    lines += `${name} view ${await sha256(new TextEncoder().encode(tsv).buffer)}\n`;
    await client.closeTable();
    await client.delete(id);
  };
  try {
    await client.acquire();
    const example = await client.sample("example", 70_000, 2026);
    await digest(
      "example",
      example.entry.id,
      {
        filters: [{ column: 6, condition: { kind: "contains", text: "LAMP" } }],
        order: [desc(6), asc(11)],
      },
      13,
    );
    const imported = await client.importCsv({ file: new Blob([text]), options: {} }, "text");
    await digest(
      "text",
      imported.entry.id,
      {
        filters: [{ column: 1, condition: { kind: "notBlank" } }],
        order: [asc(7), desc(3), asc(0)],
      },
      7,
    );
  } finally {
    await client.close();
  }
  return lines;
}
