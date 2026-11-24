import {
  ApiError,
  createWorkbook,
  type DevAccount,
  deleteWorkbook,
  devLogin,
  githubLoginUrl,
  importWorkbook,
  listWorkbooks,
  me,
  type User,
  updateWorkbook,
  type WorkbookListItem,
  type WorkbookLists,
} from "@reticula/protocol/collab-api";
import { type ChangeEvent, useCallback, useEffect, useRef, useState } from "react";
import { uploadLocalDocToCloud } from "../engine/local-upload.ts";
import {
  readCatalog,
  removeDocStorage,
  type StorageBackendKind,
  type WorkbookCatalogEntry,
} from "../engine/storage.ts";
import { apiErrorMessage, formatBytes } from "./helpers.ts";
import "./pages.css";

export interface WorkbooksPageProps {
  readonly onNavigate: (path: string) => void;
}

type TabKind = "owned" | "shared" | "local" | "samples";

function localBackend(): StorageBackendKind {
  return typeof navigator !== "undefined" && typeof navigator.storage?.getDirectory === "function"
    ? "opfs-writable"
    : "memory";
}

export default function WorkbooksPage({ onNavigate }: WorkbooksPageProps) {
  const [tab, setTab] = useState<TabKind>("owned");
  const [user, setUser] = useState<User | null>(null);
  const [cloudLists, setCloudLists] = useState<WorkbookLists | null>(null);
  const [localDocs, setLocalDocs] = useState<readonly WorkbookCatalogEntry[]>([]);
  const [newTitle, setNewTitle] = useState("Untitled Cloud Workbook");
  const [renameId, setRenameId] = useState<string | null>(null);
  const [renameTitle, setRenameTitle] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [statusMsg, setStatusMsg] = useState<string | null>(null);
  const loadSeqRef = useRef(0);

  const loadData = useCallback(async () => {
    const seq = ++loadSeqRef.current;
    setError(null);
    const localPromise = readCatalog(localBackend()).catch(() => [] as WorkbookCatalogEntry[]);
    try {
      const [current, lists, local] = await Promise.all([me(), listWorkbooks(), localPromise]);
      if (seq !== loadSeqRef.current) {
        return;
      }
      setUser(current);
      setCloudLists(lists);
      setLocalDocs(local);
    } catch (err) {
      const local = await localPromise;
      if (seq !== loadSeqRef.current) {
        return;
      }
      setLocalDocs(local);
      if (err instanceof ApiError && err.status === 401) {
        setUser(null);
        setCloudLists(null);
      } else {
        setError(apiErrorMessage(err, "Failed to load workbooks"));
      }
    }
  }, []);

  useEffect(() => {
    void loadData();
  }, [loadData]);

  const handleDevSignIn = async (account: DevAccount) => {
    loadSeqRef.current += 1;
    setBusy(true);
    setError(null);
    try {
      await devLogin({ account });
      await loadData();
    } catch (err) {
      setError(apiErrorMessage(err, "Sign-in failed"));
    } finally {
      setBusy(false);
    }
  };

  const handleCreateCloud = async () => {
    setBusy(true);
    setError(null);
    try {
      const key = await createWorkbook({ title: newTitle.trim() || "Cloud Workbook" });
      onNavigate(`/app?collab=${encodeURIComponent(key.id)}`);
    } catch (err) {
      setError(apiErrorMessage(err, "Could not create cloud workbook"));
    } finally {
      setBusy(false);
    }
  };

  const handleRenameSubmit = async (id: string) => {
    const trimmed = renameTitle.trim();
    if (!trimmed) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await updateWorkbook(id, { title: trimmed });
      setRenameId(null);
      await loadData();
    } catch (err) {
      setError(apiErrorMessage(err, "Could not rename workbook"));
    } finally {
      setBusy(false);
    }
  };

  const handleDeleteCloud = async (item: WorkbookListItem) => {
    setBusy(true);
    setError(null);
    try {
      await deleteWorkbook(item.id);
      setStatusMsg(`Deleted "${item.title}".`);
      await loadData();
    } catch (err) {
      setError(apiErrorMessage(err, "Could not delete workbook"));
    } finally {
      setBusy(false);
    }
  };

  const handleImportFile = async (e: ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    e.target.value = "";
    if (!file) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const buf = await file.arrayBuffer();
      const baseTitle = file.name.replace(/\.(rtcl|rtclsnap)$/i, "").slice(0, 120) || "Imported";
      const key = await importWorkbook(encodeURIComponent(baseTitle), new Uint8Array(buf));
      onNavigate(`/app?collab=${encodeURIComponent(key.id)}`);
    } catch (err) {
      setError(apiErrorMessage(err, "Could not import snapshot"));
    } finally {
      setBusy(false);
    }
  };

  const handleConvertLocal = async (entry: WorkbookCatalogEntry) => {
    setBusy(true);
    setError(null);
    setStatusMsg(`Packing "${entry.title}" and uploading to cloud…`);
    try {
      const key = await uploadLocalDocToCloud(entry.id, entry.title);
      onNavigate(`/app?collab=${encodeURIComponent(key.id)}`);
    } catch (err) {
      setStatusMsg(null);
      setError(apiErrorMessage(err, "Could not convert local workbook to cloud"));
    } finally {
      setBusy(false);
    }
  };

  const handleDeleteLocal = async (entry: WorkbookCatalogEntry) => {
    setBusy(true);
    try {
      const next = await removeDocStorage(entry.id, localBackend());
      setLocalDocs(next);
    } catch (err) {
      setError(apiErrorMessage(err, "Could not remove local workbook"));
    } finally {
      setBusy(false);
    }
  };

  const renderCloudRow = (item: WorkbookListItem, owned: boolean) => (
    <div key={item.id} className="page-item" data-testid={`cloud-wb-${item.id}`}>
      <div className="page-item-meta">
        {renameId === item.id ? (
          <div className="collab-row">
            <input
              type="text"
              className="collab-input"
              value={renameTitle}
              onChange={(e) => setRenameTitle(e.target.value)}
            />
            <button
              type="button"
              className="toolbar-btn primary-btn"
              disabled={busy}
              onClick={() => void handleRenameSubmit(item.id)}
            >
              Save
            </button>
            <button type="button" className="toolbar-btn" onClick={() => setRenameId(null)}>
              Cancel
            </button>
          </div>
        ) : (
          <strong>{item.title}</strong>
        )}
        <span className="page-muted">
          role: {item.role} · visibility: {item.visibility} · head #{item.headSeq} ·{" "}
          {formatBytes(item.estBytes)} · updated {item.updatedAt}
        </span>
      </div>
      <div className="collab-row">
        <button
          type="button"
          className="toolbar-btn primary-btn"
          onClick={() => onNavigate(`/app?collab=${encodeURIComponent(item.id)}`)}
        >
          Open
        </button>
        {owned && (
          <>
            <button
              type="button"
              className="toolbar-btn"
              disabled={busy}
              onClick={() => {
                setRenameId(item.id);
                setRenameTitle(item.title);
              }}
            >
              Rename
            </button>
            <button
              type="button"
              className="toolbar-btn"
              disabled={busy}
              onClick={() => void handleDeleteCloud(item)}
            >
              Delete
            </button>
          </>
        )}
      </div>
    </div>
  );

  return (
    <div className="page-shell" data-testid="workbooks-page">
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
            href="/sim"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/sim");
            }}
          >
            Sync Simulator
          </a>
          <a
            href="/account"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/account");
            }}
          >
            Account
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
          <h1>Workbooks</h1>
          <div className="page-tabs">
            {(
              [
                ["owned", "My Cloud Workbooks"],
                ["shared", "Shared with Me"],
                ["local", "Local Browser Docs"],
                ["samples", "Built-in Examples"],
              ] as const
            ).map(([key, label]) => (
              <button
                key={key}
                type="button"
                className={tab === key ? "page-tab active" : "page-tab"}
                data-testid={`workbooks-tab-${key}`}
                onClick={() => setTab(key)}
              >
                {label}
              </button>
            ))}
          </div>

          {error && (
            <div className="collab-error" data-testid="workbooks-error" role="alert">
              {error}
            </div>
          )}
          {statusMsg && <p className="page-muted">{statusMsg}</p>}

          {(tab === "owned" || tab === "shared") && !user && (
            <div className="collab-section" data-testid="workbooks-signin">
              <p className="page-muted">Sign in to manage cloud collaborative workbooks.</p>
              <div className="collab-row">
                {(["alice", "bob", "carol"] as const).map((acct) => (
                  <button
                    key={acct}
                    type="button"
                    className="toolbar-btn"
                    data-testid={`workbooks-dev-${acct}`}
                    disabled={busy}
                    onClick={() => void handleDevSignIn(acct)}
                  >
                    Dev: {acct}
                  </button>
                ))}
                <a className="toolbar-btn primary-btn" href={githubLoginUrl("/workbooks")}>
                  Sign in with GitHub
                </a>
              </div>
            </div>
          )}

          {tab === "owned" && user && (
            <>
              <div className="collab-row">
                <input
                  type="text"
                  className="collab-input"
                  value={newTitle}
                  onChange={(e) => setNewTitle(e.target.value)}
                  placeholder="New cloud workbook title"
                />
                <button
                  type="button"
                  className="toolbar-btn primary-btn"
                  data-testid="workbooks-create-btn"
                  disabled={busy}
                  onClick={() => void handleCreateCloud()}
                >
                  + New Cloud Workbook
                </button>
                <label className="toolbar-btn" style={{ cursor: "pointer" }}>
                  Import .rtcl
                  <input
                    type="file"
                    accept=".rtcl,.rtclsnap,application/octet-stream"
                    style={{ display: "none" }}
                    onChange={(e) => void handleImportFile(e)}
                  />
                </label>
              </div>
              <div className="page-list">
                {(cloudLists?.owned ?? []).length === 0 ? (
                  <p className="page-muted">No cloud workbooks yet.</p>
                ) : (
                  (cloudLists?.owned ?? []).map((item) => renderCloudRow(item, true))
                )}
              </div>
            </>
          )}

          {tab === "shared" && user && (
            <div className="page-list">
              {(cloudLists?.shared ?? []).length === 0 ? (
                <p className="page-muted">No shared workbooks yet.</p>
              ) : (
                (cloudLists?.shared ?? []).map((item) => renderCloudRow(item, false))
              )}
            </div>
          )}

          {tab === "local" && (
            <div className="page-list">
              {localDocs.length === 0 ? (
                <p className="page-muted">No local OPFS workbooks found in this browser.</p>
              ) : (
                localDocs.map((entry) => (
                  <div key={entry.id} className="page-item" data-testid={`local-wb-${entry.id}`}>
                    <div className="page-item-meta">
                      <strong>{entry.title}</strong>
                      <span className="page-muted">
                        id: {entry.id} · {formatBytes(entry.sizeBytes)}
                      </span>
                    </div>
                    <div className="collab-row">
                      <button
                        type="button"
                        className="toolbar-btn primary-btn"
                        onClick={() => onNavigate(`/app?doc=${encodeURIComponent(entry.id)}`)}
                      >
                        Open Local
                      </button>
                      {user && (
                        <button
                          type="button"
                          className="toolbar-btn"
                          data-testid={`convert-cloud-${entry.id}`}
                          disabled={busy}
                          onClick={() => void handleConvertLocal(entry)}
                        >
                          Convert to Cloud
                        </button>
                      )}
                      {entry.id !== "sample-1m" && (
                        <button
                          type="button"
                          className="toolbar-btn"
                          disabled={busy}
                          onClick={() => void handleDeleteLocal(entry)}
                        >
                          Delete Local
                        </button>
                      )}
                    </div>
                  </div>
                ))
              )}
            </div>
          )}

          {tab === "samples" && (
            <div className="page-list">
              <div className="page-item">
                <div className="page-item-meta">
                  <strong>1,000,000-Row Budget Sample</strong>
                  <span className="page-muted">
                    22 columns · 100,000 bound formulas · runs locally in Web Worker + OPFS
                  </span>
                </div>
                <button
                  type="button"
                  className="toolbar-btn primary-btn"
                  onClick={() => onNavigate("/app")}
                >
                  Open Sample
                </button>
              </div>
              <div className="page-item">
                <div className="page-item-meta">
                  <strong>Deterministic Sync Simulation Lab</strong>
                  <span className="page-muted">
                    Interactive 2–3 replica offline merge &amp; 5-replica fault injector without
                    sign-in
                  </span>
                </div>
                <button type="button" className="toolbar-btn" onClick={() => onNavigate("/sim")}>
                  Open /sim
                </button>
              </div>
            </div>
          )}
        </div>
      </main>
    </div>
  );
}
