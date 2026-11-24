import { expect, type Page } from "@playwright/test";

/**
 * Replaces the active cell's formula-bar contents and commits with `Enter`.
 *
 * Waits for the cell's raw inspection to finish and settles CodeMirror's focus
 * state before calling `fill()` so neither an in-flight `cellInspected` update
 * nor CodeMirror's focus sync can collapse Playwright's select-all to offset 0.
 */
export async function fillFormulaBar(page: Page, raw: string): Promise<void> {
  await expect(page.locator(".formulabar[data-inspected]")).toBeVisible();
  const formulaInput = page.getByTestId("formula-input");
  await expect(formulaInput).toHaveAttribute("contenteditable", "true");
  await formulaInput.click();
  await expect(page.locator(".formula-cm-wrap.focused")).toBeVisible();
  await formulaInput.fill(raw);
  await expect(formulaInput).toHaveText(raw);
  await formulaInput.press("Enter");
}
