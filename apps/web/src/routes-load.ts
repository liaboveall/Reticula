import type { ComponentType } from "react";
import type { AppRoute } from "./routes.ts";

export interface PageProps {
  readonly onNavigate: (path: string) => void;
}

export type Page = ComponentType<PageProps>;

// Each page is a chunk of its own. `crates/xtask/src/web/bundle.rs` lists the
// same modules: it preloads them and holds them to their size budgets.
const PAGES: Readonly<Record<AppRoute, () => Promise<Page>>> = {
  landing: () => import("./LandingPage.tsx").then((module) => module.LandingPage),
  app: () => import("./EditorPage.tsx").then((module) => module.EditorPage),
  workbooks: () => import("./pages/WorkbooksPage.tsx").then((module) => module.default),
  account: () => import("./pages/AccountPage.tsx").then((module) => module.default),
  privacy: () => import("./pages/PrivacyPage.tsx").then((module) => module.default),
  sim: () => import("./sim/SimLabPage.tsx").then((module) => module.default),
  benchmarks: () => import("./pages/BenchmarksPage.tsx").then((module) => module.default),
};

/** Loads a page's code. Once loaded it stays loaded, and this resolves at once. */
export function loadPage(route: AppRoute): Promise<Page> {
  return PAGES[route]();
}
