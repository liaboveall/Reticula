import { type CDPSession, expect, type Page, test } from "@playwright/test";

// The cell editor, on the page made for checking input methods by hand.
//
// Real input methods cannot be driven by a test. What can be checked is how
// the editor responds to the events an input method produces, and Chromium's
// DevTools protocol can produce those. This guards the logic against being
// broken later; it does not replace the check by hand.

const ADDRESS = "http://localhost:5275/ime.html";

test.skip(
  ({ browserName }) => browserName !== "chromium",
  "compositions are simulated through the DevTools protocol",
);

function state(page: Page) {
  return page.evaluate(() => {
    const hooks = window.__ime;
    if (!hooks) {
      throw new Error("the page's hooks are not installed");
    }
    return hooks.state();
  });
}

/** Opens the page and waits until a cell is selected and its text is there. */
async function open(page: Page): Promise<void> {
  await page.goto(ADDRESS);
  await page.waitForFunction(() => {
    const hooks = window.__ime;
    const selection = hooks?.state().selection;
    return selection != null && hooks?.textAt(selection.row, selection.col) != null;
  });
  // The page starts with D2 selected and the editor focused.
  expect((await state(page)).selection).toEqual({ row: 1, col: 3 });
}

/** Starts or continues a composition, as an input method does while the user types. */
async function compose(session: CDPSession, text: string): Promise<void> {
  await session.send("Input.imeSetComposition", {
    text,
    selectionStart: text.length,
    selectionEnd: text.length,
  });
}

/**
 * A key press as the page sees it while an input method has the keyboard.
 *
 * The input method consumes the key; the page is only told that there was
 * one. Browsers mark the event in one of two ways: as part of a composition,
 * or with the key code 229, which is also all there is for the key that
 * starts a composition. Such an event is made here, because a key pressed
 * through the browser's automation is a real key that no input method is
 * there to consume.
 */
async function pressDuringComposition(
  page: Page,
  key: string,
  shape: "composing" | "code 229",
): Promise<void> {
  await page.evaluate(
    ([key, shape]) => {
      const event = new KeyboardEvent("keydown", {
        key: shape === "composing" ? key : "Process",
        isComposing: shape === "composing",
        bubbles: true,
        cancelable: true,
      });
      if (shape === "code 229") {
        Object.defineProperty(event, "keyCode", { value: 229 });
      }
      document.activeElement?.dispatchEvent(event);
    },
    [key, shape] as const,
  );
}

test("typing a character starts an edit, and Enter accepts it and moves down", async ({ page }) => {
  await open(page);
  expect(await state(page)).toMatchObject({ editing: null, shown: false, text: "" });

  await page.keyboard.press("h");
  // The character went into the editor, once, and the editor is now seen.
  expect(await state(page)).toMatchObject({ editing: { row: 1, col: 3 }, shown: true, text: "h" });
  await page.keyboard.type("ello");
  expect((await state(page)).text).toBe("hello");

  await page.keyboard.press("Enter");
  expect(await state(page)).toMatchObject({
    editing: null,
    shown: false,
    text: "",
    selection: { row: 2, col: 3 },
    committed: { D2: "hello" },
  });
  await expect(page.getByTestId("committed")).toHaveText("D2 ← “hello”");

  // The next character starts the next edit, in the cell below.
  await page.keyboard.press("x");
  expect(await state(page)).toMatchObject({ editing: { row: 2, col: 3 }, text: "x" });
});

test("the arrows, Tab and Enter move the selection while nothing is being edited", async ({
  page,
}) => {
  await open(page);
  for (const [key, row, col] of [
    ["ArrowDown", 2, 3],
    ["ArrowRight", 2, 4],
    ["ArrowUp", 1, 4],
    ["ArrowLeft", 1, 3],
    ["Tab", 1, 4],
    ["Shift+Tab", 1, 3],
    ["Enter", 2, 3],
    ["Shift+Enter", 1, 3],
  ] as const) {
    await page.keyboard.press(key);
    expect((await state(page)).selection, key).toEqual({ row, col });
  }
  // At the edge of the sheet the selection stays where it is.
  await page.keyboard.press("ArrowUp");
  await page.keyboard.press("ArrowUp");
  expect((await state(page)).selection).toEqual({ row: 0, col: 3 });
  expect((await state(page)).editing).toBeNull();
});

test("the editor stays on its cell when the selection moves out of view", async ({ page }) => {
  await open(page);
  const { visible, metrics } = await page.evaluate(() => {
    const stats = window.__ime?.grid();
    if (!stats) {
      throw new Error("the page's hooks are not installed");
    }
    return stats;
  });
  const before = await state(page);
  // The page starts on the second row. One row further down than fits: the
  // grid has to scroll.
  const startRow = 1;
  const presses = visible.endRow - startRow;
  for (let press = 0; press < presses; press++) {
    await page.keyboard.press("ArrowDown");
  }
  await page.waitForFunction(() => (window.__ime?.grid().visible.firstRow ?? 0) > 0);
  const after = await state(page);
  expect(after.selection).toEqual({ row: startRow + presses, col: 3 });
  // The selected cell is fully in view, and the hidden editor is on it:
  // where an input method would open its window.
  const grid = await page.getByTestId("mode").evaluate(() => {
    const bounds = document.getElementById("grid")?.getBoundingClientRect();
    return { top: bounds?.top ?? 0, bottom: bounds?.bottom ?? 0 };
  });
  expect(after.box.top).toBeGreaterThanOrEqual(grid.top + metrics.headerHeight);
  expect(after.box.top + metrics.rowHeight).toBeLessThanOrEqual(
    grid.bottom - metrics.scrollbarSize,
  );
  expect(after.box.left).toBe(before.box.left);
});

