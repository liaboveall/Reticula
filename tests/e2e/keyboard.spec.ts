import { readFile } from "node:fs/promises";
import { test as base, expect, type Locator, type Page } from "@playwright/test";
import { settled } from "./collab-helpers.ts";
import {
  activate,
  announced,
  expectFocusInside,
  expectNamed,
  pressTab,
  tabTo,
} from "./keyboard-helpers.ts";

// The editor with the keyboard alone, from opening a workbook to exporting it.
// Whatever uses a mouse fails, in the test as in the page.

declare global {
  interface Window {
    /** How often a pointer was pressed in the page. */
    __pressed?: number;
  }
}

const MOUSE = ["click", "dblclick", "down", "move", "up", "wheel"] as const;
const POINTED = ["click", "dblclick", "hover", "tap", "check", "uncheck", "dragTo"] as const;

const test = base.extend<{ readonly keysOnly: undefined }>({
  keysOnly: [
    async ({ page }, use) => {
      const refuse = () => {
        throw new Error("this test has no mouse");
      };
      for (const name of MOUSE) {
        Object.defineProperty(page.mouse, name, { value: refuse });
      }
      const locators = Object.getPrototypeOf(page.locator("body"));
      const originals = POINTED.map((name) => [name, locators[name]] as const);
      for (const name of POINTED) {
        locators[name] = refuse;
      }
      await page.addInitScript(() => {
        window.addEventListener(
          "pointerdown",
          () => {
            window.__pressed = (window.__pressed ?? 0) + 1;
          },
          { capture: true },
        );
      });
      await use(undefined);
      for (const [name, original] of originals) {
        locators[name] = original;
      }
      expect(await page.evaluate(() => window.__pressed ?? 0)).toBe(0);
    },
    { auto: true },
  ],
});

// The banner of an update would be one more part of the window.
test.use({ serviceWorkers: "block" });

const textAt = (page: Page, row: number, col: number) =>
  page.evaluate(([r, c]) => window.__reticula?.textAt(r, c), [row, col] as const);

const stats = (page: Page) => page.evaluate(() => window.__reticula?.stats());

const focusInGrid = (page: Page) => expectFocusInside(page.getByTestId("grid"));

const selectionIs = (page: Page, name: string | RegExp) =>
  expect(page.getByTestId("name-box")).toHaveText(name);

async function ready(page: Page): Promise<void> {
  await expect(page.getByTestId("engine-status")).toHaveAttribute("data-status", "ready", {
    timeout: 20_000,
  });
  await settled(page);
}

/** Opens a workbook of its own and goes to the grid. */
async function open(page: Page, doc: string, query = ""): Promise<void> {
  await page.goto(`/app?hooks&doc=keys-${doc}-${test.info().project.name}${query}`);
  await ready(page);
  await pressTab(page);
  await page.keyboard.press("Enter");
  await focusInGrid(page);
}

/** Goes to a cell or a range by its name. */
async function goTo(
  page: Page,
  reference: string,
  shown: string | RegExp = reference,
): Promise<void> {
  await page.keyboard.press("ControlOrMeta+g");
  const input = page.getByTestId("name-box-input");
  await expect(input).toBeFocused();
  await page.keyboard.type(reference);
  await page.keyboard.press("Enter");
  await selectionIs(page, shown);
  await focusInGrid(page);
}

/** Types a column of values from the selected cell on. */
async function enter(page: Page, values: readonly string[]): Promise<void> {
  for (const value of values) {
    await page.keyboard.type(value);
    await page.keyboard.press("Enter");
  }
}

/** The shown texts of a column's first rows. */
const column = (page: Page, col: number, rows: number) =>
  page.evaluate(
    ([c, count]) =>
      Array.from({ length: count }, (_, row) => window.__reticula?.textAt(row, c) ?? null),
    [col, rows] as const,
  );

/** Runs a command by a part of its name, which has to be found first. */
async function command(page: Page, search: string, id: string): Promise<void> {
  await page.keyboard.press("ControlOrMeta+/");
  await expect(page.getByTestId("palette-input")).toBeFocused();
  await page.keyboard.type(search);
  await expect(page.locator('[role="option"][aria-selected="true"]')).toHaveAttribute(
    "data-testid",
    `command-${id}`,
  );
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("command-palette")).toHaveCount(0);
}

/** Goes to a control of the toolbar with the arrow keys. */
async function toolbarTo(page: Page, control: Locator): Promise<void> {
  const toolbar = page.getByRole("toolbar");
  for (let parts = 0; parts < 9; parts++) {
    if (await toolbar.evaluate((element) => element.contains(document.activeElement))) {
      break;
    }
    await page.keyboard.press("F6");
  }
  await expectFocusInside(toolbar);
  await page.keyboard.press("Home");
  for (let steps = 0; steps < 30; steps++) {
    if (await control.evaluate((element) => element === document.activeElement)) {
      return;
    }
    await page.keyboard.press("ArrowRight");
  }
  await expect(control).toBeFocused();
}

/** What is read out at once, above what else is there. */
const alerted = (page: Page) =>
  page.evaluate(() => {
    const dialog = [...document.querySelectorAll("dialog:modal")].at(-1);
    const region = dialog
      ? dialog.querySelector(':scope > [role="alert"]')
      : document.querySelector('[data-testid="page-alert"]');
    return region?.textContent ?? "";
  });

test("cells are gone to, written to and calculated", async ({ page }) => {
  await open(page, "cells");
  await enter(page, ["10", "20", "=A1+A2"]);
  await expect.poll(() => column(page, 0, 3)).toEqual(["10", "20", "30"]);
  await page.keyboard.press("ArrowUp");
  await selectionIs(page, "A3");
  await expect(page.locator(".formulabar[data-inspected]")).toBeVisible();
  await page.keyboard.press("F2");
  await page.keyboard.type("*2");
  await page.keyboard.press("Escape");
  await expect.poll(() => textAt(page, 2, 0)).toBe("30");
  await page.keyboard.press("F2");
  await page.keyboard.type("*2");
  await page.keyboard.press("Enter");
  await expect.poll(() => textAt(page, 2, 0)).toBe("50");

  await goTo(page, "C5");
  await page.keyboard.type("there");
  await page.keyboard.press("Tab");
  await expect.poll(() => textAt(page, 4, 2)).toBe("there");
  await selectionIs(page, "D5");
  await goTo(page, "a1:b3", "A1:B3");
  await goTo(page, "5:6", /^A5:/);
  expect((await stats(page))?.selection).toMatchObject({ scope: "rows" });
  await goTo(page, "B:C", /^B1:C/);
  expect((await stats(page))?.selection).toMatchObject({ scope: "cols" });
  await goTo(page, "A1:B2,D4", /\(\+1\)$/);
  expect((await stats(page))?.selection?.ranges).toEqual([
    { firstRow: 0, endRow: 2, firstCol: 0, endCol: 2 },
    { firstRow: 3, endRow: 4, firstCol: 3, endCol: 4 },
  ]);

  // What is wrong is said, and the input stays.
  await page.keyboard.press("ControlOrMeta+g");
  const input = page.getByTestId("name-box-input");
  await expect(input).toBeFocused();
  await page.keyboard.type("nowhere");
  await page.keyboard.press("Enter");
  await expect(input).toHaveAttribute("aria-invalid", "true");
  await expect(input).toHaveAccessibleDescription("“nowhere” is neither a reference nor a name.");
  await expect.poll(() => alerted(page)).toBe("“nowhere” is neither a reference nor a name.");
  await page.keyboard.press("Escape");
  await expect(input).toHaveCount(0);
  await focusInGrid(page);

  // A name stands for cells.
  await command(page, "name manager", "names");
  await expect(page.getByTestId("name-input")).toBeFocused();
  await page.keyboard.type("TARGET");
  await tabTo(page, page.getByTestId("name-formula-input"));
  await page.keyboard.press("ControlOrMeta+a");
  await page.keyboard.type("=$A$2");
  await activate(page, page.getByTestId("name-save-btn"));
  await expect(page.getByTestId("names-list")).toContainText("TARGET");
  await page.keyboard.press("Escape");
  await goTo(page, "target", "A2");

  // What is written is found by it in the formula bar as well.
  await page.keyboard.press("Shift+F6");
  await expect(page.getByTestId("formula-input")).toBeFocused();
  // On macOS, End scrolls and leaves the caret where it is.
  await page.keyboard.press(process.platform === "darwin" ? "Meta+ArrowRight" : "End");
  await page.keyboard.type("5");
  await page.keyboard.press("Enter");
  await expect.poll(() => textAt(page, 1, 0)).toBe("205");
});

