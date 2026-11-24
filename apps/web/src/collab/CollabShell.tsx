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
import {
  lazy,
  type RefObject,
  Suspense,
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { translateEngineText } from "../i18n/engine-text.ts";
import { t } from "../i18n/index.ts";
import { tx } from "../i18n/react.ts";
import { apiErrorMessage } from "../pages/helpers.ts";
import { roleName } from "../pages/labels.ts";
import { ActionButton } from "../ui/ActionButton.tsx";
import { announce } from "../ui/announcer.ts";
import { Dialog } from "../ui/Dialog.tsx";
import { focusIsLost } from "../ui/focus.ts";
import { LINK_VIEW_LABEL, personText, shareLinkText } from "./labels.ts";
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
  /** What had the focus when the dialog and the history were opened. */
  readonly modalOpener: RefObject<Element | null>;
  readonly historyOpener: RefObject<Element | null>;
  readonly onFocusGrid: () => void;
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
  if (connection === "offline") {
    return pendingCount > 0 ? t("Offline · {n} unsent", { n: pendingCount }) : t("Offline");
  }
  if (pendingCount > 0) {
    return t("Syncing ({n})", { n: pendingCount });
  }
  return connection === "online" ? t("Synced") : t("Connecting…");
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
    modalOpener,
    historyOpener,
    onFocusGrid,
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
  const headingRef = useRef<HTMLHeadingElement>(null);
  const firstFieldRef = useRef<HTMLElement>(null);
  const takeFirstField = useCallback((element: HTMLElement | null) => {
    firstFieldRef.current = element;
  }, []);

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
            setJoinError(t("Share link rejected (HTTP {status}).", { status: err.status }));
          } else {
            setJoinError(t("Could not redeem share link right now."));
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
            setAccessError(t("You do not have permission to open this collaborative workbook."));
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
              setAccessError(t("Could not load collaborative workbook metadata."));
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
                    ? t("This share link is invalid, expired, or no longer public.")
                    : t("Could not load shared workbook right now."),
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
          setJoinError(t("Offline: a network connection is required to redeem a share link."));
          requestModal();
        } else if (collabId && cache.user && cache.workbooks[collabId]) {
          setUser(cache.user);
          openCollab(makeTarget(cache.user, cache.workbooks[collabId]));
        } else if (collabId) {
          setAccessError(
            t("Offline: this collaborative workbook has not been opened on this device yet."),
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
        setAuthError(t("Loopback dev sign-in is disabled on this server. Use GitHub sign-in."));
      } else if (err instanceof ApiError) {
        setAuthError(t("Sign-in failed (HTTP {status}).", { status: err.status }));
      } else {
        setAuthError(t("Sign-in failed."));
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
          ? t("Failed to create cloud workbook (HTTP {status}).", { status: err.status })
          : t("Failed to create cloud workbook."),
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
      announce(t("Share link created."));
      setShareLabel("");
    } catch (err) {
      setAuthError(apiErrorMessage(err, t("Could not create share link.")));
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
            label: LINK_VIEW_LABEL,
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
        announce(t("Share link created."));
      }
    } catch (err) {
      setAuthError(apiErrorMessage(err, t("Could not update workbook visibility.")));
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
      setAuthError(apiErrorMessage(err, t("Could not update member role.")));
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
      setAuthError(apiErrorMessage(err, t("Could not remove member.")));
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
      setAuthError(apiErrorMessage(err, t("Could not revoke share link.")));
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
      announce(t("Copied"));
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

  const handleCloseModal = () => {
    setJoinError(null);
    setAccessError(null);
    onCloseModal();
  };

  const noticeText = notice ? translateEngineText(notice) : "";
  useEffect(() => announce(noticeText), [noticeText]);

  // What there is to do is only known once the sign-in check is done, and is
  // something else after signing in or out.
  const modalStage = !showModal ? null : loadingAuth ? "checking" : user ? "signedIn" : "signedOut";
  useLayoutEffect(() => {
    if (
      modalStage !== null &&
      (focusIsLost() || document.activeElement === headingRef.current) &&
      firstFieldRef.current !== null
    ) {
      firstFieldRef.current.focus();
    }
  }, [modalStage]);

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
              {t("Read-only")}
            </span>
          )}
          {remotePresences.length > 0 && (
            <div className="collab-peers" data-testid="collab-peers">
              <span className="visually-hidden">{t("Also here:")}</span>
              {remotePresences.map((peer) => (
                <span
                  key={peer.replica}
                  className="collab-peer-chip"
                  data-testid={`collab-peer-${peer.replica}`}
                >
                  <span
                    className="collab-peer-dot"
                    aria-hidden="true"
                    style={{ background: PEER_COLORS[peer.colorSlot % PEER_COLORS.length] }}
                  />
                  {personText(peer.displayName)}
                </span>
              ))}
            </div>
          )}
          {notice && (
            <span className="collab-notice" data-testid="collab-notice">
              {noticeText}
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
            opener={historyOpener}
            onFocusGrid={onFocusGrid}
            onSeek={onHistorySeek}
            onExit={onHistoryExit}
            onClose={onCloseHistory}
          />
        </Suspense>
      )}

      {unsyncedRescue && (
        <div className="collab-unsynced-banner" data-testid="unsynced-banner" role="alert">
          <span>
            {t("Server rejected incompatible pending edits; a local backup “{title}” was saved.", {
              title: unsyncedRescue.title,
            })}
          </span>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="open-unsynced-btn"
            onClick={() => onOpenLocalRescue(unsyncedRescue.localDocId, unsyncedRescue.title)}
          >
            {t("Open Local Copy")}
          </button>
        </div>
      )}

      {showModal && (
        <Dialog
          labelledBy="collab-heading"
          initialFocus={headingRef}
          opener={modalOpener}
          fallbackFocus={onFocusGrid}
          onClose={handleCloseModal}
          className="collab-modal"
          testId="collab-modal"
        >
          <div className="dialog-header">
            <h2 ref={headingRef} id="collab-heading" tabIndex={-1}>
              {t("Cloud Collaboration")}
            </h2>
            <button
              type="button"
              className="tip-close"
              aria-label={t("Close")}
              onClick={handleCloseModal}
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
            <p className="collab-notice" role="status">
              {t("Checking sign-in status…")}
            </p>
          ) : !user ? (
            <div className="collab-section" data-testid="collab-auth-panel">
              <h3 className="collab-section-title">{t("Sign In Required")}</h3>
              <p className="collab-notice">
                {pendingJoinTokenRef.current
                  ? t("Sign in to redeem this editor invite link.")
                  : t("Sign in to create or open collaborative cloud workbooks.")}
              </p>
              {devLoginAvailable && (
                <div className="collab-row">
                  <ActionButton
                    ref={takeFirstField}
                    className="toolbar-btn primary-btn"
                    data-testid="dev-login-alice"
                    inactive={busy}
                    onClick={() => void handleDevSignIn("alice")}
                  >
                    {t("Dev Sign In as {name}", { name: "Alice" })}
                  </ActionButton>
                  <ActionButton
                    className="toolbar-btn"
                    data-testid="dev-login-bob"
                    inactive={busy}
                    onClick={() => void handleDevSignIn("bob")}
                  >
                    {t("Dev Sign In as {name}", { name: "Bob" })}
                  </ActionButton>
                  <ActionButton
                    className="toolbar-btn"
                    data-testid="dev-login-carol"
                    inactive={busy}
                    onClick={() => void handleDevSignIn("carol")}
                  >
                    {t("Dev Sign In as {name}", { name: "Carol" })}
                  </ActionButton>
                </div>
              )}
              <div className="collab-row">
                <a
                  ref={devLoginAvailable ? undefined : takeFirstField}
                  className="toolbar-btn"
                  data-testid="github-login-link"
                  href={githubLoginUrl(window.location.pathname + window.location.search)}
                >
                  {t("Sign in with GitHub")}
                </a>
              </div>
            </div>
          ) : (
            <>
              <div className="collab-section">
                <div className="collab-row" style={{ justifyContent: "space-between" }}>
                  <span data-testid="collab-user-badge">
                    {tx("Signed in as <0>{name}</0>", { 0: <strong /> }, { name: user.name })}
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
                      {t("Workbooks")}
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
                      {t("Account")}
                    </button>
                    <ActionButton
                      className="toolbar-btn"
                      data-testid="collab-logout-btn"
                      inactive={busy}
                      onClick={() => void handleLogout()}
                    >
                      {t("Sign Out")}
                    </ActionButton>
                  </div>
                </div>
              </div>

              <div className="collab-section">
                <h3 className="collab-section-title">{t("New Cloud Workbook")}</h3>
                <div className="collab-row">
                  <input
                    ref={takeFirstField}
                    type="text"
                    className="collab-input"
                    aria-label={t("Workbook title")}
                    data-testid="new-cloud-title-input"
                    value={newTitle}
                    onChange={(e) => setNewTitle(e.target.value)}
                    placeholder={t("Workbook title")}
                  />
                  <ActionButton
                    className="toolbar-btn primary-btn"
                    data-testid="new-cloud-sheet-btn"
                    inactive={busy}
                    onClick={() => void handleCreateCloudSheet()}
                  >
                    {t("+ New Cloud Sheet")}
                  </ActionButton>
                </div>
              </div>

              {activeTarget && (
                <div className="collab-section">
                  <h3 className="collab-section-title">{t("Current Cloud Sheet")}</h3>
                  <div className="collab-row" style={{ justifyContent: "space-between" }}>
                    <span>
                      <strong>{activeTarget.title}</strong> ({roleName(activeTarget.role)})
                    </span>
                    {activeTarget.role === "owner" && (
                      <label className="collab-checkbox">
                        <input
                          type="checkbox"
                          data-testid="workbook-visibility-toggle"
                          aria-disabled={busy || undefined}
                          checked={(activeTarget.visibility ?? "private") === "link_view"}
                          onChange={() => {
                            if (!busy) {
                              void handleToggleVisibility();
                            }
                          }}
                        />
                        {t("Allow link-only viewing")}
                      </label>
                    )}
                  </div>

                  {activeTarget.role === "owner" && (
                    <>
                      <div className="collab-row">
                        <select
                          className="collab-select"
                          data-testid="share-role-select"
                          aria-label={t("Share link role")}
                          value={shareRole}
                          onChange={(e) =>
                            setShareRole(e.target.value === "viewer" ? "viewer" : "editor")
                          }
                        >
                          <option value="editor">{t("Editor invite")}</option>
                          <option value="viewer">{t("Viewer link")}</option>
                        </select>
                        <input
                          type="text"
                          className="collab-input"
                          aria-label={t("Label (optional)")}
                          data-testid="share-label-input"
                          placeholder={t("Label (optional)")}
                          maxLength={80}
                          value={shareLabel}
                          onChange={(e) => setShareLabel(e.target.value)}
                        />
                        <select
                          className="collab-select"
                          data-testid="share-ttl-select"
                          aria-label={t("Share link expiry")}
                          value={shareTtlSeconds}
                          onChange={(e) => setShareTtlSeconds(e.target.value)}
                        >
                          <option value="">{t("No expiry")}</option>
                          <option value="3600">{t("1 hour")}</option>
                          <option value="86400">{t("24 hours")}</option>
                          <option value="604800">{t("7 days")}</option>
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
                          {t("Single-use link")}
                        </label>
                        <ActionButton
                          className="toolbar-btn"
                          data-testid="create-share-link-btn"
                          inactive={busy}
                          onClick={() => void handleCreateShareLink()}
                        >
                          {shareRole === "viewer"
                            ? t("Create Viewer Link")
                            : t("Share Editor Link")}
                        </ActionButton>
                      </div>
                      {shareUrl && (
                        <div className="collab-link-box">
                          <input
                            type="text"
                            readOnly
                            className="collab-input"
                            aria-label={t("Share link")}
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
                            {copiedShare ? t("Copied") : t("Copy")}
                          </button>
                        </div>
                      )}
                      {shareLinks.length > 0 && (
                        <ul className="collab-list" data-testid="share-links-list">
                          {shareLinks.map((link) => {
                            const statusText = link.revokedAt
                              ? t("Revoked|link")
                              : link.singleUse && link.consumed
                                ? t("Used|link")
                                : t("Active|link");
                            const active = !link.revokedAt && !(link.singleUse && link.consumed);
                            return (
                              <li key={link.id} className="collab-member-row">
                                <div className="collab-member-meta">
                                  <strong>{shareLinkText(link)}</strong>
                                  <span className="collab-member-sub">
                                    {roleName(link.role)} ·{" "}
                                    {link.singleUse ? t("single-use") : t("multi-use")} ·{" "}
                                    {statusText}
                                  </span>
                                </div>
                                {active && (
                                  <ActionButton
                                    className="toolbar-btn"
                                    aria-label={t("Revoke {label}", {
                                      label: shareLinkText(link),
                                    })}
                                    data-testid={`revoke-share-link-${link.id}`}
                                    inactive={busy}
                                    onClick={() => void handleRevokeShareLink(link.id)}
                                  >
                                    {t("Revoke")}
                                  </ActionButton>
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
                      <h3 className="collab-section-title">
                        {t("Members ({n})", { n: members.length })}
                      </h3>
                      <ul className="collab-list" data-testid="members-list">
                        {members.map((member) => {
                          const isSelf = member.userId === user.id;
                          const isOwnerRow = member.role === "owner";
                          const canManage = activeTarget.role === "owner" && !isOwnerRow && !isSelf;
                          const canLeave = isSelf && !isOwnerRow;
                          return (
                            <li key={member.userId} className="collab-member-row">
                              <div className="collab-member-meta">
                                <strong>
                                  {isSelf ? t("{name} (you)", { name: member.name }) : member.name}
                                </strong>
                                <span className="collab-member-sub">{roleName(member.role)}</span>
                              </div>
                              <div className="collab-row">
                                {canManage && (
                                  <>
                                    <select
                                      className="collab-select"
                                      data-testid={`member-role-${member.userId}`}
                                      aria-label={t("Role for {name}", { name: member.name })}
                                      aria-disabled={busy || undefined}
                                      value={member.role === "viewer" ? "viewer" : "editor"}
                                      onChange={(e) => {
                                        if (!busy) {
                                          void handleUpdateMemberRole(
                                            member.userId,
                                            e.target.value === "viewer" ? "viewer" : "editor",
                                          );
                                        }
                                      }}
                                    >
                                      <option value="editor">{t("editor|role")}</option>
                                      <option value="viewer">{t("viewer|role")}</option>
                                    </select>
                                    <ActionButton
                                      className="toolbar-btn"
                                      aria-label={t("Remove {name}", { name: member.name })}
                                      data-testid={`remove-member-${member.userId}`}
                                      inactive={busy}
                                      onClick={() => void handleRemoveMember(member.userId, false)}
                                    >
                                      {t("Remove")}
                                    </ActionButton>
                                  </>
                                )}
                                {canLeave && (
                                  <ActionButton
                                    className="toolbar-btn"
                                    data-testid="leave-workbook-btn"
                                    inactive={busy}
                                    onClick={() => void handleRemoveMember(member.userId, true)}
                                  >
                                    {t("Leave")}
                                  </ActionButton>
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

          <div className="dialog-footer">
            {(activeTarget || joinError || accessError) && (
              <button
                type="button"
                className="toolbar-btn"
                data-testid="back-to-local-btn"
                onClick={handleBackToLocal}
              >
                {t("Back to Local Sheets")}
              </button>
            )}
            <button
              type="button"
              className="toolbar-btn"
              data-testid="collab-modal-close"
              onClick={handleCloseModal}
            >
              {t("Close")}
            </button>
          </div>
        </Dialog>
      )}
    </>
  );
}
