import { startTransition, useCallback, useEffect, useRef, useState } from "react";
import { ErrorBoundary } from "./ErrorBoundary.tsx";
import { enterRoute, t } from "./i18n/index.ts";
import { useLocale } from "./i18n/react.ts";
import { preloadEditor } from "./preload.ts";
import { type AppRoute, normalizeAppRoute } from "./routes.ts";
import { loadPage, type Page } from "./routes-load.ts";
import type { LoadRecovery } from "./routes-recovery.ts";
import { LiveRegions } from "./ui/LiveRegions.tsx";
import { focusPage, pageTitle } from "./ui/page.ts";

interface ShownPage {
  readonly route: AppRoute;
  readonly Page: Page;
}

export interface AppProps {
  /** The page the address asked for, loaded before anything is rendered. */
  readonly first: ShownPage;
  readonly recovery: LoadRecovery;
}

/** The address a navigation leads to: opening the editor keeps the options of the page it is opened from. */
function resolveTarget(path: string): string {
  if (path === "/app") {
    const search = new URLSearchParams(window.location.search);
    if (search.has("hooks")) {
      return `/app?hooks=${encodeURIComponent(search.get("hooks") ?? "1")}`;
    }
    if (window.location.pathname === "/" && window.location.search) {
      return `/app${window.location.search}`;
    }
  }
  return path;
}

export function App({ first, recovery }: AppProps) {
  // The pages are rendered again in another language from here.
  useLocale();
  const [shown, setShown] = useState(first);
  const [pending, setPending] = useState(false);
  const latest = useRef(0);
  const focused = useRef(first.route);
  const wentBack = useRef(false);

  // The page in view stays until the next one is loaded, and React draws the
  // next one in a transition, so no moment in between shows neither.
  const show = useCallback(
    (route: AppRoute, target: string | null) => {
      const turn = ++latest.current;
      if (route === "app") {
        preloadEditor();
      }
      setPending(true);
      Promise.all([loadPage(route), enterRoute(route)]).then(
        ([Page]) => {
          if (turn !== latest.current) {
            return;
          }
          if (
            target !== null &&
            target !== `${window.location.pathname}${window.location.search}`
          ) {
            window.history.pushState(null, "", target);
          }
          recovery.clear();
          wentBack.current = target === null;
          startTransition(() => {
            setShown((current) => (current.route === route ? current : { route, Page }));
            setPending(false);
          });
        },
        () => {
          if (turn === latest.current) {
            setPending(false);
            recovery.recover(target);
          }
        },
      );
    },
    [recovery],
  );

  useEffect(() => {
    document.title = pageTitle(shown.route);
  });

  // The link that led here is gone, and with it the focus.
  useEffect(() => {
    if (focused.current !== shown.route) {
      focused.current = shown.route;
      focusPage({ preventScroll: wentBack.current });
    }
  }, [shown.route]);

  useEffect(() => {
    const onPopState = () => show(normalizeAppRoute(window.location.pathname), null);
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, [show]);

  const navigate = useCallback(
    (path: string) => {
      const target = resolveTarget(path);
      show(normalizeAppRoute(target.split(/[?#]/)[0] ?? "/"), target);
    },
    [show],
  );

  const { Page } = shown;
  return (
    <>
      <button type="button" className="skip-button" onClick={() => focusPage()}>
        {shown.route === "app" ? t("Skip to the grid") : t("Skip to content")}
      </button>
      <LiveRegions />
      {pending && <div className="route-pending" role="status" aria-label={t("Loading page")} />}
      <ErrorBoundary key={shown.route} recovery={recovery}>
        <Page onNavigate={navigate} />
      </ErrorBoundary>
    </>
  );
}
