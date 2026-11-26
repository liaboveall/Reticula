import { expect, type Locator, type Page, test } from "@playwright/test";
import {
  type CollabServerHandle,
  createAndShareSheet,
  editCell,
  settled,
  signInDev,
  startCollabServer,
} from "./collab-helpers.ts";
import {
  activate,
  announced,
  expectFocusInside,
  expectNamed,
  pressTab,
  tabTo,
} from "./keyboard-helpers.ts";
import { type DevServer, startDevServer } from "./ui-harness.ts";

// Playwright does not intercept what a page requests in WebKit once a Service Worker controls it.
test.use({ serviceWorkers: "block" });

const isModal = (dialog: Locator) =>
  dialog.evaluate(
    (element) => element.matches("dialog:modal") && element.closest("[inert]") === null,
  );

// What the page at `apps/web/ui.html` contains, in a development build, where
// React's strict mode opens every dialog twice.
test.describe("dialogs and popovers", () => {
  let server: DevServer | null = null;

  test.beforeAll(async () => {
    test.setTimeout(60_000);
    server = await startDevServer();
  });

  test.afterAll(async () => {
    await server?.stop();
  });

  test.beforeEach(async ({ page }) => {
    await page.goto(`${server?.origin}/ui.html`);
    await expect(page.getByTestId("open-plain")).toBeVisible({ timeout: 30_000 });
  });

  const open = async (page: Page, name: string) => {
    await activate(page, page.getByTestId(`open-${name}`));
  };

  test("a dialog stays open, has a name and the focus, and gives it back", async ({ page }) => {
    await open(page, "plain");
    const dialog = page.getByRole("dialog", { name: "Plain" });
    await expect(page.getByTestId("first")).toBeFocused();
    // Longer than the close event of the first of the two openings takes.
    await page.waitForTimeout(300);
    expect(await isModal(dialog)).toBe(true);
    await expect(page.getByTestId("closings")).toHaveText("0");

    // Nothing behind it can be reached.
    for (let presses = 0; presses < 20; presses++) {
      await pressTab(page);
      const outside = await page.evaluate(
        () => document.activeElement?.closest("main") !== null && document.activeElement !== null,
      );
      expect(outside).toBe(false);
    }

    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
    await expect(page.getByTestId("closings")).toHaveText("1");
    await expect(page.getByTestId("open-plain")).toBeFocused();
  });

  test("an Escape is for what is being edited, and for the input method, first", async ({
    page,
  }) => {
    await open(page, "plain");
    const dialog = page.getByTestId("plain");
    await activate(page, page.getByTestId("edit"));
    // What had the focus is gone, and what is in its place has it.
    await expect(page.getByTestId("inline")).toBeFocused();

    await page.getByTestId("inline").evaluate((input) => {
      for (const init of [
        { key: "Escape", isComposing: true },
        { key: "Process", keyCode: 229 },
      ]) {
        input.dispatchEvent(
          new KeyboardEvent("keydown", { ...init, bubbles: true, cancelable: true }),
        );
      }
    });
    await expect(page.getByTestId("inline")).toBeFocused();

    await page.keyboard.press("Escape");
    await expect(page.getByTestId("inline")).toHaveCount(0);
    await expect(page.getByTestId("edit")).toBeFocused();
    await page.waitForTimeout(200);
    expect(await isModal(dialog)).toBe(true);

    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
  });

  test("a dialog that asks for a choice cannot be got around", async ({ page }) => {
    await open(page, "strict");
    const dialog = page.getByRole("dialog", { name: "Choose" });
    await expect(dialog).toBeFocused();
    for (let presses = 0; presses < 3; presses++) {
      await page.keyboard.press("Escape");
      await page.waitForTimeout(100);
      expect(await isModal(dialog)).toBe(true);
    }
    // However the browser may come to close it.
    await dialog.evaluate((element) => element instanceof HTMLDialogElement && element.close());
    await page.waitForTimeout(100);
    expect(await isModal(dialog)).toBe(true);
    await expect(page.getByTestId("closings")).toHaveText("0");

    await activate(page, page.getByTestId("choose"));
    await expect(dialog).toHaveCount(0);
    await expect(page.getByTestId("open-strict")).toBeFocused();
  });

  test("the focus goes elsewhere when what opened a dialog is gone", async ({ page }) => {
    await open(page, "vanishing");
    await expect(page.getByTestId("first")).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("plain")).toHaveCount(0);
    await expect(page.getByTestId("fallback")).toBeFocused();
  });

  test("pressing beside a dialog closes it, and ending a drag there does not", async ({ page }) => {
    await open(page, "plain");
    const dialog = page.getByTestId("plain");
    const box = await dialog.boundingBox();
    if (!box) {
      throw new Error("the dialog has no box");
    }
    await page.mouse.move(box.x + box.width / 2, box.y + 8);
    await page.mouse.down();
    await page.mouse.move(box.x - 20, box.y + 8);
    await page.mouse.up();
    await page.waitForTimeout(100);
    expect(await isModal(dialog)).toBe(true);

    // The cancel event of a file input bubbles.
    await page.getByTestId("file").evaluate((input) => {
      input.dispatchEvent(new Event("cancel", { bubbles: true }));
    });
    expect(await isModal(dialog)).toBe(true);

    await page.mouse.click(box.x - 20, box.y + 8);
    await expect(dialog).toHaveCount(0);
  });

  test("one dialog above another has the keys and the announcements", async ({ page }) => {
    await open(page, "plain");
    await activate(page, page.getByTestId("announce-in-dialog"));
    await expect.poll(() => announced(page)).toBe("From the dialog");
    await expect(page.getByTestId("page-status")).toHaveText("");

    await activate(page, page.getByTestId("open-nested"));
    const nested = page.getByRole("dialog", { name: "Nested" });
    expect(await isModal(nested)).toBe(true);
    await activate(page, nested.getByRole("button"));
    await expect(nested.locator('> [role="status"]')).toHaveText("From the nested dialog");
    expect(
      await nested
        .locator('> [role="status"]')
        .evaluate((element) => element.closest("[inert]") === null),
    ).toBe(true);

    await page.keyboard.press("Escape");
    await expect(nested).toHaveCount(0);
    expect(await isModal(page.getByTestId("plain"))).toBe(true);
    await expect(page.getByTestId("open-nested")).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("plain")).toHaveCount(0);

    await activate(page, page.getByTestId("announce"));
    await expect(page.getByTestId("page-status")).toHaveText("From the page");
  });

  test("the focus stays when a row of a list or the use of a button is gone", async ({ page }) => {
    await open(page, "plain");
    await activate(page, page.getByTestId("remove-one"));
    await expect(page.getByTestId("remove-two")).toBeFocused();
    await tabTo(page, page.getByTestId("remove-three"));
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("remove-three")).toHaveCount(0);
    await expectFocusInside(page.getByTestId("plain"));

    const once = page.getByTestId("once");
    await activate(page, once);
    await expect(once).toHaveText("Used 1 times");
    await expect(once).toBeDisabled();
    await expect(once).toBeFocused();
    await page.keyboard.press("Enter");
    await page.keyboard.press("Space");
    await expect(once).toHaveText("Used 1 times");
  });

  test("a palette is walked with the arrow keys and gives the focus back", async ({ page }) => {
    const button = page.getByTestId("palette-button");
    await expect(button).toHaveAttribute("aria-haspopup", "dialog");
    await expect(button).toHaveAttribute("aria-expanded", "false");
    await activate(page, button);
    await expect(button).toHaveAttribute("aria-expanded", "true");
    const palette = page.getByRole("dialog", { name: "Colors" });
    const color = (name: string) => palette.getByRole("button", { name, exact: true });
    await expect(color("Black")).toBeFocused();
    await page.keyboard.press("ArrowRight");
    await expect(color("Gray")).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(color("Green")).toBeFocused();
    await page.keyboard.press("End");
    await expect(color("Blue")).toBeFocused();
    await page.keyboard.press("Home");
    await page.keyboard.press("ArrowUp");
    await expect(color("Black")).toBeFocused();

    await page.keyboard.press("Escape");
    await expect(palette).toHaveCount(0);
    await expect(button).toBeFocused();

    await page.keyboard.press("Enter");
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("Space");
    await expect(button).toHaveText("Color: Gray");
    await expect(button).toBeFocused();
    await expect(palette).toHaveCount(0);

    // It opens at what is chosen, and Tab goes on from the button.
    await page.keyboard.press("ArrowDown");
    await expect(color("Gray")).toBeFocused();
    await pressTab(page);
    await expect(palette).toHaveCount(0);
    await expect(page.getByTestId("fruit")).toBeFocused();
  });

  test("a list is chosen from with the keyboard, and changes nothing before", async ({ page }) => {
    const button = page.getByTestId("fruit");
    await expect(button).toHaveAccessibleName("Fruit Banana");
    await expect(button).toHaveAttribute("aria-haspopup", "listbox");
    await tabTo(page, button);
    await page.keyboard.press("ArrowDown");
    const list = page.getByRole("listbox", { name: "Fruit" });
    const option = (name: string) => list.getByRole("option", { name });
    await expect(button).toHaveAttribute("aria-expanded", "true");
    await expect(option("Banana")).toBeFocused();
    await expect(option("Banana")).toHaveAttribute("aria-selected", "true");
    await page.keyboard.press("ArrowDown");
    await expect(option("Blueberry")).toBeFocused();
    await page.keyboard.press("End");
    await expect(option("Cherry")).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(option("Cherry")).toBeFocused();
    await page.keyboard.press("Home");
    await expect(option("Apple")).toBeFocused();
    await page.keyboard.type("bl");
    await expect(option("Blueberry")).toBeFocused();
    await expect(button).toHaveAttribute("data-value", "Banana");

    await page.keyboard.press("Escape");
    await expect(list).toHaveCount(0);
    await expect(button).toBeFocused();
    await expect(button).toHaveAttribute("data-value", "Banana");

    for (const [key, letter, fruit] of [
      ["Enter", "c", "Cherry"],
      ["Space", "a", "Apple"],
    ] as const) {
      await page.keyboard.press(key);
      await page.keyboard.press(letter);
      await expect(option(fruit)).toBeFocused();
      await page.keyboard.press(key);
      await expect(list).toHaveCount(0);
      await expect(button).toHaveAccessibleName(`Fruit ${fruit}`);
      await expect(button).toBeFocused();
    }

    await page.keyboard.press("Enter");
    await pressTab(page);
    await expect(list).toHaveCount(0);
    await expect(page.getByTestId("after")).toBeFocused();

    await page.getByTestId("fruit").click();
    await expect(list).toBeVisible();
    await page.mouse.click(5, 300);
    await expect(list).toHaveCount(0);
  });

  test("an Escape in a list leaves the dialog around it open", async ({ page }) => {
    await open(page, "plain");
    await activate(page, page.getByTestId("fruit-in-dialog"));
    await expect(page.getByRole("listbox")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("listbox")).toHaveCount(0);
    await expect(page.getByTestId("fruit-in-dialog")).toBeFocused();
    await page.waitForTimeout(200);
    expect(await isModal(page.getByTestId("plain"))).toBe(true);
  });

  test("a drawer takes the focus, lets go of it with Escape, and stays", async ({ page }) => {
    await activate(page, page.getByTestId("toggle-drawer"));
    const drawer = page.getByRole("complementary", { name: "Drawer" });
    await expect(drawer.getByRole("heading")).toBeFocused();
    await pressTab(page);
    await expect(page.getByTestId("tab-left")).toBeFocused();
    await page.keyboard.press("ArrowRight");
    await expect(page.getByTestId("tab-right")).toBeFocused();
    await expect(page.getByTestId("tab-right")).toHaveAttribute("aria-selected", "true");
    await expect(drawer.getByRole("tabpanel", { name: "Right" })).toContainText("The right side");
    await page.keyboard.press("ArrowRight");
    await expect(page.getByTestId("tab-left")).toBeFocused();

    await page.keyboard.press("Escape");
    await expect(page.getByTestId("fallback")).toBeFocused();
    await expect(drawer).toBeVisible();

    await activate(page, page.getByTestId("close-drawer"));
    await expect(drawer).toBeHidden();
    await expect(page.getByTestId("toggle-drawer")).toBeFocused();
  });

  test("the keyboard is seen on every control, in both themes", async ({ page }) => {
    for (const colorScheme of ["light", "dark"] as const) {
      await page.emulateMedia({ colorScheme });
      await page.reload();
      await expect(page.getByTestId("open-plain")).toBeVisible();
      for (let stops = 0; stops < 6; stops++) {
        await pressTab(page);
        const ring = await page.evaluate(() => {
          const element = document.activeElement;
          if (!element || element === document.body) {
            return null;
          }
          const { outlineStyle, outlineWidth, outlineColor } = getComputedStyle(element);
          return { outlineStyle, outlineWidth, outlineColor };
        });
        expect(ring).toEqual({
          outlineStyle: "solid",
          outlineWidth: "2px",
          outlineColor: colorScheme === "light" ? "rgb(26, 95, 208)" : "rgb(110, 161, 255)",
        });
      }
    }
  });

  test("the app's own dialogs stay open in a development build too", async ({ page }) => {
    test.setTimeout(90_000);
    await page.goto(`${server?.origin}/app?doc=strict-mode`);
    // Having opened the workbook closes what is in the way of it.
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 60_000 });
    for (const [button, name] of [
      ["workbooks-btn", "Local Workbooks"],
      ["cloud-menu-btn", "Cloud Collaboration"],
    ] as const) {
      await page.getByTestId(button).focus();
      await page.keyboard.press("Enter");
      const dialog = page.getByRole("dialog", { name });
      await expect(dialog).toBeVisible({ timeout: 30_000 });
      await page.waitForTimeout(400);
      expect(await isModal(dialog)).toBe(true);
      await page.keyboard.press("Escape");
      await expect(dialog).toHaveCount(0);
      await expect(page.getByTestId(button)).toBeFocused();
    }
  });
});

