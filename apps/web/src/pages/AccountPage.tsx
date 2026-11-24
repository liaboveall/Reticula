import {
  type AccountSummary,
  ApiError,
  account,
  type DevAccount,
  deleteAccount,
  devLogin,
  githubLoginUrl,
  logout,
} from "@reticula/protocol/collab-api";
import { useCallback, useEffect, useState } from "react";
import { fmtBytes, fmtInt, t } from "../i18n/index.ts";
import { tx } from "../i18n/react.ts";
import { apiErrorMessage } from "./helpers.ts";
import { LanguageSwitch } from "./LanguageSwitch.tsx";
import { PageNav } from "./PageNav.tsx";
import "../collab/collab.css";
import "./pages.css";

export interface AccountPageProps {
  readonly onNavigate: (path: string) => void;
}

/** Kept as it happened, for the language may be switched while it is shown. */
interface Failure {
  readonly cause: unknown;
  readonly what: "load" | "signIn" | "delete";
}

function failureText({ cause, what }: Failure): string {
  switch (what) {
    case "load":
      return apiErrorMessage(cause, t("Could not load account summary"));
    case "signIn":
      return apiErrorMessage(cause, t("Sign-in failed"));
    case "delete":
      return apiErrorMessage(cause, t("Could not delete account"));
  }
}

export default function AccountPage({ onNavigate }: AccountPageProps) {
  const [summary, setSummary] = useState<AccountSummary | null>(null);
  const [loading, setLoading] = useState(true);
  const [confirmText, setConfirmText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Failure | null>(null);

  const loadAccount = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setSummary(await account());
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        setSummary(null);
      } else {
        setError({ cause: err, what: "load" });
      }
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadAccount();
  }, [loadAccount]);

  const handleDevSignIn = async (acct: DevAccount) => {
    setBusy(true);
    setError(null);
    try {
      await devLogin({ account: acct });
      await loadAccount();
    } catch (err) {
      setError({ cause: err, what: "signIn" });
    } finally {
      setBusy(false);
    }
  };

  const handleLogout = async () => {
    setBusy(true);
    try {
      await logout();
    } catch {
      // Clear local view anyway.
    }
    try {
      window.localStorage.removeItem("reticula:collab-cache:v1");
    } catch {
      // Ignore storage access errors.
    }
    setSummary(null);
    setBusy(false);
  };

  const handleDeleteAccount = async () => {
    if (confirmText.trim() !== "DELETE") {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await deleteAccount({ confirm: "DELETE" });
      try {
        window.localStorage.removeItem("reticula:collab-cache:v1");
      } catch {
        // Ignore storage access errors.
      }
      onNavigate("/");
    } catch (err) {
      setError({ cause: err, what: "delete" });
    } finally {
      setBusy(false);
    }
  };

  const pct =
    summary && summary.quotaBytes > 0
      ? Math.min(100, Math.round((summary.storageBytes / summary.quotaBytes) * 100))
      : 0;

  return (
    <div className="page-shell" data-testid="account-page">
      <PageNav
        onNavigate={onNavigate}
        links={[
          ["/workbooks", t("Workbooks")],
          ["/app", t("Editor")],
          ["/privacy", t("Privacy")],
        ]}
      />

      <main className="page-main">
        <div className="page-card">
          <h1>{t("Account & Storage")}</h1>
          <LanguageSwitch />
          {error && (
            <div className="collab-error" role="alert">
              {failureText(error)}
            </div>
          )}
          {loading ? (
            <p className="page-muted">{t("Loading account…")}</p>
          ) : !summary ? (
            <div className="collab-section">
              <p className="page-muted">
                {t("Not signed in. Sign in to view cloud storage usage or manage data.")}
              </p>
              <div className="collab-row">
                {(["alice", "bob", "carol"] as const).map((acct) => (
                  <button
                    key={acct}
                    type="button"
                    className="toolbar-btn"
                    disabled={busy}
                    onClick={() => void handleDevSignIn(acct)}
                  >
                    {t("Dev: {account}", { account: acct })}
                  </button>
                ))}
                <a className="toolbar-btn primary-btn" href={githubLoginUrl("/account")}>
                  {t("Sign in with GitHub")}
                </a>
              </div>
            </div>
          ) : (
            <>
              <div className="collab-row" style={{ justifyContent: "space-between" }}>
                <div>
                  <strong>{summary.user.name}</strong>
                  <div className="page-muted">
                    {t("Owned workbooks: {owned} · Shared workbooks: {shared}", {
                      owned: summary.ownedWorkbooks,
                      shared: summary.sharedWorkbooks,
                    })}
                  </div>
                </div>
                <button
                  type="button"
                  className="toolbar-btn"
                  disabled={busy}
                  onClick={() => void handleLogout()}
                >
                  {t("Sign Out")}
                </button>
              </div>

              <div className="collab-section">
                <p className="collab-section-title">{t("Cloud Storage Usage")}</p>
                <div className="page-quota-bar">
                  <div className="page-quota-fill" style={{ width: `${pct}%` }} />
                </div>
                <span className="page-muted" data-testid="account-storage-usage">
                  {t("{used} used of {quota} ({percent}%)", {
                    used: fmtBytes(summary.storageBytes),
                    quota: fmtBytes(summary.quotaBytes),
                    percent: pct,
                  })}
                </span>
                {summary.diskOverHardLimit && (
                  <div className="collab-error" role="alert">
                    {t(
                      "Server disk volume is full; creating or importing new cloud workbooks is paused.",
                    )}
                  </div>
                )}
                {!summary.diskOverHardLimit && summary.diskOverSoftLimit && (
                  <p className="page-muted">
                    {t("Server storage volume is above the 80% warning threshold.")}
                  </p>
                )}
              </div>

              <div className="collab-section" data-testid="account-ai-quota">
                <p className="collab-section-title">{t("Daily AI Analyst Quota")}</p>
                <span className="page-muted">
                  {t(
                    "Status: {status} · Runs today: {runs} / {runLimit} · Tokens today: {tokens} / {tokenLimit}",
                    {
                      status: summary.aiEnabled
                        ? t("Enabled")
                        : t("Demo replays only (cloud LLM not configured)"),
                      runs: summary.aiRunsToday,
                      runLimit: summary.aiRunLimitDaily,
                      tokens: fmtInt(summary.aiTokensUsedToday),
                      tokenLimit: fmtInt(summary.aiTokenLimitDaily),
                    },
                  )}
                </span>
              </div>

              <div className="page-card page-danger">
                <h2>{t("Delete Account")}</h2>
                <p className="page-muted">
                  {tx(
                    "Permanently deletes your user profile, sessions, GitHub link, and all cloud workbooks you own (including snapshots). Contributions you made inside workbooks owned by others remain intact under “Deleted user” so collaborator history stays valid. Type <0>DELETE</0> below to confirm.",
                    { 0: <code /> },
                  )}
                </p>
                <div className="collab-row">
                  <input
                    type="text"
                    className="collab-input"
                    data-testid="delete-account-confirm-input"
                    placeholder={t('Type "DELETE"')}
                    value={confirmText}
                    onChange={(e) => setConfirmText(e.target.value)}
                  />
                  <button
                    type="button"
                    className="toolbar-btn"
                    data-testid="delete-account-btn"
                    disabled={busy || confirmText.trim() !== "DELETE"}
                    onClick={() => void handleDeleteAccount()}
                  >
                    {t("Permanently Delete Account")}
                  </button>
                </div>
              </div>
            </>
          )}
        </div>
      </main>
    </div>
  );
}
