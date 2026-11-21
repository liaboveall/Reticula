import type { RemotePresenceOverlay } from "@reticula/grid";
import {
  ApiError,
  createWorkbook,
  type DevAccount,
  devLogin,
  githubLoginUrl,
  join,
  logout,
  me,
  type Role,
  share,
  type User,
  type WorkbookSummary,
  workbook,
} from "@reticula/protocol/collab-api";
import { useCallback, useEffect, useRef, useState } from "react";
import "./collab.css";

const CACHE_KEY = "reticula:collab-cache:v1";

const PEER_COLORS = ["#0f766e", "#b45309", "#6d28d9", "#be123c", "#1d4ed8", "#15803d"];

export interface CollabWorkbookTarget {
  readonly userId: string;
  readonly userDisplayName: string;
  readonly docId: string;
  readonly branchId: string;
  readonly title: string;
  readonly role: Role;
}

export interface CollabShellProps {
  readonly open: boolean;
  readonly initialCollabId: string | null;
  readonly initialJoinToken: string | null;
  readonly activeTarget: CollabWorkbookTarget | null;
  readonly connection: "offline" | "connecting" | "catchingUp" | "online";
  readonly pendingCount: number;
  readonly remotePresences: readonly RemotePresenceOverlay[];
  readonly notice: string | null;
  readonly unsyncedRescue: {
    readonly localDocId: string;
    readonly title: string;
  } | null;
  readonly onCloseModal: () => void;
  readonly onRequestOpenModal: () => void;
  readonly onOpenCollab: (target: CollabWorkbookTarget) => void;
  readonly onOpenLocalRescue: (localDocId: string, title: string) => void;
  readonly onSwitchToLocal: () => void;
}

interface CollabCache {
  user: User | null;
  workbooks: Record<string, WorkbookSummary>;
}

function readCache(): CollabCache {
  try {
    const raw = window.localStorage.getItem(CACHE_KEY);
    if (!raw) {
      return { user: null, workbooks: {} };
    }
    const parsed = JSON.parse(raw) as Partial<CollabCache>;
    return {
      user: parsed.user ?? null,
      workbooks: parsed.workbooks ?? {},
    };
  } catch {
    return { user: null, workbooks: {} };
  }
}

function writeCache(cache: CollabCache): void {
  try {
    window.localStorage.setItem(CACHE_KEY, JSON.stringify(cache));
  } catch {
    // Ignore private mode / quota errors on cache.
  }
}

function saveWorkbookToCache(user: User, summary: WorkbookSummary): void {
  const prev = readCache();
  writeCache({
    user,
    workbooks: {
      ...prev.workbooks,
      [summary.id]: summary,
    },
  });
}

function makeTarget(user: User, summary: WorkbookSummary): CollabWorkbookTarget {
  return {
    userId: user.id,
    userDisplayName: user.name,
    docId: summary.id,
    branchId: summary.branchId,
    title: summary.title,
    role: summary.role,
  };
}

function replaceUrlCollab(collabId: string): void {
  const cur = new URL(window.location.href);
  const next = new URL("/app", window.location.origin);
  next.searchParams.set("collab", collabId);
  if (cur.searchParams.has("hooks")) {
    next.searchParams.set("hooks", cur.searchParams.get("hooks") ?? "");
  }
  window.history.replaceState({}, "", `${next.pathname}${next.search}`);
}

function clearUrlCollabParams(): void {
  const cur = new URL(window.location.href);
  const next = new URL("/app", window.location.origin);
  if (cur.searchParams.has("hooks")) {
    next.searchParams.set("hooks", cur.searchParams.get("hooks") ?? "");
  }
  window.history.replaceState({}, "", `${next.pathname}${next.search}`);
}

function statusPillLabel(
  connection: "offline" | "connecting" | "catchingUp" | "online",
  pendingCount: number,
): string {
  if (connection === "online") {
    return pendingCount > 0 ? `Syncing (${pendingCount})` : "Synced";
  }
  if (connection === "connecting" || connection === "catchingUp") {
    return pendingCount > 0 ? `Syncing (${pendingCount})` : "Connecting…";
  }
  return pendingCount > 0 ? `Offline · ${pendingCount} unsent` : "Offline";
}

