import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App.tsx";
import { LoadProblem } from "./ErrorBoundary.tsx";
import { normalizeAppRoute } from "./routes.ts";
import { loadPage } from "./routes-load.ts";
import { startRecovery } from "./routes-recovery.ts";
import "@reticula/grid/grid.css";
import "./app.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("index.html has no #root element");
}
const root = createRoot(container);
const recovery = startRecovery(import.meta.url);

// The first page is loaded before React renders anything. Rendering first and
// suspending would cost the shell up to 300 ms: React holds a retried commit
// back that long. Awaiting it here at the top level is not an option either,
// because the pages' chunks import this one.
const route = normalizeAppRoute(window.location.pathname);
loadPage(route).then(
  (Page) =>
    root.render(
      <StrictMode>
        <App first={{ route, Page }} recovery={recovery} />
      </StrictMode>,
    ),
  () => {
    recovery.recover();
    root.render(<LoadProblem recovery={recovery} />);
  },
);
