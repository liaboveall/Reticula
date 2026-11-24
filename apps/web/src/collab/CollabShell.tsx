import type { RemotePresenceOverlay } from "@reticula/grid";
import {
  ApiError,
  createWorkbook,
  type DevAccount,
  devLogin,
  githubLoginUrl,
  join,
  listMembers,
  listShareLinks,
  logout,
  type MemberEntry,
  me,
  type Role,
  removeMember,
  revokeShareLink,
  type ShareLinkRecord,
  type ShareRole,
  share,
  type User,
  updateMember,
  updateWorkbook,
  type WorkbookSummary,
  type WorkbookVisibility,
  workbook,
} from "@reticula/protocol/collab-api";
import { lazy, Suspense, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { apiErrorMessage } from "../pages/helpers.ts";
import "./collab.css";

const LazyHistoryDrawer = lazy(() =>
  import("./HistoryDrawer.tsx").then((mod) => ({ default: mod.HistoryDrawer })),
);

const CACHE_KEY = "reticula:collab-cache:v1";

const PEER_COLORS = ["#0f766e", "#b45309", "#6d28d9", "#be123c", "#1d4ed8", "#15803d"];

export interface CollabWorkbookTarget {
  readonly userId: string;
  readonly userDisplayName: string;
  readonly docId: string;
  readonly branchId: string;
  readonly title: string;
  readonly role: Role;
  readonly shareToken?: string | null | undefined;
  readonly visibility?: WorkbookVisibility;
}

export interface CollabShellProps {
  readonly open: boolean;
  readonly initialCollabId: string | null;
  readonly initialJoinToken: string | null;
  readonly initialShareToken?: string | null | undefined;
  readonly activeTarget: CollabWorkbookTarget | null;
  readonly readOnly: boolean;
  readonly historyOpen: boolean;
  readonly historyPreviewSeq: string | null;
  readonly historyPreviewElapsedMs: number | null;
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
  readonly onUpdateVisibility: (visibility: WorkbookVisibility) => void;
  readonly onOpenLocalRescue: (localDocId: string, title: string) => void;
  readonly onSwitchToLocal: () => void;
  readonly onCloseHistory: () => void;
  readonly onHistorySeek: (targetSeq: string, snapshotSeq: string | null) => void;
  readonly onHistoryExit: () => void;
  readonly onNavigate: (path: string) => void;
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

function makeTarget(
  user: User,
  summary: WorkbookSummary,
  shareToken?: string | null,
): CollabWorkbookTarget {
  return {
    userId: user.id,
    userDisplayName: user.name,
    docId: summary.id,
    branchId: summary.branchId,
    title: summary.title,
    role: summary.role,
    visibility: summary.visibility,
    ...(shareToken ? { shareToken } : {}),
  };
}

function makeShareVisitorTarget(
  summary: WorkbookSummary,
  shareToken: string,
): CollabWorkbookTarget {
  return {
    userId: "",
    userDisplayName: "Visitor",
    docId: summary.id,
    branchId: summary.branchId,
    title: summary.title,
    role: "viewer",
    visibility: summary.visibility,
    shareToken,
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
    initialShareToken,
    activeTarget,
    readOnly,
    historyOpen,
    historyPreviewSeq,
    historyPreviewElapsedMs,
    connection,
    pendingCount,
    remotePresences,
    notice,
    unsyncedRescue,
    onCloseModal,
    onRequestOpenModal,
    onOpenCollab,
    onUpdateVisibility,
    onOpenLocalRescue,
    onSwitchToLocal,
    onCloseHistory,
    onHistorySeek,
    onHistoryExit,
    onNavigate,
  } = props;

  const [user, setUser] = useState<User | null>(() => readCache().user);
  const [loadingAuth, setLoadingAuth] = useState(true);
  const [devLoginAvailable, setDevLoginAvailable] = useState(true);
  const [newTitle, setNewTitle] = useState("Cloud Workbook");
  const [shareRole, setShareRole] = useState<ShareRole>("editor");
  const [shareLabel, setShareLabel] = useState("");
  const [shareTtlSeconds, setShareTtlSeconds] = useState<string>("");
  const [singleUseShare, setSingleUseShare] = useState(false);
  const [shareUrl, setShareUrl] = useState<string | null>(null);
  const [copiedShare, setCopiedShare] = useState(false);
  const [members, setMembers] = useState<MemberEntry[]>([]);
  const [shareLinks, setShareLinks] = useState<ShareLinkRecord[]>([]);
  const [busy, setBusy] = useState(false);
  const [authError, setAuthError] = useState<string | null>(null);
  const [joinError, setJoinError] = useState<string | null>(null);
  const [accessError, setAccessError] = useState<string | null>(null);

  const pendingJoinTokenRef = useRef<string | null>(initialJoinToken);

  const refreshGovernance = useCallback(async (docId: string, role: Role, signal?: AbortSignal) => {
    try {
      const [memberRows, linkRows] = await Promise.all([
        listMembers(docId, signal),
        role === "owner" ? listShareLinks(docId, signal) : Promise.resolve([]),
      ]);
      if (signal?.aborted) {
        return;
      }
      setMembers(memberRows);
      setShareLinks(linkRows);
    } catch {
      // Ignore transient fetch errors when inspecting member lists.
    }
  }, []);

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
          if (
            err instanceof ApiError &&
            (err.status === 401 || err.status === 403 || err.status === 404) &&
            initialShareToken
          ) {
            try {
              const summary = await workbook(initialCollabId, undefined, {
                xReticulaShare: initialShareToken,
              });
              onOpenCollab(makeTarget(signedInUser, summary, initialShareToken));
              onCloseModal();
              return;
            } catch {
              // Fall through to access error reporting below.
            }
          }
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
    [
      activeTarget?.docId,
      initialCollabId,
      initialShareToken,
      onCloseModal,
      onOpenCollab,
      onRequestOpenModal,
    ],
  );

  // The sign-in check runs once per mount and only unmounting discards its
  // answer. It reads the latest callbacks through a ref: had they been
  // dependencies, a parent re-render while the request was in flight would
  // have discarded the answer and left "Checking sign-in status…" forever.
  const bootRef = useRef({
    initialCollabId,
    initialShareToken,
    onOpenCollab,
    onRequestOpenModal,
    openAfterAuth,
  });
  useLayoutEffect(() => {
    bootRef.current = {
      initialCollabId,
      initialShareToken,
      onOpenCollab,
      onRequestOpenModal,
      openAfterAuth,
    };
  });

  useEffect(() => {
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
        await bootRef.current.openAfterAuth(currentUser);
      } catch (err) {
        if (cancelled) {
          return;
        }
        const {
          initialCollabId: collabId,
          initialShareToken: shareTok,
          onOpenCollab: openCollab,
          onRequestOpenModal: requestModal,
        } = bootRef.current;
        if (err instanceof ApiError && err.status === 401) {
          setUser(null);
          if (collabId && shareTok && !pendingJoinTokenRef.current) {
            try {
              const summary = await workbook(collabId, undefined, { xReticulaShare: shareTok });
              if (!cancelled) {
                setLoadingAuth(false);
                openCollab(makeShareVisitorTarget(summary, shareTok));
              }
              return;
            } catch (shareErr) {
              if (!cancelled) {
                setLoadingAuth(false);
                setAccessError(
                  shareErr instanceof ApiError
                    ? "This share link is invalid, expired, or no longer public."
                    : "Could not load shared workbook right now.",
                );
                requestModal();
              }
              return;
            }
          }
          setLoadingAuth(false);
          if (pendingJoinTokenRef.current || collabId) {
            requestModal();
          }
          return;
        }
        setLoadingAuth(false);
        // Network error / offline: allow cached offline collaborative open.
        const cache = readCache();
        if (pendingJoinTokenRef.current) {
          setJoinError("Offline: a network connection is required to redeem a share link.");
          requestModal();
        } else if (collabId && cache.user && cache.workbooks[collabId]) {
          setUser(cache.user);
          openCollab(makeTarget(cache.user, cache.workbooks[collabId]));
        } else if (collabId) {
          setAccessError(
            "Offline: this collaborative workbook has not been opened on this device yet.",
          );
          requestModal();
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!open || !user || !activeTarget) {
      return;
    }
    const controller = new AbortController();
    void refreshGovernance(activeTarget.docId, activeTarget.role, controller.signal);
    return () => {
      controller.abort();
    };
  }, [open, user, activeTarget, refreshGovernance]);

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
    setShareUrl(null);
    setCopiedShare(false);
    setAuthError(null);
    try {
      const ttlParsed = shareTtlSeconds ? Number(shareTtlSeconds) : null;
      const created = await share(activeTarget.docId, {
        role: shareRole,
        label: shareLabel.trim() || null,
        singleUse: singleUseShare,
        ttlSeconds: ttlParsed && Number.isFinite(ttlParsed) ? ttlParsed : null,
      });
      await refreshGovernance(activeTarget.docId, activeTarget.role);
      setShareUrl(`${window.location.origin}${created.url}`);
      setShareLabel("");
    } catch (err) {
      setAuthError(apiErrorMessage(err, "Could not create share link."));
    } finally {
      setBusy(false);
    }
  };

  const handleToggleVisibility = async () => {
    if (activeTarget?.role !== "owner") {
      return;
    }
    const currentVisibility = activeTarget.visibility ?? "private";
    const nextVisibility: WorkbookVisibility =
      currentVisibility === "link_view" ? "private" : "link_view";
    setBusy(true);
    setAuthError(null);
    try {
      let autoShareUrl: string | null = null;
      if (nextVisibility === "link_view") {
        const hasActiveViewer = shareLinks.some(
          (link) =>
            link.role === "viewer" &&
            !link.revokedAt &&
            (!link.singleUse || !link.consumed) &&
            (!link.expiresAt || Date.parse(link.expiresAt) > Date.now()),
        );
        if (!hasActiveViewer) {
          const created = await share(activeTarget.docId, {
            role: "viewer",
            label: "Link-view access",
            singleUse: false,
          });
          autoShareUrl = `${window.location.origin}${created.url}`;
        }
      }
      const updated = await updateWorkbook(activeTarget.docId, { visibility: nextVisibility });
      onUpdateVisibility(updated.visibility);
      await refreshGovernance(activeTarget.docId, activeTarget.role);
      if (autoShareUrl) {
        setShareUrl(autoShareUrl);
      }
    } catch (err) {
      setAuthError(apiErrorMessage(err, "Could not update workbook visibility."));
    } finally {
      setBusy(false);
    }
  };

  const handleUpdateMemberRole = async (memberId: string, role: ShareRole) => {
    if (!activeTarget) {
      return;
    }
    setBusy(true);
    setAuthError(null);
    try {
      await updateMember(activeTarget.docId, memberId, { role });
      await refreshGovernance(activeTarget.docId, activeTarget.role);
    } catch (err) {
      setAuthError(apiErrorMessage(err, "Could not update member role."));
    } finally {
      setBusy(false);
    }
  };

  const handleRemoveMember = async (memberId: string, selfLeave: boolean) => {
    if (!activeTarget) {
      return;
    }
    setBusy(true);
    setAuthError(null);
    try {
      await removeMember(activeTarget.docId, memberId);
      if (selfLeave) {
        handleBackToLocal();
      } else {
        await refreshGovernance(activeTarget.docId, activeTarget.role);
      }
    } catch (err) {
      setAuthError(apiErrorMessage(err, "Could not remove member."));
    } finally {
      setBusy(false);
    }
  };

  const handleRevokeShareLink = async (linkId: string) => {
    if (!activeTarget) {
      return;
    }
    setBusy(true);
    setAuthError(null);
    try {
      await revokeShareLink(activeTarget.docId, linkId);
      await refreshGovernance(activeTarget.docId, activeTarget.role);
    } catch (err) {
      setAuthError(apiErrorMessage(err, "Could not revoke share link."));
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
          {activeTarget && readOnly && (
            <span className="collab-badge-readonly" data-testid="collab-readonly-badge">
              Read-only
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

      {historyOpen && activeTarget && (
        <Suspense fallback={null}>
          <LazyHistoryDrawer
            docId={activeTarget.docId}
            shareToken={activeTarget.shareToken}
            readOnly={readOnly}
            open={historyOpen}
            previewSeq={historyPreviewSeq}
            previewElapsedMs={historyPreviewElapsedMs}
            onSeek={onHistorySeek}
            onExit={onHistoryExit}
            onClose={onCloseHistory}
          />
        </Suspense>
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
                    <button
                      type="button"
                      className="toolbar-btn"
                      data-testid="dev-login-carol"
                      disabled={busy}
                      onClick={() => void handleDevSignIn("carol")}
                    >
                      Dev Sign In as Carol
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
                    <div className="collab-row">
                      <button
                        type="button"
                        className="toolbar-btn"
                        data-testid="collab-go-workbooks-btn"
                        onClick={() => {
                          onCloseModal();
                          onNavigate("/workbooks");
                        }}
                      >
                        Workbooks
                      </button>
                      <button
                        type="button"
                        className="toolbar-btn"
                        data-testid="collab-go-account-btn"
                        onClick={() => {
                          onCloseModal();
                          onNavigate("/account");
                        }}
                      >
                        Account
                      </button>
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
                      {activeTarget.role === "owner" && (
                        <label className="collab-checkbox">
                          <input
                            type="checkbox"
                            data-testid="workbook-visibility-toggle"
                            disabled={busy}
                            checked={(activeTarget.visibility ?? "private") === "link_view"}
                            onChange={() => void handleToggleVisibility()}
                          />
                          Allow link-only viewing
                        </label>
                      )}
                    </div>

                    {activeTarget.role === "owner" && (
                      <>
                        <div className="collab-row">
                          <select
                            className="collab-select"
                            data-testid="share-role-select"
                            aria-label="Share link role"
                            value={shareRole}
                            onChange={(e) =>
                              setShareRole(e.target.value === "viewer" ? "viewer" : "editor")
                            }
                          >
                            <option value="editor">Editor invite</option>
                            <option value="viewer">Viewer link</option>
                          </select>
                          <input
                            type="text"
                            className="collab-input"
                            data-testid="share-label-input"
                            placeholder="Label (optional)"
                            maxLength={80}
                            value={shareLabel}
                            onChange={(e) => setShareLabel(e.target.value)}
                          />
                          <select
                            className="collab-select"
                            data-testid="share-ttl-select"
                            aria-label="Share link expiry"
                            value={shareTtlSeconds}
                            onChange={(e) => setShareTtlSeconds(e.target.value)}
                          >
                            <option value="">No expiry</option>
                            <option value="3600">1 hour</option>
                            <option value="86400">24 hours</option>
                            <option value="604800">7 days</option>
                          </select>
                        </div>
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
                            {shareRole === "viewer" ? "Create Viewer Link" : "Share Editor Link"}
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
                        {shareLinks.length > 0 && (
                          <ul className="collab-list" data-testid="share-links-list">
                            {shareLinks.map((link) => {
                              const statusText = link.revokedAt
                                ? "Revoked"
                                : link.singleUse && link.consumed
                                  ? "Used"
                                  : "Active";
                              const active = !link.revokedAt && !(link.singleUse && link.consumed);
                              return (
                                <li key={link.id} className="collab-member-row">
                                  <div className="collab-member-meta">
                                    <strong>
                                      {link.label ||
                                        (link.role === "viewer" ? "Viewer link" : "Editor invite")}
                                    </strong>
                                    <span className="collab-member-sub">
                                      {link.role} · {link.singleUse ? "single-use" : "multi-use"} ·{" "}
                                      {statusText}
                                    </span>
                                  </div>
                                  {active && (
                                    <button
                                      type="button"
                                      className="toolbar-btn"
                                      data-testid={`revoke-share-link-${link.id}`}
                                      disabled={busy}
                                      onClick={() => void handleRevokeShareLink(link.id)}
                                    >
                                      Revoke
                                    </button>
                                  )}
                                </li>
                              );
                            })}
                          </ul>
                        )}
                      </>
                    )}

                    {members.length > 0 && (
                      <>
                        <p className="collab-section-title">Members ({members.length})</p>
                        <ul className="collab-list" data-testid="members-list">
                          {members.map((member) => {
                            const isSelf = member.userId === user.id;
                            const isOwnerRow = member.role === "owner";
                            const canManage =
                              activeTarget.role === "owner" && !isOwnerRow && !isSelf;
                            const canLeave = isSelf && !isOwnerRow;
                            return (
                              <li key={member.userId} className="collab-member-row">
                                <div className="collab-member-meta">
                                  <strong>
                                    {member.name}
                                    {isSelf ? " (you)" : ""}
                                  </strong>
                                  <span className="collab-member-sub">{member.role}</span>
                                </div>
                                <div className="collab-row">
                                  {canManage && (
                                    <>
                                      <select
                                        className="collab-select"
                                        data-testid={`member-role-${member.userId}`}
                                        aria-label={`Role for ${member.name}`}
                                        disabled={busy}
                                        value={member.role === "viewer" ? "viewer" : "editor"}
                                        onChange={(e) =>
                                          void handleUpdateMemberRole(
                                            member.userId,
                                            e.target.value === "viewer" ? "viewer" : "editor",
                                          )
                                        }
                                      >
                                        <option value="editor">editor</option>
                                        <option value="viewer">viewer</option>
                                      </select>
                                      <button
                                        type="button"
                                        className="toolbar-btn"
                                        data-testid={`remove-member-${member.userId}`}
                                        disabled={busy}
                                        onClick={() =>
                                          void handleRemoveMember(member.userId, false)
                                        }
                                      >
                                        Remove
                                      </button>
                                    </>
                                  )}
                                  {canLeave && (
                                    <button
                                      type="button"
                                      className="toolbar-btn"
                                      data-testid="leave-workbook-btn"
                                      disabled={busy}
                                      onClick={() => void handleRemoveMember(member.userId, true)}
                                    >
                                      Leave
                                    </button>
                                  )}
                                </div>
                              </li>
                            );
                          })}
                        </ul>
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
