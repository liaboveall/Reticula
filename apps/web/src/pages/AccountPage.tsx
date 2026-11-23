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
import { apiErrorMessage, formatBytes } from "./helpers.ts";
import "./pages.css";

export interface AccountPageProps {
  readonly onNavigate: (path: string) => void;
}

export default function AccountPage({ onNavigate }: AccountPageProps) {
  const [summary, setSummary] = useState<AccountSummary | null>(null);
  const [loading, setLoading] = useState(true);
  const [confirmText, setConfirmText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const loadAccount = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setSummary(await account());
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        setSummary(null);
      } else {
        setError(apiErrorMessage(err, "Could not load account summary"));
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
      setError(apiErrorMessage(err, "Sign-in failed"));
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
      setError(apiErrorMessage(err, "Could not delete account"));
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
      <nav className="page-nav">
        <a
          href="/"
          className="page-brand"
          onClick={(e) => {
            e.preventDefault();
            onNavigate("/");
          }}
        >
          Reticula
        </a>
        <div className="page-nav-links">
          <a
            href="/workbooks"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/workbooks");
            }}
          >
            Workbooks
          </a>
          <a
            href="/app"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/app");
            }}
          >
            Editor
          </a>
          <a
            href="/privacy"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/privacy");
            }}
          >
            Privacy
          </a>
        </div>
      </nav>

      <main className="page-main">
        <div className="page-card">
          <h1>Account &amp; Storage</h1>
          {error && (
            <div className="collab-error" role="alert">
              {error}
            </div>
          )}
          {loading ? (
            <p className="page-muted">Loading account…</p>
          ) : !summary ? (
            <div className="collab-section">
              <p className="page-muted">
                Not signed in. Sign in to view cloud storage usage or manage data.
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
                    Dev: {acct}
                  </button>
                ))}
                <a className="toolbar-btn primary-btn" href={githubLoginUrl("/account")}>
                  Sign in with GitHub
                </a>
              </div>
            </div>
          ) : (
            <>
              <div className="collab-row" style={{ justifyContent: "space-between" }}>
                <div>
                  <strong>{summary.user.name}</strong>
                  <div className="page-muted">
                    Owned workbooks: {summary.ownedWorkbooks} · Shared workbooks:{" "}
                    {summary.sharedWorkbooks}
                  </div>
                </div>
                <button
                  type="button"
                  className="toolbar-btn"
                  disabled={busy}
                  onClick={() => void handleLogout()}
                >
                  Sign Out
                </button>
              </div>

              <div className="collab-section">
                <p className="collab-section-title">Cloud Storage Usage</p>
                <div className="page-quota-bar">
                  <div className="page-quota-fill" style={{ width: `${pct}%` }} />
                </div>
                <span className="page-muted" data-testid="account-storage-usage">
                  {formatBytes(summary.storageBytes)} used of {formatBytes(summary.quotaBytes)} (
                  {pct}%)
                </span>
                {summary.diskOverHardLimit && (
                  <div className="collab-error" role="alert">
                    Server disk volume is full; creating or importing new cloud workbooks is paused.
                  </div>
                )}
                {!summary.diskOverHardLimit && summary.diskOverSoftLimit && (
                  <p className="page-muted">
                    Server storage volume is above the 80% warning threshold.
                  </p>
                )}
              </div>

              <div className="collab-section" data-testid="account-ai-quota">
                <p className="collab-section-title">Daily AI Analyst Quota</p>
                <span className="page-muted">
                  Status:{" "}
                  {summary.aiEnabled ? "Enabled" : "Demo replays only (cloud LLM not configured)"} ·{" "}
                  Runs today: {summary.aiRunsToday} / {summary.aiRunLimitDaily} · Tokens today:{" "}
                  {summary.aiTokensUsedToday.toLocaleString()} /{" "}
                  {summary.aiTokenLimitDaily.toLocaleString()}
                </span>
              </div>

              <div className="page-card page-danger">
                <h2>Delete Account</h2>
                <p className="page-muted">
                  Permanently deletes your user profile, sessions, GitHub link, and all cloud
                  workbooks you own (including snapshots). Contributions you made inside workbooks
                  owned by others remain intact under &ldquo;Deleted user&rdquo; so collaborator
                  history stays valid. Type <code>DELETE</code> below to confirm.
                </p>
                <div className="collab-row">
                  <input
                    type="text"
                    className="collab-input"
                    data-testid="delete-account-confirm-input"
                    placeholder='Type "DELETE"'
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
                    Permanently Delete Account
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