const textAt = (page: Page, row: number, col: number) =>
  page.evaluate(([r, c]) => window.__reticula?.textAt(r, c), [row, col] as const);

/** A workbook of its own, with a value to each row of a column. */
async function openWorkbook(page: Page, doc: string, column: readonly string[]): Promise<void> {
  await page.goto(`/app?hooks&doc=${doc}`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
  await settled(page);
  await pressTab(page);
  await expect(page.getByRole("button", { name: "Skip to the grid" })).toBeFocused();
  await page.keyboard.press("Enter");
  await expectFocusInside(page.getByTestId("grid"));
  for (const [row, value] of column.entries()) {
    await page.evaluate((r) => window.__reticula?.select(r, 0), row);
    await page.keyboard.type(value);
    await page.keyboard.press("Enter");
    await expect.poll(() => textAt(page, row, 0)).toBe(value);
  }
}

const focusInGrid = (page: Page) => expectFocusInside(page.getByTestId("grid"));

// None of these uses a mouse, except where it says so.
test.describe("the editor's dialogs and drawers", () => {
  test("find and replace all", async ({ page }, testInfo) => {
    await openWorkbook(page, `keys-find-${testInfo.project.name}`, ["item-10", "item-20", "other"]);
    await page.keyboard.press("ControlOrMeta+f");
    const dialog = page.getByRole("dialog", { name: "Find" });
    await expect(page.getByTestId("find-query")).toBeFocused();
    await page.keyboard.type("item");
    const replaceToggle = dialog.getByRole("button", { name: "Replace…" });
    await expect(replaceToggle).toHaveAttribute("aria-expanded", "false");
    await activate(page, replaceToggle);
    await tabTo(page, page.getByTestId("replace-input"));
    await page.keyboard.type("part");
    await expectNamed(dialog);

    await activate(page, page.getByTestId("find-all-btn"));
    await expect(page.getByTestId("find-status")).toHaveText("Found 2 match(es).");
    await activate(page, page.getByTestId("replace-all-btn"));
    await expect.poll(() => textAt(page, 0, 0)).toBe("part-10");
    await expect.poll(() => announced(page)).toBe("Replaced 2 cell(s).");

    // The same outcome again is read again.
    await page.evaluate(() => {
      const region = document.querySelector('dialog > [role="status"]');
      const changes: string[] = [];
      Object.assign(window, { announcements: changes });
      if (region) {
        new MutationObserver(() => changes.push(region.textContent ?? "")).observe(region, {
          childList: true,
          characterData: true,
          subtree: true,
        });
      }
    });
    await page.keyboard.press("Enter");
    await expect.poll(() => announced(page)).toBe("Replaced 0 cell(s).");
    await page.keyboard.press("Enter");
    await expect
      .poll(() =>
        page.evaluate(() => (window as unknown as { announcements: string[] }).announcements),
      )
      .toEqual(["", "Replaced 0 cell(s).", "", "Replaced 0 cell(s)."]);

    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
    await focusInGrid(page);
  });

  test("sort", async ({ page }, testInfo) => {
    await openWorkbook(page, `keys-sort-${testInfo.project.name}`, ["1", "3", "2"]);
    await page.evaluate(() =>
      window.__reticula?.selectRange({ row: 0, col: 0 }, { row: 2, col: 0 }),
    );
    await expect(page.getByTestId("name-box")).toHaveText("A1:A3");
    const button = page.getByTestId("m3a-sort-btn");
    await button.focus();
    await page.keyboard.press("Enter");
    const dialog = page.getByRole("dialog", { name: "Sort Range" });
    await expect(page.getByTestId("sort-first-row")).toBeFocused();
    await expect(page.getByTestId("sort-first-row")).toHaveAccessibleName("First row");
    await expect(page.getByTestId("sort-last-row")).toHaveAccessibleName("Last row");
    for (const name of ["First column", "Last column"]) {
      await expect(dialog.getByRole("spinbutton", { name })).toBeVisible();
    }

    // A level that is added and removed leaves the focus in the dialog.
    await activate(page, page.getByTestId("sort-add-key-btn"));
    await activate(page, dialog.getByRole("button", { name: "Remove level 2" }));
    await expect(page.getByTestId("sort-key-col-1")).toHaveCount(0);
    await expectFocusInside(dialog);
    await expectNamed(dialog);

    await page.getByTestId("sort-header-mode").focus();
    await page.getByTestId("sort-header-mode").selectOption("no");
    const order = page.getByTestId("sort-key-dir-0");
    await tabTo(page, order);
    if (process.platform === "darwin") {
      // There the key opens a menu of the system's, out of reach of the test.
      await order.selectOption("desc");
    } else {
      await page.keyboard.press("ArrowDown");
    }
    await expect(order).toHaveValue("desc");
    await activate(page, page.getByTestId("sort-apply-btn"));
    await expect(dialog).toHaveCount(0);
    await expect.poll(() => textAt(page, 0, 0)).toBe("3");
    await expect.poll(() => textAt(page, 2, 0)).toBe("1");
    await expect(button).toBeFocused();
  });

  test("filter", async ({ page }, testInfo) => {
    await openWorkbook(page, `keys-filter-${testInfo.project.name}`, ["Name", "Ann", "Ben", "Cid"]);
    await page.evaluate(() =>
      window.__reticula?.selectRange({ row: 0, col: 0 }, { row: 3, col: 0 }),
    );
    await expect(page.getByTestId("name-box")).toHaveText("A1:A4");
    await page.getByTestId("m3a-filter-btn").focus();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("m3a-filter-btn")).toHaveClass(/\bactive\b/);

    // The button is drawn in the header of the column: this is a click on it.
    await page.getByTestId("grid").focus();
    await focusInGrid(page);
    const box = await page.getByTestId("grid").boundingBox();
    const metrics = await page.evaluate(() => window.__reticula?.stats().metrics);
    if (!box || !metrics) {
      throw new Error("the grid has no box");
    }
    await page.mouse.click(
      box.x + metrics.headerWidth + metrics.colWidth - 10,
      box.y + metrics.headerHeight + metrics.rowHeight / 2,
    );
    const dialog = page.getByRole("dialog", { name: "AutoFilter — Column A" });
    await expect(dialog.getByRole("combobox", { name: "Column" })).toBeFocused();
    const modes = dialog.getByRole("group", { name: "Filter by" });
    await expect(modes.getByRole("button", { name: "Values" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await activate(page, modes.getByRole("button", { name: "Text" }));
    await expect(modes.getByRole("button", { name: "Text" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expect(dialog.getByRole("combobox", { name: "Condition" })).toBeVisible();
    await expectNamed(dialog);
    for (const mode of ["Number", "Top N"]) {
      await activate(page, modes.getByRole("button", { name: mode }));
      await expectNamed(dialog);
    }
    await tabTo(page, modes.getByRole("button", { name: "Text" }), { back: true });
    await page.keyboard.press("Enter");
    await tabTo(page, dialog.getByRole("textbox", { name: "Text" }));
    await page.keyboard.type("e");
    await activate(page, page.getByTestId("filter-apply-btn"));
    await expect(dialog).toHaveCount(0);
    await expect.poll(() => textAt(page, 1, 0)).toBe("Ben");
    await focusInGrid(page);
  });

  test("define and delete a name", async ({ page }, testInfo) => {
    await openWorkbook(page, `keys-names-${testInfo.project.name}`, ["1"]);
    await page.getByTestId("m3a-names-btn").focus();
    await page.keyboard.press("Enter");
    const dialog = page.getByRole("dialog", { name: "Name Manager" });
    await expect(page.getByTestId("name-input")).toBeFocused();
    await page.keyboard.type("RATE");
    const define = page.getByTestId("name-save-btn");
    await activate(page, define);
    await expect(page.getByTestId("names-list")).toContainText("RATE");
    // It has nothing more to do, and is where the keyboard is all the same.
    await expect(define).toBeDisabled();
    await expect(define).toBeFocused();
    await expectNamed(dialog);

    await activate(page, dialog.getByRole("button", { name: "Delete RATE" }));
    await expect(page.getByTestId("names-list")).toContainText("No defined named ranges yet.");
    await expectFocusInside(dialog);
    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
    await expect(page.getByTestId("m3a-names-btn")).toBeFocused();
  });

  test("create, rename and delete a workbook", async ({ page }, testInfo) => {
    const doc = `keys-books-${testInfo.project.name}`;
    await openWorkbook(page, doc, ["kept"]);
    const button = page.getByTestId("workbooks-btn");
    const dialog = page.getByRole("dialog", { name: "Local Workbooks" });
    const openDialog = async () => {
      await button.focus();
      await page.keyboard.press("Enter");
      await expect(page.getByTestId("new-blank-workbook-btn")).toBeFocused();
    };
    await openDialog();
    await page.keyboard.press("Enter");
    await expect(dialog).toHaveCount(0);
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await expect
      .poll(() => page.evaluate(() => window.__reticula?.docStatus().docId))
      .not.toBe(doc);
    const created = await page.evaluate(() => window.__reticula?.docStatus().docId ?? "");

    await openDialog();
    await expect(page.getByTestId(`open-doc-${created}`)).toHaveAttribute("aria-current", "true");
    await expectNamed(dialog);
    const row = page.getByTestId(`workbook-row-${doc}`);
    const rename = page.getByTestId(`rename-doc-${doc}`);
    await activate(page, rename);
    const input = page.getByTestId(`rename-doc-input-${doc}`);
    await expect(input).toBeFocused();
    await page.keyboard.type("not kept");
    await page.keyboard.press("Escape");
    await expect(input).toHaveCount(0);
    await expect(rename).toBeFocused();
    await page.waitForTimeout(200);
    expect(await isModal(dialog)).toBe(true);
    await expect(row).not.toContainText("not kept");

    await page.keyboard.press("Enter");
    await input.evaluate((element) => element instanceof HTMLInputElement && element.select());
    await page.keyboard.type("Renamed by keys");
    await page.keyboard.press("Enter");
    await expect(row).toContainText("Renamed by keys");
    await expect(rename).toBeFocused();

    // It is asked first, with keeping it where the focus is.
    await activate(page, dialog.getByRole("button", { name: "Delete Renamed by keys" }));
    const confirm = page.getByTestId("confirm-delete");
    await expect(confirm).toContainText('Delete "Renamed by keys"?');
    await expect(confirm.getByRole("button", { name: "Cancel" })).toBeFocused();
    await activate(page, confirm.getByTestId("confirm-delete-btn"));
    await expect(confirm).toHaveCount(0);
    await expect(row).toHaveCount(0);
    await expect.poll(() => announced(page)).toBe('Deleted "Renamed by keys".');
    await expectFocusInside(dialog);
    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
    await expect(button).toBeFocused();
  });

  test("a recorded demo is played, reviewed, hidden, and merged or given up", async ({
    page,
  }, testInfo) => {
    await openWorkbook(page, `keys-ai-${testInfo.project.name}`, ["1"]);
    const button = page.getByTestId("ai-panel-btn");
    await button.focus();
    await page.keyboard.press("Enter");
    const panel = page.getByRole("complementary", { name: "AI Analyst" });
    await expect(panel.getByRole("heading", { name: "AI Analyst" })).toBeFocused();
    await expect(panel.getByRole("tab", { name: "10 Recorded Demos" })).toHaveAttribute(
      "aria-selected",
      "true",
    );

    const demo = page.getByTestId("ai-demo-conflict-and-closure-demo");
    await activate(page, demo);
    await expect(page.getByTestId("ai-draft-banner")).toBeVisible({ timeout: 10_000 });
    // Being read-only now is no reason for the grid to take the focus.
    await expect(page.getByTestId("ai-replay-play")).toBeFocused();
    await activate(page, page.getByTestId("ai-replay-step"));
    await expect(page.getByTestId("ai-replay-step")).toBeFocused();
    await expect.poll(() => announced(page)).toMatch(/^1 \/ \d+: /);

    await activate(page, page.getByTestId("ai-replay-review-btn"));
    await expect(page.getByTestId("ai-diff-review")).toBeVisible();
    await expect.poll(() => announced(page)).toMatch(/^Review Changes \(\d+ hunks\)$/);
    const apply = page.getByTestId("ai-merge-apply");
    await expect(apply).toBeDisabled();
    await expectNamed(panel);

    const choice = panel.getByRole("group", { name: /^Version of .+!B3 to keep$/ });
    const keepMain = choice.getByRole("radio", { name: "Keep Main" });
    const takeDraft = choice.getByRole("radio", { name: "Use AI Draft" });
    await tabTo(page, keepMain);
    await page.keyboard.press("Space");
    await expect(keepMain).toBeChecked();
    await page.keyboard.press("ArrowDown");
    await expect(takeDraft).toBeChecked();
    await expect(apply).toBeEnabled();

    // Escape goes to the grid; the review stays. Closing the panel hides it.
    await page.keyboard.press("Escape");
    await focusInGrid(page);
    await expect(page.getByTestId("ai-diff-review")).toBeVisible();
    await page.getByTestId("ai-close-btn").focus();
    await page.keyboard.press("Enter");
    await expect(panel).toBeHidden();
    await expect(button).toBeFocused();
    await expect(page.getByTestId("ai-draft-banner")).toBeVisible();
    await page.keyboard.press("Enter");
    await expect(panel).toBeVisible();
    await expect(takeDraft).toBeChecked();

    await activate(page, apply);
    await expect(page.getByTestId("ai-draft-banner")).toBeHidden({ timeout: 10_000 });
    await expect.poll(() => announced(page)).toMatch(/^Merged selected changes from/);
    // With its draft the recording is done with: nothing could be applied a second time.
    await expect(demo).toBeFocused();
    await expect(page.getByTestId("ai-replay-play")).toHaveCount(0);
    await expect(panel.locator(".ai-card")).toContainText("Merged selected changes from");
    await expect.poll(() => textAt(page, 2, 1)).toBe("520");

    // Another one, to be given up.
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("ai-replay-play")).toBeFocused();
    await activate(page, page.getByTestId("ai-replay-review-btn"));
    await activate(page, page.getByTestId("ai-draft-discard"));
    await expect(page.getByTestId("ai-draft-banner")).toBeHidden({ timeout: 10_000 });
    await expect(demo).toBeFocused();
  });

  test("a recording is put away when its draft is left", async ({ page }, testInfo) => {
    await openWorkbook(page, `keys-left-${testInfo.project.name}`, ["kept"]);
    const panel = page.getByRole("complementary", { name: "AI Analyst" });
    const play = page.getByTestId("ai-replay-play");
    const banner = page.getByTestId("ai-draft-banner");
    await page.getByTestId("ai-panel-btn").focus();
    await page.keyboard.press("Enter");
    const demo = page.getByTestId("ai-demo-conflict-and-closure-demo");
    await activate(page, demo);
    await expect(banner).toBeVisible({ timeout: 10_000 });
    await activate(page, page.getByTestId("ai-replay-step"));
    await page.getByTestId("ai-banner-exit-btn").focus();
    await page.keyboard.press("Enter");
    await expect(banner).toBeHidden({ timeout: 10_000 });
    await expect(demo).toBeVisible();
    await expect(play).toHaveCount(0);
    await expect(panel.getByRole("button", { name: "Apply" })).toHaveCount(0);
    await expect.poll(() => textAt(page, 0, 0)).toBe("kept");

    // It is there again with a draft of its own, as is the next one that is selected.
    await demo.focus();
    await page.keyboard.press("Enter");
    await expect(banner).toBeVisible({ timeout: 10_000 });
    await expect(play).toHaveText("Play Replay");
    await tabTo(page, panel.getByRole("button", { name: "All Demos" }), { back: true });
    await page.keyboard.press("Enter");
    await expect(demo).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(banner).toBeVisible({ timeout: 10_000 });
    await expect(play).toBeVisible();
    await page.getByTestId("ai-replay-review-btn").focus();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("ai-diff-review")).toBeVisible();
    await page.getByTestId("ai-banner-exit-btn").focus();
    await page.keyboard.press("Enter");
    await expect(banner).toBeHidden({ timeout: 10_000 });
    await expect(page.getByTestId("ai-diff-review")).toHaveCount(0);
    await expect(play).toHaveCount(0);
    await expect.poll(() => textAt(page, 0, 0)).toBe("kept");
  });

  test("what could not be loaded is said in the language shown", async ({ page }, testInfo) => {
    await page.route("**/data/ai-replays/index.json", (route) => route.fulfill({ status: 503 }));
    await openWorkbook(page, `keys-said-${testInfo.project.name}`, ["1"]);
    await page.getByTestId("ai-panel-btn").focus();
    await page.keyboard.press("Enter");
    const said = page.getByTestId("ai-panel").locator(".ai-card");
    await expect(said).toHaveText("Could not load AI demos (503)");
    await page.keyboard.press("Escape");
    await focusInGrid(page);
    await page.keyboard.press("ControlOrMeta+/");
    await expect(page.getByTestId("palette-input")).toBeFocused();
    await page.keyboard.type("chinese");
    await page.keyboard.press("Enter");
    await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
    await expect(said).toHaveText(/^\p{Script=Han}.*503/u);
  });

  test("a draft is not taken into another workbook, and recordings are asked for again", async ({
    page,
  }, testInfo) => {
    let failing = true;
    await page.route("**/data/ai-replays/index.json", (route) =>
      failing ? route.abort() : route.continue(),
    );
    await openWorkbook(page, `keys-draft-${testInfo.project.name}`, ["kept"]);
    const button = page.getByTestId("ai-panel-btn");
    const panel = page.getByRole("complementary", { name: "AI Analyst" });
    await button.focus();
    await page.keyboard.press("Enter");
    await expect(panel.getByRole("heading", { name: "AI Analyst" })).toBeFocused();
    await expect(panel.locator(".ai-card")).toBeVisible();
    const demo = page.getByTestId("ai-demo-conflict-and-closure-demo");
    await expect(demo).toHaveCount(0);
    failing = false;
    await page.getByTestId("ai-close-btn").focus();
    await page.keyboard.press("Enter");
    await expect(button).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(panel.getByRole("heading", { name: "AI Analyst" })).toBeFocused();
    await activate(page, demo);
    await expect(panel.locator(".ai-card")).toHaveCount(1);
    await expect(page.getByTestId("ai-draft-banner")).toBeVisible({ timeout: 10_000 });
    await activate(page, page.getByTestId("ai-replay-review-btn"));
    await expect(page.getByTestId("ai-diff-review")).toBeVisible();
    await page.getByTestId("ai-close-btn").focus();
    await page.keyboard.press("Enter");
    await expect(panel).toBeHidden();

    await page.getByTestId("workbooks-btn").focus();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-blank-workbook-btn")).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("ai-draft-banner")).toBeHidden({ timeout: 10_000 });
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await settled(page);
    await editCell(page, 0, 0, "A1", "written");
    await expect.poll(() => textAt(page, 0, 0)).toBe("written");
    await button.focus();
    await page.keyboard.press("Enter");
    await expect(panel).toBeVisible();
    await expect(page.getByTestId("ai-diff-review")).toHaveCount(0);
  });

  test("the safe mode has to be answered", async ({ page }, testInfo) => {
    const doc = `keys-safe-${testInfo.project.name}`;
    await openWorkbook(page, doc, ["kept"]);
    const backend = await page.evaluate(() => window.__reticula?.docStatus().storageBackend);
    test.skip(backend === "memory", "nothing is kept here that could be opened again");
    await page.goto(`/app?hooks&doc=${doc}&safe=1`);
    const dialog = page.getByTestId("safe-mode-modal");
    await expect(page.getByTestId("safe-open-frozen")).toBeFocused();
    await expect(dialog).toHaveAccessibleDescription(/\S/);
    await page.keyboard.press("Escape");
    await page.keyboard.press("Escape");
    await page.waitForTimeout(200);
    expect(await isModal(dialog)).toBe(true);
    await page.keyboard.press("Enter");
    await expect(dialog).toHaveCount(0);
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await focusInGrid(page);
  });
});

test.describe("the pages", () => {
  test("have their heading focused and a title of their own after a navigation", async ({
    page,
  }) => {
    await page.goto("/privacy");
    await expect(page).toHaveTitle("Privacy · Reticula");
    // Nothing is taken from whoever has just loaded the page.
    expect(await page.evaluate(() => document.activeElement === document.body)).toBe(true);
    for (const [link, heading, title] of [
      ["Workbooks", "Workbooks", "Workbooks · Reticula"],
      ["Account", "Account & Storage", "Account · Reticula"],
      ["Privacy", "Privacy & Data Notice", "Privacy · Reticula"],
    ] as const) {
      await activate(page, page.getByRole("link", { name: link, exact: true }).first());
      await expect(page.getByRole("heading", { level: 1, name: heading })).toBeFocused();
      await expect(page).toHaveTitle(title);
    }
    await page.goBack();
    await expect(page.getByRole("heading", { level: 1 })).toBeFocused();
    await expect(page).toHaveTitle("Account · Reticula");

    await page.goto("/workbooks");
    await activate(page, page.getByRole("link", { name: "Editor", exact: true }));
    await focusInGrid(page);
    await expect(page).toHaveTitle("Reticula");
  });

  test("can be skipped to, and have landmarks", async ({ page }) => {
    for (const path of ["/", "/workbooks", "/account", "/privacy", "/sim", "/benchmarks"]) {
      await page.goto(path);
      await expect(page.getByRole("main")).toHaveCount(1);
      await expect(page.getByRole("main").getByRole("heading", { level: 1 })).toHaveCount(1);
      await pressTab(page);
      const skip = page.getByRole("button", { name: "Skip to content" });
      await expect(skip, path).toBeFocused();
      const box = await skip.boundingBox();
      expect(box?.width, path).toBeGreaterThan(40);
      await page.keyboard.press("Enter");
      await expect(page.getByRole("heading", { level: 1 }), path).toBeFocused();
    }
    for (const path of ["/workbooks", "/account", "/privacy", "/sim", "/benchmarks"]) {
      await page.goto(path);
      await expect(page.getByRole("navigation", { name: "Pages" })).toHaveCount(1);
    }
  });

  test("of workbooks and the simulator have tabs the arrow keys go through", async ({ page }) => {
    await page.goto("/workbooks");
    await tabTo(page, page.getByTestId("workbooks-tab-owned"));
    await page.keyboard.press("End");
    await expect(page.getByTestId("workbooks-tab-samples")).toHaveAttribute(
      "aria-selected",
      "true",
    );
    await expect(page.getByRole("tabpanel", { name: "Built-in Examples" })).toBeVisible();
    await pressTab(page);
    await expect(page.getByRole("button", { name: "Open Sample" })).toBeFocused();

    await page.goto("/sim");
    await expect(page.getByTestId("sim-replica-card-0")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByRole("region", { name: /Alice/ })).toBeVisible();
    await expectNamed(page.locator("body"));
    await tabTo(page, page.getByTestId("sim-mode-interactive"));
    await page.keyboard.press("ArrowRight");
    await expect(page.getByTestId("sim-mode-autorun")).toBeFocused();
    await expect(page.getByRole("progressbar", { name: "5-Replica Auto-Run" })).toBeVisible();
    // A grid without an editor shows that it has the keyboard.
    await page.keyboard.press("ArrowLeft");
    const grid = page.getByTestId("sim-grid-0");
    await tabTo(page, grid);
    await expect(grid).toHaveCSS("outline-style", "solid");
    await expect(grid).toHaveCSS("outline-width", "2px");
    await tabTo(page, page.getByTestId("sim-mode-interactive"), { back: true });
    await page.keyboard.press("ArrowRight");
    const run = page.getByTestId("sim-autorun-btn");
    await activate(page, run);
    await expect(run).toBeFocused();
    await expect(page.getByRole("status").filter({ hasText: "Invariants Verified" })).toBeVisible({
      timeout: 30_000,
    });
    await expect(run).toBeFocused();
  });
});

test.describe("with a server", () => {
  let server: CollabServerHandle | null = null;

  test.beforeAll(async () => {
    server = await startCollabServer();
  });

  test.afterAll(async () => {
    await server?.stop();
  });

  test("a share link is made, and the history walked through and left", async ({
    page,
  }, testInfo) => {
    test.setTimeout(90_000);
    if (!server) {
      throw new Error("the server has not been started");
    }
    await page.goto(`${server.baseUrl}/app?hooks`);
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
    const cloud = page.getByTestId("cloud-menu-btn");
    const dialog = page.getByRole("dialog", { name: "Cloud Collaboration" });
    await activate(page, cloud);
    await expect(page.getByTestId("dev-login-alice")).toBeFocused();
    await page.keyboard.press("Enter");
    // The buttons to sign in with are gone then.
    await expect(page.getByTestId("new-cloud-title-input")).toBeFocused({ timeout: 10_000 });
    await expect(page.getByTestId("new-cloud-title-input")).toHaveAccessibleName("Workbook title");
    await page.keyboard.press("ControlOrMeta+a");
    await page.keyboard.type(`Keys ${testInfo.project.name}`);
    await activate(page, page.getByTestId("new-cloud-sheet-btn"));
    await expect(dialog).toHaveCount(0);
    await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout: 15_000 });
    await settled(page);

    await cloud.focus();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-cloud-title-input")).toBeFocused();
    const share = page.getByTestId("create-share-link-btn");
    await activate(page, share);
    const url = page.getByRole("textbox", { name: "Share link" });
    await expect(url).toHaveValue(/\/join\//, { timeout: 10_000 });
    await expect(share).toBeFocused();
    await expect.poll(() => announced(page)).toBe("Share link created.");
    await expectNamed(dialog);
    await pressTab(page);
    await expect(url).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
    await expect(cloud).toBeFocused();

    await editCell(page, 0, 0, "A1", "100");
    await editCell(page, 0, 0, "A1", "250");
    await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout: 10_000 });

    const historyButton = page.getByTestId("history-btn");
    await historyButton.focus();
    await page.keyboard.press("Enter");
    const drawer = page.getByRole("complementary", { name: "Version History" });
    await expect(drawer.getByRole("heading", { name: "Version History" })).toBeFocused();
    await expect(page.getByTestId("history-entry-2")).toBeVisible({ timeout: 10_000 });
    await expectNamed(drawer);

    const name = drawer.getByRole("textbox", { name: "Name of the version" });
    await tabTo(page, name);
    await page.keyboard.type("By keys");
    const save = page.getByTestId("history-save-version-btn");
    await activate(page, save);
    await expect(page.getByTestId("history-versions-list")).toContainText("By keys", {
      timeout: 10_000,
    });
    await expect(save).toBeFocused();

    const back = drawer.getByRole("button", { name: "Previous revision" });
    await tabTo(page, back, { back: true });
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("history-preview-banner")).toBeVisible({ timeout: 10_000 });
    await expect.poll(() => textAt(page, 0, 0)).toBe("100");
    await expect(back).toBeFocused();
    await expect.poll(() => announced(page)).toMatch(/^Revision #\d+ of #\d+$/);
    await expect(drawer.locator('[aria-current="true"]').first()).toBeVisible();

    const forward = drawer.getByRole("button", { name: "Next revision" });
    await tabTo(page, forward);
    await page.keyboard.press("Enter");
    await expect.poll(() => textAt(page, 0, 0)).toBe("250");
    // At the end there is nothing further, and the keyboard stays.
    await expect(forward).toBeDisabled();
    await expect(forward).toBeFocused();

    const scrubber = page.getByTestId("history-scrubber");
    await tabTo(page, scrubber, { back: true });
    await page.keyboard.press("ArrowLeft");
    await expect.poll(() => textAt(page, 0, 0)).toBe("100");
    await expect(scrubber).toBeFocused();
    await expect(scrubber).toHaveAttribute("aria-valuetext", /^Revision #\d+ of #\d+$/);

    await page.keyboard.press("Escape");
    await focusInGrid(page);
    await expect(drawer).toBeVisible();

    const live = page.getByTestId("history-return-live-btn");
    await live.focus();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("history-preview-banner")).toBeHidden({ timeout: 10_000 });
    await expect.poll(() => textAt(page, 0, 0)).toBe("250");
    await expect(scrubber).toBeFocused();

    // The grid takes the focus nobody has, as after a click in a browser whose buttons do not.
    await page.getByTestId("history-step-prev").evaluate((step) => {
      if (document.activeElement instanceof HTMLElement && step instanceof HTMLElement) {
        document.activeElement.blur();
        step.click();
      }
    });
    await expect(page.getByTestId("history-preview-banner")).toBeVisible({ timeout: 10_000 });
    await focusInGrid(page);
    await live.focus();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("history-preview-banner")).toBeHidden({ timeout: 10_000 });

    await page.getByTestId("history-close-btn").focus();
    await page.keyboard.press("Enter");
    await expect(drawer).toHaveCount(0);
    await expect(historyButton).toBeFocused();

    for (const path of ["/workbooks", "/account"]) {
      await page.goto(`${server.baseUrl}${path}`);
      await expect(
        page.getByTestId(path === "/account" ? "delete-account-btn" : "workbooks-create-btn"),
      ).toBeVisible({ timeout: 10_000 });
      await expectNamed(page.locator("body"));
    }
  });

  test("a run that has just been started does not follow into another workbook", async ({
    page,
  }, testInfo) => {
    test.setTimeout(60_000);
    if (!server) {
      throw new Error("the server has not been started");
    }
    await signInDev(page, server.baseUrl, "carol");
    const { docId } = await createAndShareSheet(page, `Keys run ${testInfo.project.name}`);
    const requested: string[] = [];
    page.on("request", (request) => requested.push(new URL(request.url()).pathname));
    let answer = () => {};
    const answered = new Promise<void>((resolve) => {
      answer = resolve;
    });
    await page.route("**/agent/runs", async (route) => {
      if (route.request().method() !== "POST") {
        await route.continue();
        return;
      }
      await answered;
      const run = { id: "late-run", branchId: "late-branch", workbookId: docId, status: "running" };
      await route.fulfill({ status: 201, json: { ...run, model: "", prompt: "", summary: "" } });
    });

    await page.getByTestId("ai-panel-btn").focus();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("ai-prompt-input")).toBeFocused();
    await page.keyboard.type("Sum up");
    await activate(page, page.getByTestId("ai-run-btn"));
    await page.getByTestId("cloud-menu-btn").focus();
    await page.keyboard.press("Enter");
    await activate(page, page.getByTestId("back-to-local-btn"));
    await expect(page.getByTestId("collab-status-pill")).toHaveCount(0);
    await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 20_000 });
    // There are no cloud runs here, and the recordings are.
    await expect(page.getByTestId("ai-demo-conflict-and-closure-demo")).toBeVisible();

    const response = page.waitForResponse("**/agent/runs");
    answer();
    await response;
    await page.waitForTimeout(300);
    expect(requested.filter((path) => path.endsWith("/events"))).toEqual([]);
    await expect(page.getByTestId("ai-draft-banner")).toHaveCount(0);
  });

  test("cloud workbooks are renamed and deleted on their page", async ({ page }, testInfo) => {
    test.setTimeout(60_000);
    if (!server) {
      throw new Error("the server has not been started");
    }
    const title = `Keys page ${testInfo.project.name} ${Date.now()}`;
    await signInDev(page, server.baseUrl, "bob");
    const { docId } = await createAndShareSheet(page, title);
    await page.goto(`${server.baseUrl}/workbooks`);
    const row = page.getByTestId(`cloud-wb-${docId}`);
    await expect(row).toBeVisible({ timeout: 10_000 });
    const rename = row.getByRole("button", { name: `Rename ${title}` });
    await activate(page, rename);
    const input = row.getByRole("textbox", { name: `New name of ${title}` });
    await expect(input).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(input).toHaveCount(0);
    await expect(rename).toBeFocused();

    await page.keyboard.press("Enter");
    await page.keyboard.press("End");
    await page.keyboard.type(" B");
    await page.keyboard.press("Enter");
    await expect(row).toContainText(`${title} B`);
    await expect(row.getByRole("button", { name: `Rename ${title} B` })).toBeFocused();

    // It is asked first, and keeping it is what Enter does then.
    const del = row.getByRole("button", { name: `Delete ${title} B` });
    await activate(page, del);
    const confirm = page.getByTestId("confirm-delete");
    await expect(confirm).toContainText(`Delete "${title} B"?`);
    await expect(confirm.getByRole("button", { name: "Cancel" })).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(confirm).toHaveCount(0);
    await expect(row).toBeVisible();
    await expect(del).toBeFocused();
    await page.keyboard.press("Enter");
    await page.keyboard.press("Enter");
    await expect(confirm).toHaveCount(0);
    await expect(row).toBeVisible();

    await page.keyboard.press("Enter");
    await activate(page, confirm.getByTestId("confirm-delete-btn"));
    await expect(row).toHaveCount(0, { timeout: 10_000 });
    await expect.poll(() => announced(page)).toBe(`Deleted "${title} B".`);
    await expectFocusInside(page.getByRole("main"));
  });
});
