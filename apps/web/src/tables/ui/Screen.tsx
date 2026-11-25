import { type RefObject, useEffect, useLayoutEffect, useRef, useSyncExternalStore } from "react";
import type { EditorHandle, ShownTable } from "../../commands/handle.ts";
import { CommandMenu, type Entry } from "../../commands/menus.tsx";
import { withKeys } from "../../editor/load-later.ts";
import { NameBox, type NameBoxProps } from "../../editor/NameBox.tsx";
import { t } from "../../i18n/index.ts";
import { useOpener } from "../../ui/opener.ts";
import type { TableEntry } from "../catalog.ts";
import { FilterDialog } from "./FilterDialog.tsx";
import type { TableModel } from "./model.ts";
import { Panel } from "./Panel.tsx";
import { failureText } from "./text.ts";

const COLUMN: readonly Entry[] = [
  ["copy"],
  "line",
  ["tableSortAscending"],
  ["tableSortDescending"],
  ["tableSortAdd"],
  ["tableSortClear"],
  "line",
  ["tableFilterValues"],
  ["tableFilterCondition"],
  ["tableFilterClear"],
  "line",
  ["autofitCols"],
  ["colWidth"],
  ["freeze:active"],
  ["freeze:none"],
  "line",
  ["tablePanel"],
];

/** What one can do about a table that is not shown. */
function Problem({ model }: { readonly model: TableModel }) {
  const { failure } = useSyncExternalStore(model.subscribe, model.getState);
  const firstRef = useRef<HTMLButtonElement>(null);
  const code = failure?.code;
  const isTaken = code === "blocked" || code === "yielded";
  const isLost = code === "denied" || code === "notFound";
  const canRetry = code !== "unsupported" && code !== "notFound";

  // The grid has nothing to have the focus for.
  useLayoutEffect(() => {
    const { activeElement } = document;
    if (activeElement === document.body || activeElement?.closest(".table-grid")) {
      firstRef.current?.focus();
    }
  }, []);

  const button = (id: string, text: string, onClick: () => void, first = false) => (
    <button
      ref={first ? firstRef : undefined}
      type="button"
      className={first ? "toolbar-btn primary-btn" : "toolbar-btn"}
      data-testid={`table-${id}`}
      onClick={onClick}
    >
      {text}
    </button>
  );
  return (
    <div className="failure table-problem" data-testid="table-problem" data-code={code}>
      <p>{failure && failureText(failure)}</p>
      {failure?.detail && <p className="detail">{failure.detail}</p>}
      <div className="table-actions">
        {isTaken && button("take-over", t("Use Them Here"), () => void model.open(true), true)}
        {canRetry && !isTaken && button("reopen", t("Open Again"), () => void model.open(), true)}
        {!isTaken &&
          !isLost &&
          button("export", t("Export File"), () => void model.run("tableExport"), !canRetry)}
        {!isTaken &&
          !isLost &&
          model.can("tableDelete") &&
          button("delete", t("Delete…"), () => void model.run("tableDelete"))}
      </div>
    </div>
  );
}

export interface ScreenProps {
  readonly model: TableModel;
  readonly handle: EditorHandle;
  readonly entry: TableEntry | undefined;
  readonly shown: ShownTable;
  readonly nameBox: Omit<NameBoxProps, "text" | "inactive">;
  readonly panel: {
    readonly isOpen: boolean;
    readonly opener: RefObject<Element | null>;
    readonly onClose: () => void;
  };
}

/** A data table in view. */
export function Screen({ model, handle, entry, shown, nameBox, panel }: ScreenProps) {
  const state = useSyncExternalStore(model.subscribe, model.getState);
  const { status, selection, text, part, refused } = state;
  const hostRef = useRef<HTMLDivElement>(null);
  const partOpener = useOpener(part !== null && part.kind !== "menu");
  const { focus } = shown;

  useEffect(() => {
    const host = hostRef.current;
    if (!host) {
      return;
    }
    return model.attach(host);
  }, [model]);

  const name = entry?.name ?? "";
  const label = t("Data table {name}", { name });
  const selectedCell = t("Selected cell");
  useEffect(() => {
    model.grid()?.setStrings({ grid: label, cellEditor: "", selectedCell });
  }, [model, label, selectedCell]);

  useEffect(() => {
    if (focus) {
      model.focus();
    }
  }, [model, focus]);

  const isPanelOpen = panel.isOpen;
  useEffect(() => {
    if (isPanelOpen) {
      model.panelOpened();
    }
  }, [model, isPanelOpen]);

  return (
    <section
      className="table-view"
      aria-label={label}
      data-testid="table-view"
      data-status={status}
    >
      <div className="formulabar table-bar">
        <NameBox {...nameBox} text={selection?.name ?? "—"} inactive={status !== "open"} />
        <section
          className="table-text"
          data-testid="table-text"
          aria-label={t("Contents of the active cell")}
          // biome-ignore lint/a11y/noNoninteractiveTabindex: it scrolls when the text is long
          tabIndex={0}
        >
          {text}
        </section>
        <button
          type="button"
          className="toolbar-btn"
          aria-expanded={panel.isOpen}
          data-testid="table-panel-btn"
          title={withKeys(t("Show or Hide the Table Panel"), "tablePanel")}
          onClick={() => model.run("tablePanel")}
        >
          {t("Table")}
        </button>
      </div>
      {refused && (
        <div className="rejected-banner" data-testid="table-refused">
          <span>{failureText(refused)}</span>
          <button
            type="button"
            className="tip-close"
            aria-label={t("Dismiss")}
            onClick={() => {
              model.clearRefused();
              model.focus();
            }}
          >
            ×
          </button>
        </div>
      )}
      <div className="table-body">
        <div className="table-space">
          <div
            ref={hostRef}
            className="grid table-grid"
            data-testid="table-grid"
            inert={status === "failed"}
          />
          {status === "opening" && <p className="table-opening">{t("Opening the data table…")}</p>}
          {status === "failed" && <Problem model={model} />}
        </div>
        {panel.isOpen && (
          <Panel model={model} entry={entry} opener={panel.opener} onClose={panel.onClose} />
        )}
      </div>
      {part?.kind === "menu" && (
        <CommandMenu
          handle={handle}
          label={t("Column {name}", {
            name: state.info?.columns[model.column]?.name ?? "",
          })}
          entries={COLUMN}
          at={part}
          onClose={() => model.openPart(null)}
          testId="table-menu"
        />
      )}
      {part && part.kind !== "menu" && (
        <FilterDialog
          key={`${part.kind}${part.col}`}
          model={model}
          kind={part.kind}
          col={part.col}
          opener={partOpener}
          onClose={() => model.openPart(null)}
        />
      )}
    </section>
  );
}
