import { useEffect } from "react";
import { ProgressBox } from "../../editor/Banners.tsx";
import { fmtInt, t } from "../../i18n/index.ts";
import { announce } from "../../ui/announcer.ts";
import type { Importing } from "./session.ts";

/** At which parts of an import its progress is read out. */
const ANNOUNCED_PERCENT = 25;

export function progressText({ name, progress }: Importing): string {
  if (!progress) {
    return t("Importing {name}…", { name });
  }
  const percent = Math.floor(progress.share * 100);
  const { rows, pass = 1 } = progress;
  if (rows === undefined) {
    return t("Importing {name}: {percent}%", { name, percent });
  }
  const params = { name, percent, rows: fmtInt(rows) };
  // The types of the columns have turned out to be others than the first rows have.
  return pass > 1
    ? t("Importing {name}: {percent}%, {rows} rows, second reading", params)
    : t("Importing {name}: {percent}%, {rows} rows", params);
}

/** How far an import has got. */
export function Progress({
  importing,
  testId,
}: {
  readonly importing: Importing;
  readonly testId: string;
}) {
  const percent = Math.floor((importing.progress?.share ?? 0) * 100);
  const part = Math.floor(percent / ANNOUNCED_PERCENT);
  const { name } = importing;
  useEffect(() => {
    if (part > 0) {
      announce(t("Importing {name}: {percent}%", { name, percent: part * ANNOUNCED_PERCENT }));
    }
  }, [name, part]);
  return (
    <ProgressBox
      testId={testId}
      text={progressText(importing)}
      label={t("Import")}
      percent={percent}
    />
  );
}
