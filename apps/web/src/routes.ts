/** The pages of the app. */
export type AppRoute =
  | "landing"
  | "app"
  | "workbooks"
  | "account"
  | "privacy"
  | "sim"
  | "benchmarks";

/**
 * The page an address shows. The inline script that preloads a page's code
 * before any module runs decides the same way (`crates/xtask/src/web/bundle.rs`).
 */
export function normalizeAppRoute(pathname: string): AppRoute {
  if (pathname === "/app" || pathname.startsWith("/app/") || pathname.startsWith("/join/")) {
    return "app";
  }
  if (pathname === "/workbooks" || pathname.startsWith("/workbooks/")) {
    return "workbooks";
  }
  if (pathname === "/account" || pathname.startsWith("/account/")) {
    return "account";
  }
  if (pathname === "/privacy" || pathname.startsWith("/privacy/")) {
    return "privacy";
  }
  if (pathname === "/sim" || pathname.startsWith("/sim/")) {
    return "sim";
  }
  if (pathname === "/benchmarks" || pathname.startsWith("/benchmarks/")) {
    return "benchmarks";
  }
  return "landing";
}
