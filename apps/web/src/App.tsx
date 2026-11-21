import { lazy, Suspense, useEffect, useState } from "react";
import { EditorPage } from "./EditorPage.tsx";
import { LandingPage } from "./LandingPage.tsx";
import { type AppRoute, normalizeAppRoute } from "./pages/helpers.ts";

const LazyWorkbooksPage = lazy(() => import("./pages/WorkbooksPage.tsx"));
const LazyAccountPage = lazy(() => import("./pages/AccountPage.tsx"));
const LazyPrivacyPage = lazy(() => import("./pages/PrivacyPage.tsx"));

export function App() {
  const [route, setRoute] = useState<AppRoute>(() => normalizeAppRoute(window.location.pathname));

  useEffect(() => {
    const onPopState = () => setRoute(normalizeAppRoute(window.location.pathname));
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, []);

  const navigate = (pathWithSearch: string) => {
    const curParams = new URLSearchParams(window.location.search);
    let target = pathWithSearch;
    if (!pathWithSearch.includes("?") && pathWithSearch === "/app") {
      if (curParams.has("hooks")) {
        target = `/app?hooks=${encodeURIComponent(curParams.get("hooks") ?? "1")}`;
      } else if (window.location.pathname === "/" && window.location.search) {
        target = `/app${window.location.search}`;
      }
    }
    if (`${window.location.pathname}${window.location.search}` !== target) {
      window.history.pushState(null, "", target);
    }
    const cleanPath = target.split(/[?#]/)[0] ?? "/";
    setRoute(normalizeAppRoute(cleanPath));
  };

  if (route === "app") {
    return <EditorPage onNavigate={navigate} />;
  }
  if (route === "workbooks") {
    return (
      <Suspense fallback={<div className="page-loading">Loading…</div>}>
        <LazyWorkbooksPage onNavigate={navigate} />
      </Suspense>
    );
  }
  if (route === "account") {
    return (
      <Suspense fallback={<div className="page-loading">Loading…</div>}>
        <LazyAccountPage onNavigate={navigate} />
      </Suspense>
    );
  }
  if (route === "privacy") {
    return (
      <Suspense fallback={<div className="page-loading">Loading…</div>}>
        <LazyPrivacyPage onNavigate={navigate} />
      </Suspense>
    );
  }
  if (route === "sim") {
    return (
      <main style={{ padding: "32px", fontFamily: "system-ui, sans-serif" }}>
        <h1>Sync Simulator</h1>
        <p>Preparing simulator workspace…</p>
        <button type="button" onClick={() => navigate("/")}>
          Back to Home
        </button>
      </main>
    );
  }
  return <LandingPage onNavigate={navigate} />;
}
