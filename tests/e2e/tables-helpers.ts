// Data tables, as far as several specs need them.
import { expect, type Locator, type Page } from "@playwright/test";

/** A worksheet that takes no time to make. */
export const DOC = "tables-doc";

/** Opens a local workbook that is empty unless it has been there before. */
export async function openWorkbook(page: Page, query = ""): Promise<void> {
  await page.goto(`/app?hooks&doc=${DOC}${query}`);
  await expect(page.getByTestId("engine-status")).toHaveAttribute("data-status", "ready", {
    timeout: 20_000,
  });
  // The hooks are loaded after everything else.
  await page.waitForFunction(() => window.__reticula !== undefined);
}

/** Whether the browser has what data tables are kept with. */
export function canKeepTables(page: Page): Promise<boolean> {
  return page.evaluate(
    () =>
      new Promise<boolean>((resolve) => {
        const source = `
          (async () => {
            const root = await navigator.storage.getDirectory();
            const file = await root.getFileHandle("probe-" + Math.random(), { create: true });
            (await file.createSyncAccessHandle()).close();
            return true;
          })().catch(() => false).then((can) => postMessage(can));`;
        const url = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
        const worker = new Worker(url);
        worker.onmessage = ({ data }: MessageEvent<boolean>) => {
          worker.terminate();
          URL.revokeObjectURL(url);
          resolve(data);
        };
      }),
  );
}

/** The parts of the table that is shown. */
export const view = (page: Page): Locator => page.getByTestId("table-view");
export const tableGrid = (page: Page): Locator => page.getByTestId("table-grid");
export const tab = (page: Page, name: string): Locator =>
  page.getByRole("tab", { name, exact: true });

/** The selected cell, as assistive technology has it. */
export function selectedCell(
  page: Page,
): Promise<{ readonly column: string; readonly row: string; readonly text: string }> {
  return tableGrid(page).evaluate((host) => {
    const text = (selector: string) => host.querySelector(selector)?.textContent ?? "";
    return {
      column: text(".reticula-mirror thead th"),
      row: text(".reticula-mirror tbody th"),
      text: text(".reticula-mirror tbody td"),
    };
  });
}

/** The selected cell once it has a text: what has not come yet looks like a blank. */
export async function filledCell(page: Page): ReturnType<typeof selectedCell> {
  await expect.poll(async () => (await selectedCell(page)).text).not.toBe("");
  return selectedCell(page);
}

/** Goes to a cell with the name box, and waits until it is known what is in it. */
export async function goTo(page: Page, place: string): Promise<void> {
  await page.keyboard.press("ControlOrMeta+g");
  const box = page.getByTestId("name-box-input");
  await box.fill(place);
  await box.press("Enter");
  await expect(box).toBeHidden();
}

/** The texts of some rows of a column. */
export async function textsOf(
  page: Page,
  column: string,
  rows: readonly number[],
): Promise<string[]> {
  const texts: string[] = [];
  for (const row of rows) {
    await goTo(page, `${column} ${row}`);
    await expect
      .poll(async () => {
        const cell = await selectedCell(page);
        return `${cell.column} ${cell.row}`;
      })
      .toBe(`${column} ${row}`);
    // What has not come yet looks like a blank.
    await expect
      .poll(() => page.evaluate(() => window.__reticula?.table()?.stats().selection?.text ?? null))
      .not.toBeNull();
    const text = page.getByTestId("table-text");
    await expect
      .poll(async () => (await selectedCell(page)).text === (await text.textContent()))
      .toBe(true);
    texts.push((await selectedCell(page)).text);
  }
  return texts;
}

/** A CSV file of orders: an integer, a text, a decimal, a date and a blank now and then. */
export function ordersCsv(rows: number): string {
  const regions = ["North", "south", "East", "west"];
  const lines = ["id,region,amount,day,note"];
  for (let row = 1; row <= rows; row++) {
    const day = new Date(Date.UTC(2024, 0, 1 + (row % 366))).toISOString().slice(0, 10);
    const amount = ((row * 7919) % 100_000) / 100;
    lines.push(
      `${row},${regions[row % 4]},${amount.toFixed(2)},${day},${row % 5 === 0 ? "" : `note ${row % 17}`}`,
    );
  }
  return `${lines.join("\n")}\n`;
}