test("a formula is completed from a list", async ({ page }) => {
  await open(page, "complete");
  const list = page.getByRole("listbox", { name: "Formula completions" });
  const inGrid = page.locator(".reticula-editor .cm-content");
  const inBar = page.getByTestId("formula-input");
  for (const input of [inGrid, inBar]) {
    if (input === inBar) {
      await page.keyboard.press("Shift+F6");
      await expect(inBar).toBeFocused();
    }
    await page.keyboard.type("=SUMI");
    await expect(list).toBeVisible();
    await expect(input).toHaveAttribute("aria-autocomplete", "list");
    const id = await list.getAttribute("id");
    await expect(input).toHaveAttribute("aria-controls", id ?? "");
    await expect(input).not.toHaveAttribute("aria-activedescendant", /./);
    await expect.poll(() => announced(page)).toMatch(/^SUMIFS?, \d+ suggestions?/);
    await page.keyboard.press("ArrowDown");
    const option = list.locator('[aria-selected="true"]');
    await expect(input).toHaveAttribute(
      "aria-activedescendant",
      (await option.getAttribute("id")) ?? "",
    );
    const name = (await option.getAttribute("data-testid"))?.replace("completion-", "");
    await page.keyboard.press("Enter");
    await expect(input).toHaveText(`=${name}(`);
    await expect(list).toHaveCount(0);
    await expect(input).not.toHaveAttribute("aria-controls", /./);
    await page.keyboard.press("Escape");
    await focusInGrid(page);
  }
});

