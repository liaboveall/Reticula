import { startTransition, useCallback, useEffect, useRef, useState } from "react";
import { ErrorBoundary } from "./ErrorBoundary.tsx";
import { prefetchEditorWhenIdle, preloadEditor } from "./preload.ts";
import { type AppRoute, normalizeAppRoute } from "./routes.ts";
import { loadPage, type Page } from "./routes-load.ts";
import type { LoadRecovery } from "./routes-recovery.ts";

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
  const [shown, setShown] = useState(first);
  const [pending, setPending] = useState(false);
  const latest = useRef(0);

  // The page in view stays until the next one is loaded, and React draws the
  // next one in a transition, so no moment in between shows neither.
  const show = useCallback(
    (route: AppRoute, target: string | null) => {
      const turn = ++latest.current;
      if (route === "app") {
        preloadEditor();
      }
      setPending(true);
      loadPage(route).then(
        (Page) => {
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
    const onPopState = () => show(normalizeAppRoute(window.location.pathname), null);
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, [show]);

  useEffect(
    () => (shown.route === "landing" ? prefetchEditorWhenIdle() : undefined),
    [shown.route],
  );

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
      {pending && <div className="route-pending" role="status" aria-label="Loading page" />}
      <ErrorBoundary key={shown.route} recovery={recovery}>
        <Page onNavigate={navigate} />
      </ErrorBoundary>
    </>
  );
}