export default function CollabShell(props: CollabShellProps) {
  const {
    open,
    initialCollabId,
    initialJoinToken,
    activeTarget,
    connection,
    pendingCount,
    remotePresences,
    notice,
    unsyncedRescue,
    onCloseModal,
    onRequestOpenModal,
    onOpenCollab,
    onOpenLocalRescue,
    onSwitchToLocal,
  } = props;

  const [user, setUser] = useState<User | null>(() => readCache().user);
  const [loadingAuth, setLoadingAuth] = useState(true);
  const [devLoginAvailable, setDevLoginAvailable] = useState(true);
  const [newTitle, setNewTitle] = useState("Cloud Workbook");
  const [singleUseShare, setSingleUseShare] = useState(false);
  const [shareUrl, setShareUrl] = useState<string | null>(null);
  const [copiedShare, setCopiedShare] = useState(false);
  const [busy, setBusy] = useState(false);
  const [authError, setAuthError] = useState<string | null>(null);
  const [joinError, setJoinError] = useState<string | null>(null);
  const [accessError, setAccessError] = useState<string | null>(null);

  const pendingJoinTokenRef = useRef<string | null>(initialJoinToken);
  const resolvedBootRef = useRef(false);

  const openAfterAuth = useCallback(
    async (signedInUser: User) => {
      const joinToken = pendingJoinTokenRef.current;
      if (joinToken) {
        setBusy(true);
        setJoinError(null);
        try {
          const joined = await join(joinToken);
          const summary = await workbook(joined.id);
          pendingJoinTokenRef.current = null;
          saveWorkbookToCache(signedInUser, summary);
          replaceUrlCollab(summary.id);
          onOpenCollab(makeTarget(signedInUser, summary));
          onCloseModal();
        } catch (err) {
          pendingJoinTokenRef.current = null;
          if (err instanceof ApiError) {
            setJoinError(`Share link rejected (HTTP ${err.status}).`);
          } else {
            setJoinError("Could not redeem share link right now.");
          }
          onRequestOpenModal();
        } finally {
          setBusy(false);
        }
        return;
      }

      if (initialCollabId && activeTarget?.docId !== initialCollabId) {
        setBusy(true);
        setAccessError(null);
        try {
          const summary = await workbook(initialCollabId);
          saveWorkbookToCache(signedInUser, summary);
          onOpenCollab(makeTarget(signedInUser, summary));
          onCloseModal();
        } catch (err) {
          if (err instanceof ApiError && (err.status === 403 || err.status === 404)) {
            setAccessError("You do not have permission to open this collaborative workbook.");
            onRequestOpenModal();
          } else if (err instanceof ApiError && err.status === 401) {
            setUser(null);
            onRequestOpenModal();
          } else {
            const cached = readCache().workbooks[initialCollabId];
            if (cached) {
              onOpenCollab(makeTarget(signedInUser, cached));
              onCloseModal();
            } else {
              setAccessError("Could not load collaborative workbook metadata.");
              onRequestOpenModal();
            }
          }
        } finally {
          setBusy(false);
        }
      }
    },
    [activeTarget?.docId, initialCollabId, onCloseModal, onOpenCollab, onRequestOpenModal],
  );

  useEffect(() => {
    if (resolvedBootRef.current) {
      return;
    }
    resolvedBootRef.current = true;
    let cancelled = false;
    (async () => {
      setLoadingAuth(true);
      try {
        const currentUser = await me();
        if (cancelled) {
          return;
        }
        setUser(currentUser);
        const cache = readCache();
        writeCache({ ...cache, user: currentUser });
        setLoadingAuth(false);
        await openAfterAuth(currentUser);
      } catch (err) {
        if (cancelled) {
          return;
        }
        setLoadingAuth(false);
        if (err instanceof ApiError && err.status === 401) {
          setUser(null);
          if (pendingJoinTokenRef.current || initialCollabId) {
            onRequestOpenModal();
          }
          return;
        }
        // Network error / offline: allow cached offline collaborative open.
        const cache = readCache();
        if (pendingJoinTokenRef.current) {
          setJoinError("Offline: a network connection is required to redeem a share link.");
          onRequestOpenModal();
        } else if (initialCollabId && cache.user && cache.workbooks[initialCollabId]) {
          setUser(cache.user);
          onOpenCollab(makeTarget(cache.user, cache.workbooks[initialCollabId]));
        } else if (initialCollabId) {
          setAccessError(
            "Offline: this collaborative workbook has not been opened on this device yet.",
          );
          onRequestOpenModal();
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [initialCollabId, onOpenCollab, onRequestOpenModal, openAfterAuth]);

  const handleDevSignIn = async (account: DevAccount) => {
    setBusy(true);
    setAuthError(null);
    try {
      const signedIn = await devLogin({ account });
      setUser(signedIn);
      const cache = readCache();
      writeCache({ ...cache, user: signedIn });
      await openAfterAuth(signedIn);
    } catch (err) {
      if (err instanceof ApiError && err.status === 404) {
        setDevLoginAvailable(false);
        setAuthError("Loopback dev sign-in is disabled on this server. Use GitHub sign-in.");
      } else if (err instanceof ApiError) {
        setAuthError(`Sign-in failed (HTTP ${err.status}).`);
      } else {
        setAuthError("Sign-in failed.");
      }
    } finally {
      setBusy(false);
    }
  };

  const handleLogout = async () => {
    setBusy(true);
    try {
      await logout();
    } catch {
      // Clear local user view even if already expired.
    }
    setUser(null);
    setShareUrl(null);
    const cache = readCache();
    writeCache({ ...cache, user: null });
    if (activeTarget) {
      clearUrlCollabParams();
      onSwitchToLocal();
    }
    setBusy(false);
  };

  const handleCreateCloudSheet = async () => {
    if (!user) {
      return;
    }
    setBusy(true);
    setAuthError(null);
    setAccessError(null);
    setJoinError(null);
    try {
      const created = await createWorkbook({ title: newTitle.trim() || "Cloud Workbook" });
      const summary = await workbook(created.id);
      saveWorkbookToCache(user, summary);
      setShareUrl(null);
      replaceUrlCollab(summary.id);
      onOpenCollab(makeTarget(user, summary));
      onCloseModal();
    } catch (err) {
      setAuthError(
        err instanceof ApiError
          ? `Failed to create cloud workbook (HTTP ${err.status}).`
          : "Failed to create cloud workbook.",
      );
    } finally {
      setBusy(false);
    }
  };

  const handleCreateShareLink = async () => {
    if (!activeTarget) {
      return;
    }
    setBusy(true);
    setCopiedShare(false);
    try {
      const created = await share(activeTarget.docId, { singleUse: singleUseShare });
      setShareUrl(`${window.location.origin}${created.url}`);
    } catch (err) {
      setAuthError(
        err instanceof ApiError
          ? `Could not create share link (HTTP ${err.status}).`
          : "Could not create share link.",
      );
    } finally {
      setBusy(false);
    }
  };

  const handleCopyShareUrl = async () => {
    if (!shareUrl) {
      return;
    }
    try {
      await navigator.clipboard.writeText(shareUrl);
      setCopiedShare(true);
    } catch {
      // Fallback: user can still copy directly from input.
    }
  };

  const handleBackToLocal = () => {
    clearUrlCollabParams();
    setShareUrl(null);
    setAccessError(null);
    setJoinError(null);
    onSwitchToLocal();
    onCloseModal();
  };

  const showModal = open || Boolean(joinError) || Boolean(accessError);

  return (
    <>
      {(activeTarget || notice) && (
        <div className="collab-strip" data-testid="collab-strip">
          {activeTarget && (
            <span
              className="collab-status-pill"
              data-testid="collab-status-pill"
              data-connection={connection}
            >
              {statusPillLabel(connection, pendingCount)}
            </span>
          )}
          {remotePresences.length > 0 && (
            <div className="collab-peers" data-testid="collab-peers">
              {remotePresences.map((peer) => (
                <span
                  key={peer.replica}
                  className="collab-peer-chip"
                  data-testid={`collab-peer-${peer.replica}`}
                >
                  <span
                    className="collab-peer-dot"
                    style={{ background: PEER_COLORS[peer.colorSlot % PEER_COLORS.length] }}
                  />
                  {peer.displayName}
                </span>
              ))}
            </div>
          )}
          {notice && (
            <span className="collab-notice" data-testid="collab-notice">
              {notice}
            </span>
          )}
        </div>
      )}

      {unsyncedRescue && (
        <div className="collab-unsynced-banner" data-testid="unsynced-banner" role="alert">
          <span>
            Server rejected incompatible pending edits; a local backup &ldquo;
            {unsyncedRescue.title}&rdquo; was saved.
          </span>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="open-unsynced-btn"
            onClick={() => onOpenLocalRescue(unsyncedRescue.localDocId, unsyncedRescue.title)}
          >
            Open Local Copy
          </button>
        </div>
      )}

      {showModal && (
        <div className="modal-backdrop" role="presentation">
          <div
            className="modal-card collab-modal"
            role="dialog"
            aria-modal="true"
            aria-label="Cloud Collaboration"
            data-testid="collab-modal"
          >
            <div className="modal-header">
              <h2>Cloud Collaboration</h2>
              <button
                type="button"
                className="tip-close"
                aria-label="Close"
                onClick={() => {
                  setJoinError(null);
                  setAccessError(null);
                  onCloseModal();
                }}
              >
                ×
              </button>
            </div>

            {joinError && (
              <div className="collab-error" data-testid="collab-join-error" role="alert">
                {joinError}
              </div>
            )}

            {accessError && (
              <div className="collab-error" data-testid="collab-access-denied" role="alert">
                {accessError}
              </div>
            )}

            {authError && (
              <div className="collab-error" data-testid="collab-auth-error" role="alert">
                {authError}
              </div>
            )}

            {loadingAuth ? (
              <p className="collab-notice">Checking sign-in status…</p>
            ) : !user ? (
              <div className="collab-section" data-testid="collab-auth-panel">
                <p className="collab-section-title">Sign In Required</p>
                <p className="collab-notice">
                  {pendingJoinTokenRef.current
                    ? "Sign in to redeem this editor invite link."
                    : "Sign in to create or open collaborative cloud workbooks."}
                </p>
                {devLoginAvailable && (
                  <div className="collab-row">
                    <button
                      type="button"
                      className="toolbar-btn primary-btn"
                      data-testid="dev-login-alice"
                      disabled={busy}
                      onClick={() => void handleDevSignIn("alice")}
                    >
                      Dev Sign In as Alice
                    </button>
                    <button
                      type="button"
                      className="toolbar-btn"
                      data-testid="dev-login-bob"
                      disabled={busy}
                      onClick={() => void handleDevSignIn("bob")}
                    >
                      Dev Sign In as Bob
                    </button>
                  </div>
                )}
                <div className="collab-row">
                  <a
                    className="toolbar-btn"
                    data-testid="github-login-link"
                    href={githubLoginUrl(window.location.pathname + window.location.search)}
                  >
                    Sign in with GitHub
                  </a>
                </div>
              </div>
            ) : (
              <>
                <div className="collab-section">
                  <div className="collab-row" style={{ justifyContent: "space-between" }}>
                    <span data-testid="collab-user-badge">
                      Signed in as <strong>{user.name}</strong>
                    </span>
                    <button
                      type="button"
                      className="toolbar-btn"
                      data-testid="collab-logout-btn"
                      disabled={busy}
                      onClick={() => void handleLogout()}
                    >
                      Sign Out
                    </button>
                  </div>
                </div>

                <div className="collab-section">
                  <p className="collab-section-title">New Cloud Workbook</p>
                  <div className="collab-row">
                    <input
                      type="text"
                      className="collab-input"
                      data-testid="new-cloud-title-input"
                      value={newTitle}
                      onChange={(e) => setNewTitle(e.target.value)}
                      placeholder="Workbook title"
                    />
                    <button
                      type="button"
                      className="toolbar-btn primary-btn"
                      data-testid="new-cloud-sheet-btn"
                      disabled={busy}
                      onClick={() => void handleCreateCloudSheet()}
                    >
                      + New Cloud Sheet
                    </button>
                  </div>
                </div>

                {activeTarget && (
                  <div className="collab-section">
                    <p className="collab-section-title">Current Cloud Sheet</p>
                    <div className="collab-row" style={{ justifyContent: "space-between" }}>
                      <span>
                        <strong>{activeTarget.title}</strong> ({activeTarget.role})
                      </span>
                    </div>
                    {activeTarget.role === "owner" && (
                      <>
                        <div className="collab-row">
                          <label className="collab-checkbox">
                            <input
                              type="checkbox"
                              data-testid="share-single-use-toggle"
                              checked={singleUseShare}
                              onChange={(e) => setSingleUseShare(e.target.checked)}
                            />
                            Single-use link
                          </label>
                          <button
                            type="button"
                            className="toolbar-btn"
                            data-testid="create-share-link-btn"
                            disabled={busy}
                            onClick={() => void handleCreateShareLink()}
                          >
                            Share Editor Link
                          </button>
                        </div>
                        {shareUrl && (
                          <div className="collab-link-box">
                            <input
                              type="text"
                              readOnly
                              className="collab-input"
                              data-testid="share-link-url"
                              value={shareUrl}
                              onFocus={(e) => e.currentTarget.select()}
                            />
                            <button
                              type="button"
                              className="toolbar-btn"
                              data-testid="copy-share-link-btn"
                              onClick={() => void handleCopyShareUrl()}
                            >
                              {copiedShare ? "Copied" : "Copy"}
                            </button>
                          </div>
                        )}
                      </>
                    )}
                  </div>
                )}
              </>
            )}

            <div className="modal-footer">
              {(activeTarget || joinError || accessError) && (
                <button
                  type="button"
                  className="toolbar-btn"
                  data-testid="back-to-local-btn"
                  onClick={handleBackToLocal}
                >
                  Back to Local Sheets
                </button>
              )}
              <button
                type="button"
                className="toolbar-btn"
                data-testid="collab-modal-close"
                onClick={() => {
                  setJoinError(null);
                  setAccessError(null);
                  onCloseModal();
                }}
              >
                Close
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