test("rows, columns and ranges are selected and formatted", async ({ page }) => {
  await open(page, "formats");
  await enter(page, ["1", "2", "3"]);
  await goTo(page, "A1:B2");
  await page.keyboard.press("Shift+Space");
  expect((await stats(page))?.selection).toMatchObject({
    scope: "rows",
    range: { firstRow: 0, endRow: 2 },
  });
  await goTo(page, "B2:C2");
  await page.keyboard.press("Control+Space");
  expect((await stats(page))?.selection).toMatchObject({
    scope: "cols",
    range: { firstCol: 1, endCol: 3 },
  });
  // Neither of them has typed a space.
  expect(await textAt(page, 1, 1)).toBe("");

  await goTo(page, "A1:A3");
  const bold = page.getByTestId("fmt-bold");
  await page.keyboard.press("ControlOrMeta+b");
  await expect(bold).toHaveAttribute("aria-pressed", "true");
  await focusInGrid(page);
  await page.keyboard.press("ControlOrMeta+i");
  await expect(page.getByTestId("fmt-italic")).toHaveAttribute("aria-pressed", "true");
  await page.keyboard.press(process.platform === "darwin" ? "Meta+Shift+x" : "Control+5");
  await expect(page.getByTestId("fmt-strike")).toHaveAttribute("aria-pressed", "true");

  // The toolbar is one stop, and what it is used for leaves the keyboard there.
  await toolbarTo(page, bold);
  await page.keyboard.press("Enter");
  await expect(bold).toHaveAttribute("aria-pressed", "false");
  await expect(bold).toBeFocused();
  const center = page.getByTestId("align-center");
  await toolbarTo(page, center);
  await page.keyboard.press("Space");
  await expect(center).toHaveAttribute("aria-pressed", "true");
  await expect(center).toBeFocused();

  const format = page.getByTestId("number-format-select");
  await toolbarTo(page, format);
  await expect(format).toHaveAccessibleName("Number format General");
  await page.keyboard.press("ArrowDown");
  const formats = page.getByRole("listbox", { name: "Number format" });
  await expect(formats.getByRole("option", { name: "General" })).toBeFocused();
  await page.keyboard.type("perc");
  await page.keyboard.press("Enter");
  await expect(formats).toHaveCount(0);
  await expect(format).toHaveAccessibleName("Number format Percent (0%)");
  await expect(format).toBeFocused();
  await expect.poll(() => textAt(page, 0, 0)).toBe("100%");

  const fill = page.getByTestId("fill-color-btn");
  await toolbarTo(page, fill);
  await expect(fill).toHaveAttribute("aria-expanded", "false");
  await page.keyboard.press("Enter");
  const palette = page.getByRole("dialog", { name: "Fill color" });
  await expect(palette.getByRole("button", { name: "Gray" })).toBeFocused();
  await expect(fill).toHaveAttribute("aria-expanded", "true");
  await page.keyboard.press("ArrowRight");
  await page.keyboard.press("ArrowDown");
  await expect(palette.getByRole("button", { name: "Purple" })).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(palette).toHaveCount(0);
  await expect(fill).toBeFocused();
  await expect(fill.locator(".color-swatch-bar")).not.toHaveAttribute("style", /shell-line/);
  await page.keyboard.press("Enter");
  await expect(palette.getByRole("button", { name: "Purple" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  await expect(palette.getByRole("button", { name: "Purple" })).toBeFocused();
  await page.keyboard.press("End");
  await page.keyboard.press("Enter");
  await expect(palette).toHaveCount(0);
  await page.keyboard.press("Escape");

  // The grid is where the commands by name come from too.
  await page.keyboard.press("F6");
  await page.keyboard.press("F6");
  await focusInGrid(page);
  const undoLabel = () => page.getByTestId("undo-btn").getAttribute("title");
  for (const [search, id] of [
    ["medium borders", "borderMedium"],
    ["text color: red", "textColor:1"],
    ["format: integer", "numberFormat:0"],
    ["align right", "alignRight"],
  ] as const) {
    await command(page, search, id);
    await focusInGrid(page);
  }
  await expect(page.getByTestId("align-right")).toHaveAttribute("aria-pressed", "true");
  await expect.poll(() => textAt(page, 0, 0)).toBe("1");
  expect(await undoLabel()).toMatch(/^Undo .+\((Ctrl\+|⌘)Z\)$/);
});

test("rows and columns are inserted, moved, sized and deleted", async ({ page }) => {
  await open(page, "structure");
  await enter(page, ["a", "b", "c", "d"]);
  await goTo(page, "A2");
  const menu = page.getByRole("menu", { name: "Selection" });
  const item = (name: string) => menu.getByRole("menuitem", { name, exact: true });
  for (const key of ["Shift+F10", "ContextMenu"]) {
    await page.keyboard.press(key);
    await expect(item("Cut")).toBeFocused();
    // Where the cell is, and not at the pointer.
    const box = await menu.boundingBox();
    const grid = await page.getByTestId("grid").boundingBox();
    expect(box && grid && box.y - grid.y).toBeLessThan(120);
    await page.keyboard.press("ArrowUp");
    await expect(item("Filter This Column…")).toBeFocused();
    await expect(item("Filter This Column…")).toBeDisabled();
    await page.keyboard.press("Home");
    await page.keyboard.type("ins");
    await expect(item("Insert Row Above")).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(menu).toHaveCount(0);
    await focusInGrid(page);
  }
  await page.keyboard.press("Shift+F10");
  await page.keyboard.type("insert row b");
  await page.keyboard.press("Enter");
  await expect(menu).toHaveCount(0);
  await expect.poll(() => column(page, 0, 5)).toEqual(["a", "b", "", "c", "d"]);
  await focusInGrid(page);

  await goTo(page, "3:3", /^A3:/);
  await page.keyboard.press("Shift+F10");
  await expect(item("Move Rows Up")).toBeVisible();
  await page.keyboard.type("delete");
  await page.keyboard.press("Enter");
  await expect.poll(() => column(page, 0, 4)).toEqual(["a", "b", "c", "d"]);

  await goTo(page, "A1:A2");
  await command(page, "rows down", "moveRowsDown");
  await expect.poll(() => column(page, 0, 4)).toEqual(["c", "a", "b", "d"]);
  await command(page, "rows up", "moveRowsUp");
  await expect.poll(() => column(page, 0, 4)).toEqual(["a", "b", "c", "d"]);
  await goTo(page, "A1");
  await command(page, "rows up", "moveRowsUp");
  await expect.poll(() => announced(page)).toContain("Not possible here: Move Rows Up");

  await goTo(page, "A1");
  await command(page, "columns right", "moveColsRight");
  await expect.poll(() => textAt(page, 0, 1)).toBe("a");
  await goTo(page, "B1");
  await command(page, "columns left", "moveColsLeft");
  await expect.poll(() => textAt(page, 0, 0)).toBe("a");
  await goTo(page, "A:A", /^A1:A/);
  await command(page, "delete selected col", "deleteCols");
  await expect.poll(() => textAt(page, 0, 0)).toBe("");
  await page.keyboard.press("ControlOrMeta+z");
  await expect.poll(() => textAt(page, 0, 0)).toBe("a");

  // The sizes are the workbook's.
  const widthOf = () =>
    page.evaluate(() => window.__reticula?.stats().scrollLimits.left ?? Number.NaN);
  await goTo(page, "A1");
  const before = await widthOf();
  await command(page, "column width", "colWidth");
  const size = page.getByTestId("size-input");
  await expect(page.getByRole("dialog", { name: "Column Width" })).toBeVisible();
  await expect(size).toBeFocused();
  const width = Number(await size.inputValue());
  expect(width).toBeGreaterThan(20);
  await page.keyboard.type(String(width + 60));
  await page.keyboard.press("Enter");
  await expect(size).toHaveCount(0);
  await focusInGrid(page);
  await expect.poll(widthOf).toBe(before + 60);
  await command(page, "fit column", "autofitCols");
  await expect.poll(widthOf).toBeLessThan(before);
  await command(page, "column width", "colWidth");
  await activate(page, page.getByTestId("size-default"));
  await expect.poll(widthOf).toBe(before);
  await goTo(page, "3:4", /^A3:/);
  await page.keyboard.press("Shift+F10");
  await page.keyboard.type("row h");
  await page.keyboard.press("Enter");
  await expect(page.getByRole("dialog", { name: "Row Height" })).toBeVisible();
  await page.keyboard.press("Escape");
  await focusInGrid(page);

  // A sheet of as many columns as there can be has no room for another one; the sample has.
  await command(page, "column left", "insertColLeft");
  await expect(page.getByTestId("rejected-banner")).toContainText("16,384 columns");
  await page.goto("/app?hooks");
  await ready(page);
  await page.getByTestId("grid").focus();
  await goTo(page, "B1");
  const second = await textAt(page, 0, 1);
  await command(page, "column left", "insertColLeft");
  await expect.poll(() => textAt(page, 0, 2)).toBe(second);
  await command(page, "column right", "insertColRight");
  await expect.poll(() => textAt(page, 0, 3)).toBe(second);
  await expect(page.getByTestId("document")).toContainText("24 columns");
});

test("cells are filled, as copies and as a series", async ({ page }) => {
  await open(page, "fill");
  await enter(page, ["5", "=A1+1"]);
  await goTo(page, "A1:A4");
  await page.keyboard.press("ControlOrMeta+d");
  await expect.poll(() => column(page, 0, 4)).toEqual(["5", "5", "5", "5"]);
  await page.keyboard.press("ControlOrMeta+z");
  await expect.poll(() => column(page, 0, 4)).toEqual(["5", "6", "", ""]);
  // A single row takes what is above it, and a single column what is on its left.
  await goTo(page, "A3");
  await page.keyboard.press("ControlOrMeta+d");
  await expect.poll(() => textAt(page, 2, 0)).toBe("7");
  await goTo(page, "B2");
  await page.keyboard.press("ControlOrMeta+r");
  await expect.poll(() => textAt(page, 1, 1)).toBe("1");
  await goTo(page, "A1:C1");
  await page.keyboard.press("ControlOrMeta+r");
  await expect.poll(() => textAt(page, 0, 2)).toBe("5");

  await goTo(page, "E1");
  // How far the beginning of the series goes does not depend on what is shown of it by now.
  await page.evaluate(() => window.__reticula?.spinWorker(500));
  await enter(page, ["1", "3"]);
  await goTo(page, "E1:E5");
  await command(page, "series down", "fillSeriesDown");
  await expect.poll(() => column(page, 4, 5)).toEqual(["1", "3", "5", "7", "9"]);
  await goTo(page, "F5");
  await enter(page, ["10"]);
  await goTo(page, "F1:F5");
  await command(page, "series up", "fillSeriesUp");
  await expect.poll(() => textAt(page, 0, 5)).not.toBe("");
  await goTo(page, "E1:G1");
  await command(page, "series right", "fillSeriesRight");
  await expect.poll(() => textAt(page, 0, 6)).not.toBe("");
  await goTo(page, "C7");
  await enter(page, ["x"]);
  await goTo(page, "A7:C7");
  await command(page, "series left", "fillSeriesLeft");
  await expect.poll(() => textAt(page, 6, 0)).not.toBe("");
  await goTo(page, "D1");
  await enter(page, ["y"]);
  await goTo(page, "D1");
  await command(page, "end of the table", "autoFillDown");
  await expect.poll(() => textAt(page, 1, 3)).toBe("y");
});

test("rows are sorted, filtered, found and replaced", async ({ page }) => {
  await open(page, "data");
  await enter(page, ["Name", "Cid", "Ann", "Ben"]);
  await goTo(page, "A2:A4");
  await command(page, "sort range", "sort");
  await expect(page.getByTestId("sort-first-row")).toBeFocused();
  await activate(page, page.getByTestId("sort-apply-btn"));
  await expect.poll(() => column(page, 0, 4)).toEqual(["Name", "Ann", "Ben", "Cid"]);
  await focusInGrid(page);

  await goTo(page, "A1:A4");
  // There is nothing to choose from yet.
  await page.keyboard.press("Alt+ArrowDown");
  await expect.poll(() => announced(page)).toContain("Not possible here: Filter This Column…");
  await command(page, "autofilter", "filter");
  await expect(page.getByTestId("m3a-filter-btn")).toHaveAttribute("aria-pressed", "true");
  await page.keyboard.press("Alt+ArrowDown");
  const dialog = page.getByRole("dialog", { name: "AutoFilter — Column A" });
  await expect(dialog.getByRole("combobox", { name: "Column" })).toBeFocused();
  const modes = dialog.getByRole("group", { name: "Filter by" });
  await activate(page, modes.getByRole("button", { name: "Text" }));
  await tabTo(page, dialog.getByRole("textbox", { name: "Text" }));
  await page.keyboard.type("e");
  await activate(page, page.getByTestId("filter-apply-btn"));
  await expect(dialog).toHaveCount(0);
  await expect.poll(() => textAt(page, 1, 0)).toBe("Ben");
  await focusInGrid(page);
  // Rows have the numbers they have without the filter.
  await page.keyboard.press("ControlOrMeta+g");
  await page.keyboard.type("A2");
  await page.keyboard.press("Enter");
  await expect.poll(() => alerted(page)).toBe("Those rows are hidden by the filter.");
  await page.keyboard.press("Escape");
  await page.keyboard.press("ControlOrMeta+g");
  await page.keyboard.type("A3");
  await page.keyboard.press("Enter");
  await focusInGrid(page);
  expect((await stats(page))?.selection).toMatchObject({ row: 1, col: 0, text: "Ben" });
  await command(page, "clear filters", "clearFilters");
  await expect.poll(() => textAt(page, 1, 0)).toBe("Ann");
  await command(page, "autofilter", "filter");
  await expect(page.getByTestId("m3a-filter-btn")).toHaveAttribute("aria-pressed", "false");

  await page.keyboard.press("ControlOrMeta+f");
  await expect(page.getByTestId("find-query")).toBeFocused();
  // A dialog is not opened above another one.
  await page.keyboard.press("ControlOrMeta+/");
  await expect(page.getByRole("dialog")).toHaveCount(1);
  await expect(page.getByTestId("find-query")).toBeFocused();
  await page.keyboard.type("Cid");
  await activate(page, page.getByRole("button", { name: "Find Next" }));
  await expect(page.getByTestId("find-status")).toContainText("A4");
  await page.keyboard.press("Escape");
  await selectionIs(page, "A4");
  await focusInGrid(page);
  await page.keyboard.press(process.platform === "darwin" ? "Meta+Shift+h" : "Control+h");
  await expect(page.getByRole("dialog", { name: "Find & Replace" })).toBeVisible();
  await page.keyboard.type("Ann");
  await tabTo(page, page.getByTestId("replace-input"));
  await page.keyboard.type("Amy");
  await activate(page, page.getByTestId("replace-all-btn"));
  await expect.poll(() => textAt(page, 1, 0)).toBe("Amy");
  await page.keyboard.press("Escape");
  await focusInGrid(page);
});

test("worksheets are added, named, moved, gone to and deleted", async ({ page }) => {
  await open(page, "sheets");
  const tabs = page.getByRole("tablist", { name: "Worksheets" });
  const tab = (name: string) => tabs.getByRole("tab", { name, exact: true });
  await enter(page, ["first"]);
  await command(page, "add worksheet", "addSheet");
  await expect(tab("Sheet2")).toHaveAttribute("aria-selected", "true");
  await focusInGrid(page);
  await command(page, "add worksheet", "addSheet");
  await expect(tab("Sheet3")).toHaveAttribute("aria-selected", "true");
  // There is nothing in the list but tabs, and one of them to stop at.
  expect(
    await tabs.evaluate((list) => [...list.children].map((c) => c.getAttribute("role"))),
  ).toEqual(["tab", "tab", "tab"]);
  await expect(tabs.locator('[tabindex="0"]')).toHaveCount(1);

  await page.keyboard.press("Alt+PageUp");
  await expect(tab("Sheet2")).toHaveAttribute("aria-selected", "true");
  await page.keyboard.press("Alt+PageUp");
  await expect(tab("Sheet1")).toHaveAttribute("aria-selected", "true");
  await focusInGrid(page);
  await page.keyboard.press("Alt+PageDown");
  await expect(tab("Sheet2")).toHaveAttribute("aria-selected", "true");

  await page.keyboard.press("F6");
  await expect(tab("Sheet2")).toBeFocused();
  await page.keyboard.press("ArrowRight");
  await expect(tab("Sheet3")).toBeFocused();
  await expect(tab("Sheet3")).toHaveAttribute("aria-selected", "true");
  await page.keyboard.press("ArrowRight");
  await expect(tab("Sheet1")).toBeFocused();
  await page.keyboard.press("End");
  await expect(tab("Sheet3")).toBeFocused();

  // Escape takes a name back, and Enter keeps it, once.
  const input = page.getByTestId("rename-sheet-input");
  await page.keyboard.press("F2");
  await expect(input).toBeFocused();
  await expect(input).toHaveAccessibleName("New name of Sheet3");
  await page.keyboard.type("Not this");
  await page.keyboard.press("Escape");
  await expect(input).toHaveCount(0);
  await expect(tab("Sheet3")).toBeFocused();
  await page.waitForTimeout(200);
  await expect(tab("Sheet3")).toHaveText("Sheet3");
  const operations = () =>
    page.evaluate(() => Number(window.__reticula?.docStatus().walSeq ?? Number.NaN));
  const before = await operations();
  await page.keyboard.press("F2");
  await page.keyboard.type("Summary");
  await page.keyboard.press("Enter");
  await expect(tab("Summary")).toBeFocused();
  await page.waitForTimeout(200);
  expect(await operations()).toBe(before + 1);

  const menu = page.getByRole("menu", { name: "Summary" });
  await page.keyboard.press("Shift+F10");
  await expect(menu.getByRole("menuitem", { name: "Rename Sheet…" })).toBeFocused();
  await expect(menu.getByRole("menuitem", { name: "Move Sheet Right" })).toBeDisabled();
  await page.keyboard.type("move");
  await page.keyboard.press("Enter");
  await expect(tabs.getByRole("tab")).toHaveText(["Sheet1", "Summary", "Sheet2"]);
  await expect(tab("Summary")).toBeFocused();

  // Nothing is lost with an empty sheet. What there is something on is asked for.
  await page.keyboard.press("Delete");
  await expect(tabs.getByRole("tab")).toHaveText(["Sheet1", "Sheet2"]);
  await command(page, "previous sheet", "previousSheet");
  await tabTo(page, tabs.locator('[tabindex="0"]'));
  await page.keyboard.press("Home");
  await expect(tab("Sheet1")).toBeFocused();
  await page.keyboard.press("Delete");
  const asking = page.getByRole("dialog", { name: "Delete Sheet" });
  await expect(asking).toHaveAccessibleDescription(/Sheet1/);
  await expect(asking.getByRole("button", { name: "Cancel" })).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(asking).toHaveCount(0);
  await expect(tabs.getByRole("tab")).toHaveText(["Sheet1", "Sheet2"]);
  await expect(tab("Sheet1")).toBeFocused();
  await page.keyboard.press("Delete");
  await activate(page, page.getByTestId("delete-sheet-confirm"));
  await expect(tabs.getByRole("tab")).toHaveText(["Sheet2"]);
  await page.keyboard.press("ControlOrMeta+z");
  await expect(tabs.getByRole("tab")).toHaveText(["Sheet1", "Sheet2"]);
  await goTo(page, "Sheet1!A1", "A1");
  await expect(tab("Sheet1")).toHaveAttribute("aria-selected", "true");
  await expect.poll(() => textAt(page, 0, 0)).toBe("first");

  // A sheet that is not in view has rows of its own that are hidden.
  await goTo(page, "Sheet2!A1", "A1");
  await enter(page, ["Name", "Cid", "Ann", "Ben"]);
  await goTo(page, "A1:A4");
  await command(page, "autofilter", "filter");
  await expect(page.getByTestId("m3a-filter-btn")).toHaveAttribute("aria-pressed", "true");
  await page.keyboard.press("Alt+ArrowDown");
  const filter = page.getByRole("dialog", { name: "AutoFilter — Column A" });
  await activate(
    page,
    filter.getByRole("group", { name: "Filter by" }).getByRole("button", { name: "Text" }),
  );
  await tabTo(page, filter.getByRole("textbox", { name: "Text" }));
  await page.keyboard.type("e");
  await activate(page, page.getByTestId("filter-apply-btn"));
  await expect.poll(() => textAt(page, 1, 0)).toBe("Ben");
  await goTo(page, "Sheet1!A1", "A1");
  await goTo(page, "Sheet2!A4", "A4");
  await expect
    .poll(async () => (await stats(page))?.selection)
    .toMatchObject({ row: 1, text: "Ben" });
  await goTo(page, "Sheet1!A1", "A1");
  await page.keyboard.press("ControlOrMeta+g");
  await page.keyboard.type("Sheet2!A2");
  await page.keyboard.press("Enter");
  await expect.poll(() => alerted(page)).toBe("Those rows are hidden by the filter.");
  await page.keyboard.press("Escape");
  await focusInGrid(page);
});

test("what was done is undone and redone, and panes frozen and zoomed", async ({ page }) => {
  await open(page, "view");
  await enter(page, ["1", "2"]);
  const undo = page.getByTestId("undo-btn");
  await page.keyboard.press("ControlOrMeta+z");
  await expect.poll(() => textAt(page, 1, 0)).toBe("");
  await page.keyboard.press("ControlOrMeta+Shift+z");
  await expect.poll(() => textAt(page, 1, 0)).toBe("2");
  // The last use of a button does not take the keyboard from it.
  await undo.focus();
  await page.keyboard.press("Enter");
  await expect.poll(() => textAt(page, 1, 0)).toBe("");
  await page.keyboard.press("Enter");
  await expect(undo).toBeDisabled();
  await expect(undo).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(undo).toBeFocused();
  await page.keyboard.press("ControlOrMeta+Shift+z");
  await expect.poll(() => textAt(page, 0, 0)).toBe("1");

  const view = page.getByRole("button", { name: "View", exact: true });
  await view.focus();
  await page.keyboard.press("ArrowDown");
  const menu = page.getByRole("menu", { name: "View" });
  await expect(menu).toBeVisible();
  await expect(view).toHaveAttribute("aria-expanded", "true");
  await page.keyboard.type("freeze top");
  await page.keyboard.press("Enter");
  await expect(view).toBeFocused();
  await expect(page.getByTestId("freeze-select")).toHaveAccessibleName(
    "Freeze panes Freeze Top Row",
  );
  await page.keyboard.press("Enter");
  await expect(menu.getByRole("menuitemradio", { name: "Freeze Top Row" })).toBeChecked();
  await page.keyboard.press("Escape");
  await toolbarTo(page, page.getByTestId("freeze-select"));
  await page.keyboard.press("Enter");
  await expect(page.getByRole("option", { name: "Freeze Top Row" })).toBeFocused();
  await page.keyboard.press("Home");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("freeze-select")).toHaveAccessibleName("Freeze panes No Freeze");

  const zoom = page.getByTestId("zoom-value");
  await command(page, "zoom: 150", "zoom:150");
  await expect(zoom).toHaveText("150%");
  await command(page, "zoom out", "zoomOut");
  await expect(zoom).toHaveText("125%");
  for (let parts = 0; parts < 9; parts++) {
    if (await page.getByTestId("zoom-out").evaluate((one) => one === document.activeElement)) {
      break;
    }
    await page.keyboard.press("F6");
  }
  await pressTab(page);
  await expect(zoom).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("menuitemradio", { name: "125%" })).toBeFocused();
  await page.keyboard.type("100");
  await page.keyboard.press("Enter");
  await expect(zoom).toHaveText("100%");
  await expect(zoom).toBeFocused();
});

test("workbooks are exported and imported, and settings kept", async ({ page }) => {
  await open(page, "files");
  await enter(page, ["kept", "here"]);
  for (const [search, id, file] of [
    ["export .csv", "exportCsv", /\.csv$/],
    ["export .rtcl", "exportRtcl", /\.rtcl$/],
    ["export .xlsx", "exportXlsx", /\.xlsx$/],
  ] as const) {
    const download = page.waitForEvent("download");
    await command(page, search, id);
    const { suggestedFilename, path } = await download.then((one) => ({
      suggestedFilename: one.suggestedFilename(),
      path: one.path(),
    }));
    expect(suggestedFilename).toMatch(file);
    if (id === "exportCsv") {
      expect((await readFile(await path, "utf8")).trim()).toBe("kept\r\nhere");
    }
    await focusInGrid(page);
  }
  // The button for choosing a file has the focus, for browsers that do not open the choice themselves.
  const choosing = page.waitForEvent("filechooser", { timeout: 3_000 }).catch(() => null);
  await command(page, "import", "import");
  await expect(page.getByTestId("workbooks-drawer")).toBeVisible();
  let chooser = await choosing;
  if (!chooser) {
    await expect(page.getByTestId("import-file-btn")).toBeFocused();
    const again = page.waitForEvent("filechooser");
    await page.keyboard.press("Enter");
    chooser = await again;
  }
  await chooser.setFiles({
    name: "brought.csv",
    mimeType: "text/csv",
    buffer: Buffer.from("x,y\r\n1,2\r\n"),
  });
  await expect(page.getByTestId("workbooks-drawer")).toHaveCount(0);
  await expect(page.getByTestId("document")).toContainText("brought");
  await ready(page);
  await expect.poll(() => textAt(page, 1, 1)).toBe("2");

  await command(page, "canvas 2d", "rendererCanvas");
  expect(await page.evaluate(() => localStorage.getItem("reticula:renderer"))).toBe("canvas2d");
  await command(page, "graphics card", "rendererAuto");
  expect(await page.evaluate(() => localStorage.getItem("reticula:renderer"))).toBe("auto");

  // Whoever wants Chinese need not be able to type it yet.
  await command(page, "chinese", "language:zh-CN");
  await expect(page.getByRole("button", { name: "视图" })).toBeVisible();
  // Commands are found in either language.
  await command(page, "bold", "bold");
  await expect(page.getByTestId("fmt-bold")).toHaveAttribute("aria-pressed", "true");
  await command(page, "加粗", "bold");
  await expect(page.getByTestId("fmt-bold")).toHaveAttribute("aria-pressed", "false");
  await command(page, "english", "language:en");
  await expect(page.getByRole("button", { name: "View", exact: true })).toBeVisible();

  await command(page, "shortcuts", "keyboardHelp");
  const help = page.getByRole("dialog", { name: "Keyboard Shortcuts" });
  await expect(help.getByRole("row", { name: /Commands (Ctrl\+|⌘)\// })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(help).toHaveCount(0);
});

test("the parts of the window are gone through and left", async ({ page }) => {
  await open(page, "parts");
  const parts = [
    page.getByRole("tab", { selected: true }),
    page.getByTestId("zoom-out"),
    page.getByRole("link", { name: "Reticula" }),
    page.getByTestId("number-format-select"),
    page.getByTestId("formula-input"),
  ];
  for (const part of parts) {
    await page.keyboard.press("F6");
    await expect(part).toBeFocused();
  }
  await page.keyboard.press("F6");
  await focusInGrid(page);
  for (const part of parts.toReversed()) {
    await page.keyboard.press("Shift+F6");
    await expect(part).toBeFocused();
  }
  await page.keyboard.press("Shift+F6");
  await focusInGrid(page);
  // Also in the middle of typing, and nothing is lost.
  await page.keyboard.type("half");
  await page.keyboard.press("F6");
  await expect(page.getByRole("tab", { selected: true })).toBeFocused();
  await page.keyboard.press("Shift+F6");
  await focusInGrid(page);
  await page.keyboard.type("way");
  await page.keyboard.press("Enter");
  await expect.poll(() => textAt(page, 0, 0)).toBe("halfway");

  // A banner is a part as long as there is one, and so is a panel.
  await page.keyboard.type("=SUM(1,");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("rejected-banner")).toBeVisible();
  await expect.poll(() => alerted(page)).not.toBe("");
  await command(page, "ai analyst", "ai");
  const panel = page.getByRole("complementary", { name: "AI Analyst" });
  await expect(panel.getByRole("heading", { name: "AI Analyst" })).toBeFocused();
  await page.keyboard.press("F6");
  await expect(page.getByRole("link", { name: "Reticula" })).toBeFocused();
  await page.keyboard.press("F6");
  await page.keyboard.press("F6");
  const dismiss = page.getByRole("button", { name: "Dismiss error" });
  await expect(dismiss).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(dismiss).toHaveCount(0);
  await focusInGrid(page);
});

test("the toolbar is one stop of the Tab key", async ({ page }) => {
  await open(page, "stops");
  const toolbar = page.getByRole("toolbar");
  const stops = () =>
    toolbar.evaluate(
      (element) =>
        [...element.querySelectorAll("button")].filter((button) => button.tabIndex === 0).length,
    );
  expect(await stops()).toBe(1);
  const format = page.getByTestId("number-format-select");
  await toolbarTo(page, format);
  await page.keyboard.press("End");
  const names = page.getByTestId("m3a-names-btn");
  await expect(names).toBeFocused();
  expect(await stops()).toBe(1);
  await page.keyboard.press("ArrowRight");
  await expect(format).toBeFocused();
  await page.keyboard.press("ArrowLeft");
  await expect(names).toBeFocused();
  await pressTab(page);
  await expect(page.getByTestId("name-box")).toBeFocused();
  await pressTab(page, { back: true });
  await expect(names).toBeFocused();
});

test("a draft is looked at, and left for the grid", async ({ page }) => {
  await open(page, "draft");
  await enter(page, ["1"]);
  await command(page, "ai analyst", "ai");
  await activate(page, page.getByTestId("ai-demo-conflict-and-closure-demo"));
  await expect(page.getByTestId("ai-draft-banner")).toBeVisible({ timeout: 10_000 });
  // The formula bar is not for writing then, and can be read.
  const formula = page.getByTestId("formula-input");
  await expect(formula).toHaveAttribute("aria-readonly", "true");
  await page.keyboard.press("Escape");
  await focusInGrid(page);
  await page.keyboard.press("Shift+F6");
  await expect(formula).toBeFocused();
  await page.keyboard.type("x");
  await expect(formula).not.toHaveText(/x/);
  await page.keyboard.press("Shift+F6");
  await expect(page.getByTestId("ai-banner-exit-btn")).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("ai-draft-banner")).toBeHidden();
  await focusInGrid(page);
  await expect(formula).not.toHaveAttribute("aria-readonly", "true");
});

test("what the engine refuses is said, whatever dialog is open", async ({ page }) => {
  await open(page, "refused");
  await command(page, "name manager", "names");
  await expect(page.getByTestId("name-input")).toBeFocused();
  await page.keyboard.type("A1");
  await tabTo(page, page.getByTestId("name-formula-input"));
  await page.keyboard.press("ControlOrMeta+a");
  await page.keyboard.type("=1");
  await activate(page, page.getByTestId("name-save-btn"));
  await expect.poll(() => alerted(page)).not.toBe("");
  await expect(page.locator("dialog:modal")).toHaveCount(1);
});

test("a cell is edited from what is in it, not from what is shown", async ({ page }) => {
  await open(page, "formula");
  await enter(page, ["5", "=A1+1"]);
  await expect.poll(() => textAt(page, 1, 0)).toBe("6");
  const formula = page.getByTestId("formula-input");
  const editor = page.locator(".reticula-editor[data-editing]");
  /** Selects the formula, with the engine busy for a while. */
  const goThere = async () => {
    await goTo(page, "C5");
    await page.evaluate(() => {
      window.__reticula?.spinWorker(700);
      window.__reticula?.select(1, 0);
    });
  };

  await goThere();
  await page.keyboard.press("F2");
  await expect(editor).toHaveText("");
  await page.keyboard.press("Enter");
  await selectionIs(page, "A3");
  await goTo(page, "A2");
  await expect(formula).toHaveText("=A1+1");

  await goThere();
  await page.keyboard.press("F2");
  await page.keyboard.press("Escape");
  await page.keyboard.press("F2");
  await page.keyboard.type("+10");
  await expect(editor).toHaveText("=A1+1+10");
  await page.keyboard.press("Enter");
  await selectionIs(page, "A3");
  await expect.poll(() => textAt(page, 1, 0)).toBe("16");
  await goTo(page, "A2");
  await expect(formula).toHaveText("=A1+1+10");

  // What is entered before that goes behind it all the same, and what is typed next elsewhere.
  await goThere();
  await page.keyboard.press("F2");
  await page.keyboard.type("+100");
  await page.keyboard.press("Enter");
  await selectionIs(page, "A3");
  await page.keyboard.type("7");
  await page.keyboard.press("Enter");
  await expect.poll(() => column(page, 0, 3)).toEqual(["5", "116", "7"]);
  await goTo(page, "A2");
  await expect(formula).toHaveText("=A1+1+10+100");

  // What has been cleared a moment before is not there any more.
  await page.evaluate(() => window.__reticula?.spinWorker(700));
  await page.keyboard.press("Delete");
  await page.keyboard.press("F2");
  await page.keyboard.type("new");
  await page.keyboard.press("Enter");
  await expect.poll(() => column(page, 0, 3)).toEqual(["5", "new", "7"]);
});

test("what an input method composes is left alone when a cell's contents come", async ({
  page,
  browserName,
}) => {
  test.skip(browserName !== "chromium", "compositions are simulated through DevTools");
  await open(page, "composed");
  await enter(page, ["abc"]);
  await expect.poll(() => textAt(page, 0, 0)).toBe("abc");
  await goTo(page, "C5");
  await page.evaluate(() => {
    window.__reticula?.spinWorker(500);
    window.__reticula?.select(0, 0);
  });
  const editor = page.locator(".reticula-editor[data-editing]");
  await page.keyboard.press("F2");
  const session = await page.context().newCDPSession(page);
  const compose = (text: string) => {
    const at = text.length;
    return session.send("Input.imeSetComposition", { text, selectionStart: at, selectionEnd: at });
  };
  await compose("n");
  await compose("ni");
  await expect(page.getByTestId("formula-input")).toHaveText("ni");
  await page.evaluate(() => window.__reticula?.stateDigest());
  await compose("nih");
  await expect(editor).toHaveText("nih");
  await session.send("Input.insertText", { text: "你好" });
  await expect(editor).toHaveText("abc你好");
  await page.keyboard.press("Enter");
  await expect.poll(() => textAt(page, 0, 0)).toBe("abc你好");
});

test("on macOS the grid's own keys go with Cmd, and Ctrl is for others", async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(Navigator.prototype, "platform", { get: () => "MacIntel" });
  });
  await open(page, "mac");
  await enter(page, ["1", "2", "3"]);
  await expect.poll(() => column(page, 0, 3)).toEqual(["1", "2", "3"]);
  await page.evaluate(() => window.__reticula?.select(0, 0));
  for (const keys of ["z", "y", "a", "ArrowDown", "End", "Home", "Shift+ArrowDown"]) {
    await page.keyboard.press(`Control+${keys}`);
  }
  await page.waitForTimeout(300);
  await selectionIs(page, "A1");
  expect(await column(page, 0, 3)).toEqual(["1", "2", "3"]);

  await page.keyboard.press("Meta+ArrowDown");
  await selectionIs(page, "A3");
  await page.keyboard.press("Meta+Home");
  await selectionIs(page, "A1");
  await page.keyboard.press("Meta+a");
  await selectionIs(page, /^A1:/);
  await page.keyboard.press("Meta+z");
  await expect.poll(() => column(page, 0, 3)).toEqual(["1", "2", ""]);
  await page.keyboard.press("Meta+Shift+z");
  await expect.poll(() => column(page, 0, 3)).toEqual(["1", "2", "3"]);
});

