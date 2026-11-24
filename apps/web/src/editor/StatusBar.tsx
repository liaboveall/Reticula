import type { GridStats, Selection } from "@reticula/grid";
import type { SheetInfo } from "@reticula/protocol";
import { fmtMs, t } from "../i18n/index.ts";
import { hint } from "./load-later.ts";
import type { EngineStatus } from "./types.ts";
import { ZoomControl, type ZoomControlProps } from "./ZoomControl.tsx";

export interface StatusBarProps {
  readonly status: EngineStatus;
  /** Whether a workbook is being opened and nothing else is in the way. */
  readonly opening: boolean;
  /** The sheet in view, or `null` while there is none to work on. */
  readonly sheet: SheetInfo | null;
  readonly selection: Selection | null;
  /** What the engine has done last, or is doing. */
  readonly activity: string | null;
  readonly stats: GridStats | null;
  readonly kernelVersion: string;
  readonly zoom: ZoomControlProps | null;
}

function statusText(status: EngineStatus): string {
  switch (status) {
    case "starting":
      return t("starting|engine");
    case "ready":
      return t("ready|engine");
    case "failed":
      return t("failed|engine");
    case "blocked":
      return t("blocked|engine");
    case "takingOver":
      return t("takingOver|engine");
    case "yielded":
      return t("yielded|engine");
    case "safeMode":
      return t("safeMode|engine");
  }
}

export function StatusBar({
  status,
  opening,
  sheet,
  selection,
  activity,
  stats,
  kernelVersion,
  zoom,
}: StatusBarProps) {
  const selected = selection ?? stats?.selection;
  return (
    <footer className="statusbar">
      <span data-testid="engine-status" data-status={status}>
        {statusText(status)}
      </span>
      {opening && <span>{t("Opening local workbook…")}</span>}
      {sheet && (
        <>
          {selected && (
            <span data-testid="selection">
              {selected.name}
              {selected.text ? ` · ${selected.text}` : ""}
            </span>
          )}
          {activity && (
            <span className="recalc-pill" data-testid="recalc-status">
              {activity}
            </span>
          )}
          <span className="spacer" />
          {sheet.seed !== 0 && (
            <span title={hint("seed")}>{t("seed {seed}", { seed: sheet.seed })}</span>
          )}
          <span data-testid="generation" title={hint("generation")}>
            {t("generated in {time}", { time: fmtMs(sheet.generationMs) })}
          </span>
          {stats && stats.frameWork.samples > 0 && (
            <span data-testid="frame-work" title={hint("frame")}>
              {t("frame {p50} · p99 {p99}", {
                p50: fmtMs(stats.frameWork.p50),
                p99: fmtMs(stats.frameWork.p99),
              })}
            </span>
          )}
          <span data-testid="kernel-version">
            {t("kernel {version}", { version: kernelVersion })}
          </span>
          {zoom && <ZoomControl {...zoom} />}
        </>
      )}
    </footer>
  );
}