test("F2 opens the cell with its text, and Escape leaves it as it was", async ({ page }) => {
  await open(page);
  const shown = await page.evaluate(() => window.__ime?.textAt(1, 3));
  expect(shown).toBeTruthy();

  await page.keyboard.press("F2");
  expect(await state(page)).toMatchObject({
    editing: { row: 1, col: 3 },
    shown: true,
    text: shown,
  });
  // The caret is at the end.
  await page.keyboard.type("!");
  expect((await state(page)).text).toBe(`${shown}!`);

  await page.keyboard.press("Escape");
  expect(await state(page)).toMatchObject({
    editing: null,
    shown: false,
    text: "",
    selection: { row: 1, col: 3 },
    committed: {},
  });
});

test("while an input method composes, keys are its own", async ({ page }) => {
  await open(page);
  const session = await page.context().newCDPSession(page);

  // The first key of a composition already shows the editor on the cell.
  await compose(session, "n");
  expect(await state(page)).toMatchObject({
    editing: { row: 1, col: 3 },
    composing: true,
    shown: true,
  });
  await compose(session, "ni");
  expect((await state(page)).text).toBe("ni");

  // Arrows walk the candidates, Enter accepts one, Escape drops the
  // composition: none of them is the grid's while composing.
  for (const key of ["ArrowDown", "ArrowRight", "Enter", "Escape", "Tab"]) {
    for (const shape of ["composing", "code 229"] as const) {
      await pressDuringComposition(page, key, shape);
      expect(await state(page), `${key}, ${shape}`).toMatchObject({
        editing: { row: 1, col: 3 },
        composing: true,
        text: "ni",
        selection: { row: 1, col: 3 },
        committed: {},
      });
    }
  }

  // The input method puts its text in: that ends the composition, not the edit.
  await session.send("Input.insertText", { text: "你" });
  expect(await state(page)).toMatchObject({
    editing: { row: 1, col: 3 },
    composing: false,
    text: "你",
    committed: {},
  });

  // A second composition continues the same edit.
  await compose(session, "hao");
  await session.send("Input.insertText", { text: "好" });
  expect((await state(page)).text).toBe("你好");

  // Now Enter is ours again.
  await page.keyboard.press("Enter");
  expect(await state(page)).toMatchObject({
    editing: null,
    selection: { row: 2, col: 3 },
    committed: { D2: "你好" },
  });
});

test("a composition that is dropped leaves the edit open and empty", async ({ page }) => {
  await open(page);
  const session = await page.context().newCDPSession(page);
  await compose(session, "ni");
  // Composing nothing is how an input method takes its text back.
  await compose(session, "");
  expect(await state(page)).toMatchObject({
    editing: { row: 1, col: 3 },
    composing: false,
    text: "",
  });
  // With nothing being composed, Escape ends the edit.
  await page.keyboard.press("Escape");
  expect(await state(page)).toMatchObject({ editing: null, committed: {} });
});

test("clicking another cell accepts the edit and selects that cell", async ({ page }) => {
  await open(page);
  await page.keyboard.type("abc");
  const { box } = await state(page);
  const metrics = await page.evaluate(() => window.__ime?.grid().metrics);
  if (!metrics) {
    throw new Error("the page's hooks are not installed");
  }
  // Two columns to the right and three rows down from the cell being edited.
  await page.mouse.click(box.left + metrics.colWidth * 2.5, box.top + metrics.rowHeight * 3.5);
  await page.waitForFunction(() => window.__ime?.state().editing === null);
  expect(await state(page)).toMatchObject({
    selection: { row: 4, col: 5 },
    committed: { D2: "abc" },
    shown: false,
    text: "",
  });
  // The keyboard is still the editor's.
  await page.keyboard.press("z");
  expect(await state(page)).toMatchObject({ editing: { row: 4, col: 5 }, text: "z" });
});

test("clipboard shortcuts reach the editor and leave it empty", async ({ page }) => {
  await open(page);
  for (const key of ["Control+C", "Control+X", "Control+V"]) {
    await page.keyboard.press(key);
  }
  const clipboard = await page.evaluate(() =>
    (window.__ime?.events ?? []).flatMap((event) =>
      event.kind === "clipboard" ? [event.type] : [],
    ),
  );
  expect(clipboard).toEqual(["copy", "cut", "paste"]);
  expect(await state(page)).toMatchObject({ editing: null, text: "" });
});