test("what is typed before its cell is known is not lost", async ({ page }) => {
  await open(page, "held");
  await page.evaluate(() => {
    // Nothing comes from the engine for a while, so that the rows gone to are not there yet.
    window.__reticula?.spinWorker(700);
    window.__reticula?.select(900, 2);
    window.__reticula?.scrollTo({ top: Number.MAX_SAFE_INTEGER });
  });
  await page.keyboard.type("far away");
  await page.keyboard.press("Enter");
  expect(await textAt(page, 900, 2)).toBeNull();
  await expect.poll(() => textAt(page, 900, 2), { timeout: 10_000 }).toBe("far away");

  // The same where another sheet has just come into view.
  await command(page, "add worksheet", "addSheet");
  await ready(page);
  await command(page, "previous sheet", "previousSheet");
  await expect(page.getByTestId("sheet-tab-0")).toHaveAttribute("aria-selected", "true");
  await page.evaluate(() => {
    const tab = document.querySelector('[data-testid="sheet-tab-1"]');
    const observer = new MutationObserver(() => {
      observer.disconnect();
      window.__reticula?.spinWorker(700);
    });
    if (tab) {
      observer.observe(tab, { attributeFilter: ["aria-selected"] });
    }
  });
  await page.keyboard.press("Alt+PageDown");
  await expect(page.getByTestId("sheet-tab-1")).toHaveAttribute("aria-selected", "true");
  await page.keyboard.type("over here");
  await page.keyboard.press("Enter");
  await expect.poll(() => textAt(page, 0, 0), { timeout: 10_000 }).toBe("over here");
  await expect(page.getByTestId("sheet-tab-1")).toHaveAttribute("aria-selected", "true");

  // It is not for a workbook that is opened meanwhile.
  await page.evaluate(() => {
    // Long enough for the command below, however busy the machine is.
    window.__reticula?.spinWorker(3_000);
    window.__reticula?.select(800, 2);
    window.__reticula?.scrollTo({ top: Number.MAX_SAFE_INTEGER });
  });
  await page.keyboard.type("not there");
  await page.keyboard.press("Enter");
  await command(page, "new local workbook", "newWorkbook");
  await expect(page.getByTestId("rejected-banner")).toContainText("not there", {
    timeout: 10_000,
  });
  await expect(page.getByTestId("document")).toContainText("Untitled workbook");
  await ready(page);
  await goTo(page, "C801");
  await page.waitForTimeout(300);
  expect(await textAt(page, 800, 2)).toBe("");
  // No more than what nothing may be entered into any more.
  await expect(page.getByTestId("undo-btn")).toBeDisabled();
});