/** Imports a text as a table by way of the dialog, and waits until it is shown. */
export async function importTable(page: Page, name: string, text: string): Promise<void> {
  await openImport(page);
  await page
    .getByTestId("table-import-input")
    .setInputFiles({ name: `${name}.csv`, mimeType: "text/csv", buffer: Buffer.from(text) });
  await expect(page.getByTestId("table-import-preview")).toBeVisible();
  await page.getByTestId("table-import-start").click();
  await expect(page.getByTestId("table-import")).toBeHidden();
  await expect(view(page)).toHaveAttribute("data-status", "open");
}

/** Opens the dialog for importing a table, without the browser's own one for files. */
export async function openImport(page: Page): Promise<void> {
  // The browser's dialog is asked for at once, and is of no use here.
  page.once("filechooser", () => {});
  await runCommand(page, "importTable");
  await expect(page.getByTestId("table-import")).toBeVisible();
}

/** Opens the list of all commands, which may have to be loaded first. */
export async function openPalette(page: Page): Promise<void> {
  await page.keyboard.press("ControlOrMeta+/");
  await expect(page.getByTestId("palette-input")).toBeFocused();
}

/** Runs a command by way of the list of all commands. */
export async function runCommand(page: Page, id: string): Promise<void> {
  await openPalette(page);
  await page.getByTestId(`command-${id}`).click();
  await expect(page.getByTestId("command-palette")).toBeHidden();
}

/** What the worksheets have in them: equal exactly when no byte of it has changed. */
export function content(
  page: Page,
): Promise<{ readonly digest: string | undefined; readonly operations: string | undefined }> {
  return page.evaluate(async () => ({
    digest: await window.__reticula?.stateDigest(),
    operations: window.__reticula?.docStatus().walSeq,
  }));
}

declare global {
  interface Window {
    /** Everything that was put into the page's live region, in order. */
    __tableSaid?: string[];
  }
}

/** Keeps what is read out from now on, in this page and after it is loaded again. */
export async function listenToPage(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const said: string[] = [];
    window.__tableSaid = said;
    let shown = "";
    new MutationObserver(() => {
      const text = document.querySelector('[data-testid="page-status"]')?.textContent ?? "";
      if (text !== shown) {
        shown = text;
        if (text !== "") {
          said.push(text);
        }
      }
    }).observe(document, { childList: true, subtree: true, characterData: true });
  });
}

/** Waits until something has been read out, and forgets what came before. */
export async function expectSaid(page: Page, expected: RegExp): Promise<void> {
  await expect
    .poll(
      () =>
        page.evaluate((source) => {
          const said = window.__tableSaid ?? [];
          const at = said.findIndex((one) => new RegExp(source).test(one));
          if (at >= 0) {
            said.splice(0, at + 1);
            return "said";
          }
          return said;
        }, expected.source),
      { message: `${expected} to be read out` },
    )
    .toBe("said");
}

/** The columns of the header that are sorted or filtered are told by the selected cell's. */
export function sortOfSelected(page: Page): Promise<string | null> {
  return tableGrid(page).evaluate(
    (host) => host.querySelector(".reticula-mirror thead th")?.getAttribute("aria-sort") ?? null,
  );
}

/** Opens the menu of the active cell's column with the keyboard, and runs an entry of it. */
export async function columnMenu(page: Page, entry: string): Promise<void> {
  await page.keyboard.press("Alt+ArrowDown");
  await page.getByTestId("table-menu").getByTestId(`menu-${entry}`).click();
  await expect(page.getByTestId("table-menu")).toBeHidden();
}

/** Drops a file on a part of the page, as it comes from the desktop. */
export async function dropFiles(
  target: Locator,
  files: readonly { readonly name: string; readonly text: string }[],
): Promise<void> {
  await target.evaluate((element, dropped) => {
    const dataTransfer = new DataTransfer();
    for (const { name, text } of dropped) {
      dataTransfer.items.add(new File([text], name));
    }
    for (const type of ["dragenter", "dragover", "drop"]) {
      element.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer }));
    }
  }, files);
}

/** The files of a workbook's tables, in this browser. */
export function tableFiles(page: Page, docId = DOC): Promise<string[]> {
  return page.evaluate(async (id) => {
    try {
      let dir = await navigator.storage.getDirectory();
      for (const name of ["reticula-v1", "tables", id]) {
        dir = await dir.getDirectoryHandle(name);
      }
      const names: string[] = [];
      for await (const name of (dir as unknown as { keys(): AsyncIterable<string> }).keys()) {
        names.push(name);
      }
      return names.sort();
    } catch {
      return [];
    }
  }, docId);
}
