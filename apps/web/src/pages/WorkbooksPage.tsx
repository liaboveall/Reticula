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
import { flushSync } from "react-dom";
import { uploadLocalDocToCloud } from "../engine/local-upload.ts";
import {
  readCatalog,
  removeDocStorage,
  type StorageBackendKind,
  type WorkbookCatalogEntry,
} from "../engine/storage.ts";
import { fmtBytes } from "../i18n/formats.ts";
import { type Message, t, tn } from "../i18n/index.ts";
import { keepsUploads, type TableSize, tableSizes, uploadsReason } from "../local-tables.ts";
import { deleteTables } from "../tables/delete.ts";
import { ActionButton } from "../ui/ActionButton.tsx";
import { announce } from "../ui/announcer.ts";
import { isComposing, useKeptFocus } from "../ui/focus.ts";
import { Tabs } from "../ui/Tabs.tsx";
import { apiErrorMessage } from "./helpers.ts";
import { roleName, visibilityName } from "./labels.ts";
import { PageNav } from "./PageNav.tsx";
import "../collab/collab.css";
import "./pages.css";

export interface WorkbooksPageProps {
  readonly onNavigate: (path: string) => void;
}

/** The workbook with the built-in data table, which is made if it is not there yet. */
const SAMPLE_TABLE_PATH = "/app?doc=orders-10m&sample=10000000";

type TabKind = "owned" | "shared" | "local" | "samples";

function failureOf(err: unknown, fallback: () => string): Message {
  return { say: () => apiErrorMessage(err, fallback()) };
}

function localBackend(): StorageBackendKind {
  return typeof navigator.storage?.getDirectory === "function" ? "opfs-writable" : "memory";
}

/** Deletes a workbook with its data tables, and lists the others. `null` if it is open somewhere. */
async function deleteLocal(id: string): Promise<readonly WorkbookCatalogEntry[] | null> {
  const remove = async () =>
    (await deleteTables(id)) ? removeDocStorage(id, localBackend()) : null;
  return navigator.locks?.request
    ? navigator.locks.request(`reticula:local:${id}`, { ifAvailable: true }, (lock) =>
        lock ? remove() : null,
      )
    : remove();
}

