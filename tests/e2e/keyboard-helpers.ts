// Going through the page with the keyboard alone.
import { type BrowserContext, expect, type Locator, type Page } from "@playwright/test";

function browserName(context: BrowserContext): string {
  return context.browser()?.browserType().name() ?? "";
}

/** Presses Tab, so that it stops at every control: on macOS, WebKit leaves out buttons and links otherwise. */
export async function pressTab(page: Page, { back = false } = {}): Promise<void> {
  const skips = process.platform === "darwin" && browserName(page.context()) === "webkit";
  await page.keyboard.press(`${skips ? "Alt+" : ""}${back ? "Shift+" : ""}Tab`);
}

/** Presses Tab until a control has the focus. */
export async function tabTo(
  page: Page,
  control: Locator,
  { back = false, most = 60 } = {},
): Promise<void> {
  await expect(control).toBeVisible();
  if (
    process.platform === "win32" &&
    browserName(page.context()) === "webkit" &&
    (await control.evaluate((element) => element.tagName === "A"))
  ) {
    // This one stops at no link, whatever is pressed.
    await control.focus();
    return;
  }
  for (let presses = 0; presses < most; presses++) {
    if (await control.evaluate((element) => element === document.activeElement)) {
      return;
    }
    await pressTab(page, { back });
  }
  await expect(control).toBeFocused();
}

/** Goes to a control and presses it. */
export async function activate(page: Page, control: Locator, key = "Enter"): Promise<void> {
  await tabTo(page, control);
  await page.keyboard.press(key);
}

/** What is read out: the live region that is not behind a dialog. */
export async function announced(page: Page): Promise<string> {
  return page.evaluate(() => {
    const dialog = [...document.querySelectorAll("dialog:modal")].at(-1);
    const region = dialog
      ? dialog.querySelector(':scope > [role="status"]')
      : document.querySelector('[data-testid="page-status"]');
    return region?.textContent ?? "";
  });
}

/** The focus is somewhere in a part of the page. */
export async function expectFocusInside(container: Locator): Promise<void> {
  await expect
    .poll(() => container.evaluate((element) => element.contains(document.activeElement)))
    .toBe(true);
}

/** Every control in a part of the page says what it is for, as far as there are not too many of them. */
export async function expectNamed(container: Locator, most = 60): Promise<void> {
  const controls = container.locator("button, input, select, textarea, a[href]");
  for (const control of (await controls.filter({ visible: true }).all()).slice(0, most)) {
    await expect(control).toHaveAccessibleName(/\S/);
  }
  for (const dialog of await container.locator("dialog").all()) {
    await expect(dialog).toHaveAccessibleName(/\S/);
  }
}