test("a workbook that is open in another tab is taken over with the keyboard", async ({
  page,
  context,
}) => {
  await open(page, "lock");
  const backend = await page.evaluate(() => window.__reticula?.docStatus().storageBackend);
  test.skip(backend === "memory", "nothing is kept here that could be locked");
  await enter(page, ["kept"]);
  await expect.poll(() => textAt(page, 0, 0)).toBe("kept");

  const other = await context.newPage();
  await other.goto(page.url());
  await expect(other.getByTestId("engine-status")).toHaveText("blocked");
  const takeOver = other.getByTestId("take-over-btn");
  await expect(takeOver).toBeFocused();
  await expect(takeOver).toHaveAccessibleDescription(/open in another tab/);
  await expect(other.getByRole("dialog")).toHaveCount(0);
  // The grid and the formula bar are not to be reached.
  await expect(other.getByTestId("grid")).toHaveAttribute("inert", "");
  for (let presses = 0; presses < 12; presses++) {
    await pressTab(other);
    const inside = await other.evaluate(
      () => document.activeElement?.closest(".reticula-grid, .formulabar") !== null,
    );
    expect(inside).toBe(false);
  }

  // What is being typed here meanwhile cannot be saved any more, and is not thrown away.
  await page.bringToFront();
  await page.keyboard.type("half a thought");
  await other.bringToFront();
  await takeOver.focus();
  await other.keyboard.press("Enter");
  await expect(other.getByTestId("engine-status")).toHaveText("ready", { timeout: 15_000 });
  await focusInGrid(other);
  await expect(page.getByTestId("engine-status")).toHaveText("yielded");
  await expect(page.getByTestId("lock-lost-edit")).toContainText("half a thought");
  await expect(page.getByTestId("take-over-btn")).toBeFocused();
  await page.keyboard.type("lost");
  await expect.poll(() => textAt(other, 0, 0)).toBe("kept");

  // With a dialog open as it happens, and with nothing in the cell but what was there.
  await other.keyboard.press("F2");
  await page.bringToFront();
  await page.getByTestId("take-over-btn").focus();
  await page.keyboard.press("Enter");
  await focusInGrid(page);
  await expect(other.getByTestId("engine-status")).toHaveText("yielded");
  await expect(other.getByTestId("lock-lost-edit")).toHaveCount(0);
  await command(page, "shortcuts", "keyboardHelp");
  await other.bringToFront();
  await other.getByTestId("take-over-btn").focus();
  await other.keyboard.press("Enter");
  await expect(other.getByTestId("engine-status")).toHaveText("ready", { timeout: 15_000 });
  await page.bringToFront();
  await expect(page.getByTestId("engine-status")).toHaveText("yielded");
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("take-over-btn")).toBeFocused();
  await other.close();
});