export default function WorkbooksPage({ onNavigate }: WorkbooksPageProps) {
  const [tab, setTab] = useState<TabKind>("owned");
  const [user, setUser] = useState<User | null>(null);
  const [cloudLists, setCloudLists] = useState<WorkbookLists | null>(null);
  const [localDocs, setLocalDocs] = useState<readonly WorkbookCatalogEntry[]>([]);
  const [tables, setTables] = useState<ReadonlyMap<string, TableSize>>(new Map());
  const [newTitle, setNewTitle] = useState("Untitled Cloud Workbook");
  const [renameId, setRenameId] = useState<string | null>(null);
  const [renameTitle, setRenameTitle] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Message | null>(null);
  const [statusMsg, setStatusMsg] = useState<Message | null>(null);
  const loadSeqRef = useRef(0);
  const mainRef = useKeptFocus();
  const importRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (statusMsg !== null) {
      announce(statusMsg.say());
    }
  }, [statusMsg]);

  useEffect(() => {
    let isOver = false;
    void tableSizes(localDocs).then((sizes) => isOver || setTables(sizes));
    return () => {
      isOver = true;
    };
  }, [localDocs]);

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
        setError(failureOf(err, () => t("Failed to load workbooks")));
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
      setError(failureOf(err, () => t("Sign-in failed")));
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
      setError(failureOf(err, () => t("Could not create cloud workbook")));
    } finally {
      setBusy(false);
    }
  };

  /** The row's button for renaming has the focus again. */
  const endRename = (id: string) => {
    flushSync(() => setRenameId(null));
    mainRef.current?.querySelector<HTMLElement>(`[data-rename="${id}"]`)?.focus();
  };

  const handleRenameSubmit = async (id: string) => {
    const trimmed = renameTitle.trim();
    if (!trimmed || busy) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await updateWorkbook(id, { title: trimmed });
      endRename(id);
      await loadData();
    } catch (err) {
      setError(failureOf(err, () => t("Could not rename workbook")));
    } finally {
      setBusy(false);
    }
  };

  const handleDeleteCloud = async (item: WorkbookListItem) => {
    setBusy(true);
    setError(null);
    try {
      await deleteWorkbook(item.id);
      setStatusMsg({ say: () => t('Deleted "{title}".', { title: item.title }) });
      await loadData();
    } catch (err) {
      setError(failureOf(err, () => t("Could not delete workbook")));
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
      setError(failureOf(err, () => t("Could not import snapshot")));
    } finally {
      setBusy(false);
    }
  };

  const handleConvertLocal = async (entry: WorkbookCatalogEntry) => {
    setBusy(true);
    setError(null);
    setStatusMsg({
      say: () => t('Packing "{title}" and uploading to cloud…', { title: entry.title }),
    });
    try {
      const key = await uploadLocalDocToCloud(entry.id, entry.title);
      onNavigate(`/app?collab=${encodeURIComponent(key.id)}`);
    } catch (err) {
      setStatusMsg(null);
      setError(failureOf(err, () => t("Could not convert local workbook to cloud")));
    } finally {
      setBusy(false);
    }
  };

  const handleDeleteLocal = async (entry: WorkbookCatalogEntry) => {
    setBusy(true);
    try {
      if (await keepsUploads(entry.id)) {
        setError({ say: uploadsReason });
        return;
      }
      const next = await deleteLocal(entry.id);
      if (next) {
        setLocalDocs(next);
      } else {
        setError({
          say: () => t("The workbook is open in another tab. Close it there to delete it."),
        });
      }
    } catch (err) {
      setError(failureOf(err, () => t("Could not remove local workbook")));
    } finally {
      setBusy(false);
    }
  };

  const tablesText = (id: string) => {
    const { count, bytes } = tables.get(id) ?? { count: 0, bytes: 0 };
    return bytes === 0
      ? null
      : tn(
          count,
          "{n} data table · {size} · stays in this browser",
          "{n} data tables · {size} · stay in this browser",
          { size: fmtBytes(bytes) },
        );
  };

  const renderCloudRow = (item: WorkbookListItem, owned: boolean) => (
    <div key={item.id} className="page-item" data-testid={`cloud-wb-${item.id}`}>
      <div className="page-item-meta">
        {renameId === item.id ? (
          <div className="collab-row">
            <input
              type="text"
              className="collab-input"
              // biome-ignore lint/a11y/noAutofocus: the button that was pressed asks for a new name
              autoFocus
              aria-label={t("New name of {title}", { title: item.title })}
              value={renameTitle}
              onChange={(e) => setRenameTitle(e.target.value)}
              onKeyDown={(e) => {
                if (isComposing(e.nativeEvent)) {
                  return;
                }
                if (e.key === "Enter") {
                  void handleRenameSubmit(item.id);
                } else if (e.key === "Escape") {
                  endRename(item.id);
                }
              }}
            />
            <ActionButton
              className="toolbar-btn primary-btn"
              inactive={busy}
              onClick={() => void handleRenameSubmit(item.id)}
            >
              {t("Save")}
            </ActionButton>
            <button type="button" className="toolbar-btn" onClick={() => endRename(item.id)}>
              {t("Cancel")}
            </button>
          </div>
        ) : (
          <strong>{item.title}</strong>
        )}
        <span className="page-muted">
          {t("role: {role} · visibility: {visibility} · head #{seq} · {size} · updated {time}", {
            role: roleName(item.role),
            visibility: visibilityName(item.visibility),
            seq: item.headSeq,
            size: fmtBytes(item.estBytes),
            time: item.updatedAt,
          })}
        </span>
      </div>
      <div className="collab-row">
        <button
          type="button"
          className="toolbar-btn primary-btn"
          aria-label={t("Open {title}", { title: item.title })}
          onClick={() => onNavigate(`/app?collab=${encodeURIComponent(item.id)}`)}
        >
          {t("Open|verb")}
        </button>
        {owned && (
          <>
            <ActionButton
              className="toolbar-btn"
              aria-label={t("Rename {title}", { title: item.title })}
              data-rename={item.id}
              inactive={busy}
              onClick={() => {
                setRenameId(item.id);
                setRenameTitle(item.title);
              }}
            >
              {t("Rename")}
            </ActionButton>
            <ActionButton
              className="toolbar-btn"
              aria-label={t("Delete {title}", { title: item.title })}
              inactive={busy}
              onClick={() => void handleDeleteCloud(item)}
            >
              {t("Delete")}
            </ActionButton>
          </>
        )}
      </div>
    </div>
  );

  return (
    <div className="page-shell" data-testid="workbooks-page">
      <PageNav
        onNavigate={onNavigate}
        links={[
          ["/app", t("Editor")],
          ["/sim", t("Sync Simulator")],
          ["/account", t("Account")],
          ["/privacy", t("Privacy")],
        ]}
      />

      <main ref={mainRef} className="page-main">
        <div className="page-card">
          <h1>{t("Workbooks")}</h1>
          <Tabs
            label={t("Workbooks")}
            tabs={[
              {
                key: "owned",
                text: t("My Cloud Workbooks"),
                testId: "workbooks-tab-owned",
              },
              { key: "shared", text: t("Shared with Me"), testId: "workbooks-tab-shared" },
              { key: "local", text: t("Local Browser Docs"), testId: "workbooks-tab-local" },
              { key: "samples", text: t("Built-in Examples"), testId: "workbooks-tab-samples" },
            ]}
            selected={tab}
            onSelect={setTab}
            listClassName="page-tabs"
            tabClassName="page-tab"
            panelClassName="page-panel"
          >
            {error && (
              <div className="collab-error" data-testid="workbooks-error" role="alert">
                {error.say()}
              </div>
            )}
            {statusMsg && <p className="page-muted">{statusMsg.say()}</p>}

            {(tab === "owned" || tab === "shared") && !user && (
              <div className="collab-section" data-testid="workbooks-signin">
                <p className="page-muted">
                  {t("Sign in to manage cloud collaborative workbooks.")}
                </p>
                <div className="collab-row">
                  {(["alice", "bob", "carol"] as const).map((acct) => (
                    <ActionButton
                      key={acct}
                      className="toolbar-btn"
                      data-testid={`workbooks-dev-${acct}`}
                      inactive={busy}
                      onClick={() => void handleDevSignIn(acct)}
                    >
                      {t("Dev: {account}", { account: acct })}
                    </ActionButton>
                  ))}
                  <a className="toolbar-btn primary-btn" href={githubLoginUrl("/workbooks")}>
                    {t("Sign in with GitHub")}
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
                    aria-label={t("New cloud workbook title")}
                    value={newTitle}
                    onChange={(e) => setNewTitle(e.target.value)}
                    placeholder={t("New cloud workbook title")}
                  />
                  <ActionButton
                    className="toolbar-btn primary-btn"
                    data-testid="workbooks-create-btn"
                    inactive={busy}
                    onClick={() => void handleCreateCloud()}
                  >
                    + {t("New Cloud Workbook")}
                  </ActionButton>
                  <ActionButton
                    className="toolbar-btn"
                    inactive={busy}
                    onClick={() => importRef.current?.click()}
                  >
                    {t("Import .rtcl")}
                  </ActionButton>
                  <input
                    ref={importRef}
                    type="file"
                    accept=".rtcl,.rtclsnap,application/octet-stream"
                    style={{ display: "none" }}
                    onChange={(e) => void handleImportFile(e)}
                  />
                </div>
                <div className="page-list">
                  {(cloudLists?.owned ?? []).length === 0 ? (
                    <p className="page-muted">{t("No cloud workbooks yet.")}</p>
                  ) : (
                    (cloudLists?.owned ?? []).map((item) => renderCloudRow(item, true))
                  )}
                </div>
              </>
            )}

            {tab === "shared" && user && (
              <div className="page-list">
                {(cloudLists?.shared ?? []).length === 0 ? (
                  <p className="page-muted">{t("No shared workbooks yet.")}</p>
                ) : (
                  (cloudLists?.shared ?? []).map((item) => renderCloudRow(item, false))
                )}
              </div>
            )}

            {tab === "local" && (
              <div className="page-list">
                {localDocs.length === 0 ? (
                  <p className="page-muted">
                    {t("No local OPFS workbooks found in this browser.")}
                  </p>
                ) : (
                  localDocs.map((entry) => (
                    <div key={entry.id} className="page-item" data-testid={`local-wb-${entry.id}`}>
                      <div className="page-item-meta">
                        <strong>{entry.title}</strong>
                        <span className="page-muted">
                          {t("id: {id} · {size}", {
                            id: entry.id,
                            size: fmtBytes(entry.sizeBytes),
                          })}
                        </span>
                        {tablesText(entry.id) && (
                          <span className="page-muted" data-testid={`local-tables-${entry.id}`}>
                            {tablesText(entry.id)}
                          </span>
                        )}
                      </div>
                      <div className="collab-row">
                        <button
                          type="button"
                          className="toolbar-btn primary-btn"
                          onClick={() => onNavigate(`/app?doc=${encodeURIComponent(entry.id)}`)}
                        >
                          {t("Open Local")}
                        </button>
                        {user && (
                          <ActionButton
                            className="toolbar-btn"
                            data-testid={`convert-cloud-${entry.id}`}
                            inactive={busy}
                            onClick={() => void handleConvertLocal(entry)}
                          >
                            {t("Convert to Cloud")}
                          </ActionButton>
                        )}
                        {entry.id !== "sample-1m" && (
                          <ActionButton
                            className="toolbar-btn"
                            aria-label={`${t("Delete Local")}: ${entry.title}`}
                            inactive={busy}
                            onClick={() => void handleDeleteLocal(entry)}
                          >
                            {t("Delete Local")}
                          </ActionButton>
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
                    <strong>{t("1,000,000-Row Budget Sample")}</strong>
                    <span className="page-muted">
                      {t("22 columns · 100,000 bound formulas · runs locally in Web Worker + OPFS")}
                    </span>
                  </div>
                  <button
                    type="button"
                    className="toolbar-btn primary-btn"
                    onClick={() => onNavigate("/app")}
                  >
                    {t("Open Sample")}
                  </button>
                </div>
                <div className="page-item">
                  <div className="page-item-meta">
                    <strong>{t("10,000,000-Row Data Table")}</strong>
                    <span className="page-muted">
                      {t("15 columns · made in this browser, about 190 MB of its storage")}
                    </span>
                  </div>
                  <button
                    type="button"
                    className="toolbar-btn primary-btn"
                    data-testid="open-sample-table"
                    onClick={() => onNavigate(SAMPLE_TABLE_PATH)}
                  >
                    {t("Open Data Table")}
                  </button>
                </div>
                <div className="page-item">
                  <div className="page-item-meta">
                    <strong>{t("Deterministic Sync Simulation Lab")}</strong>
                    <span className="page-muted">
                      {t(
                        "Interactive 2–3 replica offline merge & 5-replica fault injector without sign-in",
                      )}
                    </span>
                  </div>
                  <button type="button" className="toolbar-btn" onClick={() => onNavigate("/sim")}>
                    {t("Open /sim")}
                  </button>
                </div>
              </div>
            )}
          </Tabs>
        </div>
      </main>
    </div>
  );
}
