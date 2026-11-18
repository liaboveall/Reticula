import { useEffect, useState } from "react";
import { EditorPage } from "./EditorPage.tsx";
import { LandingPage } from "./LandingPage.tsx";

function normalizeRoute(pathname: string): "landing" | "app" {
  return pathname === "/app" || pathname.startsWith("/app/") ? "app" : "landing";
}

export function App() {
  const [route, setRoute] = useState<"landing" | "app">(() =>
    normalizeRoute(window.location.pathname),
  );

  useEffect(() => {
    const onPopState = () => setRoute(normalizeRoute(window.location.pathname));
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, []);

  const navigate = (path: "/" | "/app") => {
    const target = `${path}${window.location.search}`;
    if (`${window.location.pathname}${window.location.search}` !== target) {
      window.history.pushState(null, "", target);
    }
    setRoute(normalizeRoute(path));
  };

  if (route === "app") {
    return <EditorPage onNavigateHome={() => navigate("/")} />;
  }
  return <LandingPage onOpenApp={() => navigate("/app")} />;
}