test.describe("every control", () => {
  test("has a name", async ({ page }) => {
    await open(page, "names");
    await enter(page, ["1"]);
    await expectNamed(page.locator("body"), 120);
    const tabs = page.getByRole("tab");
    for (const tab of await tabs.all()) {
      await expect(tab).toHaveAccessibleName(/\S/);
    }
    await page.keyboard.press("Shift+F10");
    await expectNamed(page.getByRole("menu"));
    await page.keyboard.press("Escape");
    for (const [search, id] of [
      ["row height", "rowHeight"],
      ["shortcuts", "keyboardHelp"],
      ["find & replace", "replace"],
    ] as const) {
      await command(page, search, id);
      // Not the palette's, nor the moment before this one's code has loaded.
      await expect(page.getByTestId("palette-input")).toHaveCount(0);
      await expect(page.getByRole("dialog")).toBeVisible();
      await expectNamed(page.getByRole("dialog"));
      await page.keyboard.press("Escape");
      await expect(page.getByRole("dialog")).toHaveCount(0);
    }
    await page.keyboard.press("ControlOrMeta+/");
    await expect(page.getByTestId("palette-input")).toHaveAccessibleName("Search commands");
    for (const option of (await page.getByRole("option").all()).slice(0, 40)) {
      await expect(option).toHaveAccessibleName(/\S/);
    }
    await page.keyboard.press("Escape");
  });

  test("is there in English", async ({ page }) => {
    await open(page, "aria-en");
    await expect(page.locator(".titlebar")).toMatchAriaSnapshot(`
      - link "Reticula":
        - heading "Reticula" [level=1]
      - button "Workbooks"
      - button "Cloud"
      - button "AI"
      - button "View"
      - button "Export .rtcl"
      - button /Undo/ [disabled]
      - button /Redo/ [disabled]
      - button /Insert Row Above/
      - button "Delete Selected Rows"
    `);
    await expect(page.getByRole("toolbar")).toMatchAriaSnapshot(`
      - toolbar "Cell formatting and freeze panes":
        - button "Number format General"
        - button "Bold"
        - button "Italic"
        - button "Strikethrough"
        - button "Align left"
        - button "Align center"
        - button "Align right"
        - button "Text color"
        - button "Fill color"
        - button "Thin borders"
        - button "Medium borders"
        - button "Clear borders"
        - button "Freeze panes No Freeze"
        - button "Insert Column Left"
        - button "Delete Selected Columns"
        - button "Sort"
        - button "Filter"
        - button "Find"
        - button "Names"
    `);
    await expect(page.locator(".formulabar-wrap")).toMatchAriaSnapshot(`
      - 'button "Name box: A1"'
      - textbox "Formula bar"
    `);
    await expect(page.locator(".sheetbar")).toMatchAriaSnapshot(`
      - tablist "Worksheets":
        - tab "Sheet1" [selected]
      - button "Add worksheet"
    `);
    await expect(page.locator(".statusbar")).toMatchAriaSnapshot(`
      - group "Zoom":
        - button "Zoom out"
        - 'button "Zoom: 100%"'
        - button "Zoom in"
    `);
  });

  test("is there in Chinese", async ({ page }) => {
    await open(page, "aria-zh", "&lang=zh");
    await expect(page.locator(".titlebar")).toMatchAriaSnapshot(`
      - link "Reticula":
        - heading "Reticula" [level=1]
      - button "工作簿"
      - button "云端"
      - button "AI"
      - button "视图"
      - button "导出 .rtcl"
      - button /撤销/ [disabled]
      - button /重做/ [disabled]
      - button /在上方插入行/
      - button "删除选中的行"
    `);
    await expect(page.getByRole("toolbar")).toMatchAriaSnapshot(`
      - toolbar "单元格格式与冻结窗格":
        - button "数字格式 常规"
        - button "加粗"
        - button "倾斜"
        - button "删除线"
        - button "左对齐"
        - button "居中"
        - button "右对齐"
        - button "文字颜色"
        - button "填充颜色"
        - button "细边框"
        - button "中等边框"
        - button "清除边框"
        - button "冻结窗格 不冻结"
        - button "在左侧插入列"
        - button "删除选中的列"
        - button "排序"
        - button "筛选"
        - button "查找"
        - button "名称"
    `);
    await expect(page.locator(".formulabar-wrap")).toMatchAriaSnapshot(`
      - button "名称框：A1"
      - textbox "编辑栏"
    `);
    await expect(page.locator(".sheetbar")).toMatchAriaSnapshot(`
      - tablist "工作表":
        - tab "Sheet1" [selected]
      - button "添加工作表"
    `);
    await page.keyboard.press("Shift+F10");
    await expect(page.getByRole("menu")).toMatchAriaSnapshot(`
      - menu "选区":
        - menuitem "剪切"
        - menuitem "复制"
        - menuitem "粘贴"
        - menuitem "清除内容"
        - separator
        - menuitem "在上方插入行"
        - menuitem "在下方插入行"
        - menuitem "删除选中的行"
        - separator
        - menuitem "在左侧插入列"
        - menuitem "在右侧插入列"
        - menuitem "删除选中的列"
        - separator
        - menuitem "筛选此列…" [disabled]
    `);
  });
});
