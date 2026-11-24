import { Component, type ErrorInfo, type ReactNode, useSyncExternalStore } from "react";
import { t } from "./i18n/index.ts";
import { useLocale } from "./i18n/react.ts";
import type { LoadRecovery, RecoveryState } from "./routes-recovery.ts";
import "./ErrorBoundary.css";

type ProblemKind = "stale" | "offline" | "crashed";

interface ProblemText {
  readonly title: string;
  readonly text: string;
}

function problemText(kind: ProblemKind): ProblemText {
  switch (kind) {
    case "stale":
      return {
        title: t("A newer version of Reticula is available"),
        text: t(
          "This page comes from an older version, and a part of it could not be loaded. Reload to continue.",
        ),
      };
    case "offline":
      return {
        title: t("A part of Reticula could not be loaded"),
        text: t("You appear to be offline. Reconnect, then reload."),
      };
    case "crashed":
      return {
        title: t("Something went wrong"),
        text: t(
          "The page ran into an error. Workbooks already saved in this browser are not affected. Reload to continue.",
        ),
      };
  }
}

interface ProblemProps {
  readonly kind: ProblemKind;
  /** Beside a page that still works, instead of in place of one. */
  readonly banner?: boolean;
}

function Problem({ kind, banner = false }: ProblemProps) {
  // The page around it, which follows the language otherwise, may be what failed.
  useLocale();
  const { title, text } = problemText(kind);
  return (
    <div
      className={banner ? "load-problem load-problem-banner" : "load-problem"}
      role="alert"
      data-testid="load-problem"
    >
      <strong>{title}</strong>
      <p>{text}</p>
      <button type="button" onClick={() => window.location.reload()}>
        {t("Reload")}
      </button>
    </div>
  );
}

function shownProblem(state: RecoveryState, banner: boolean): ProblemKind | null {
  switch (state) {
    case "stale":
    case "offline":
      return state;
    case "reloading":
      return null;
    case "ok":
      // In place of a page there is nothing else to show, even after something
      // else has loaded since.
      return banner ? null : "stale";
  }
}

interface LoadProblemProps {
  readonly recovery: LoadRecovery;
  readonly banner?: boolean;
}

/** Says that code is missing, unless the page is being reloaded to get it. */
export function LoadProblem({ recovery, banner = false }: LoadProblemProps) {
  const kind = shownProblem(useSyncExternalStore(recovery.subscribe, recovery.state), banner);
  return kind === null ? null : <Problem kind={kind} banner={banner} />;
}

interface ErrorBoundaryProps {
  readonly recovery: LoadRecovery;
  readonly children: ReactNode;
}

interface ErrorBoundaryState {
  readonly failed: boolean;
  readonly error: unknown;
}

/**
 * The last resort for an error that no page handles: a message instead of a
 * blank page. While there is a page, it also says when a part of it that
 * nobody was waiting for could not be loaded.
 */
export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  override state: ErrorBoundaryState = { failed: false, error: null };

  static getDerivedStateFromError(error: unknown): ErrorBoundaryState {
    return { failed: true, error };
  }

  override componentDidCatch(error: unknown, _info: ErrorInfo): void {
    if (this.props.recovery.isLoadFailure(error)) {
      this.props.recovery.recover();
    }
  }

  override render(): ReactNode {
    const { recovery, children } = this.props;
    if (!this.state.failed) {
      return (
        <>
          {children}
          <LoadProblem recovery={recovery} banner />
        </>
      );
    }
    return recovery.isLoadFailure(this.state.error) ? (
      <LoadProblem recovery={recovery} />
    ) : (
      <Problem kind="crashed" />
    );
  }
}
