import { t } from "../i18n/index.ts";
import type { AppRoute } from "../routes.ts";

/** What the browser's tab reads. */
export function pageTitle(route: AppRoute): string {
  switch (route) {
    case "landing":
      return t("Reticula: a local-first spreadsheet");
    case "app":
      return t("Reticula|title");
    case "workbooks":
      return t("{page} · Reticula", { page: t("Workbooks") });
    case "account":
      return t("{page} · Reticula", { page: t("Account") });
    case "privacy":
      return t("{page} · Reticula", { page: t("Privacy") });
    case "sim":
      return t("{page} · Reticula", { page: t("Sync Simulator") });
    case "benchmarks":
      return t("{page} · Reticula", { page: t("Benchmarks|page") });
  }
}

/**
 * Takes the keyboard to what the page is about: the grid, or else the heading,
 * which is read out then and where the Tab key goes on from.
 */
export function focusPage(options: FocusOptions = {}): void {
  const grid = document.querySelector<HTMLElement>(".sheet .reticula-grid");
  if (grid) {
    grid.focus({ preventScroll: true });
    return;
  }
  const heading = document.querySelector<HTMLElement>("main h1") ?? document.querySelector("main");
  if (heading instanceof HTMLElement) {
    heading.tabIndex = -1;
    heading.focus(options);
  }
}
