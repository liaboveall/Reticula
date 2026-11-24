import { t } from "../i18n/index.ts";
import type { FreezeChoice } from "./types.ts";

/** The number formats there are to choose from, by their codes. */
export function numberFormats(): readonly { readonly value: string; readonly text: string }[] {
  return [
    { value: "General", text: t("General") },
    { value: "0", text: t("Integer (0)") },
    { value: "0.00", text: t("Number (0.00)") },
    { value: "#,##0.00", text: t("Thousands (#,##0.00)") },
    { value: "$#,##0.00", text: t("USD ($#,##0.00)") },
    { value: "¥#,##0.00", text: t("CNY (¥#,##0.00)") },
    { value: "€#,##0.00", text: t("EUR (€#,##0.00)") },
    { value: "0%", text: t("Percent (0%)") },
    { value: "0.00%", text: t("Percent (0.00%)") },
    { value: "0.00E+00", text: t("Scientific (0.00E+00)") },
    { value: "yyyy-mm-dd", text: t("Date (yyyy-mm-dd)") },
    { value: "yyyy-mm-dd hh:mm", text: t("DateTime (yyyy-mm-dd hh:mm)") },
    { value: "@", text: t("Plain Text (@)") },
  ];
}

export function freezeChoices(): readonly {
  readonly value: FreezeChoice;
  readonly text: string;
}[] {
  return [
    { value: "none", text: t("No Freeze") },
    { value: "row1", text: t("Freeze Top Row") },
    { value: "col1", text: t("Freeze First Column") },
    { value: "row1col1", text: t("Freeze Row 1 & Col A") },
    { value: "active", text: t("Freeze to Active Cell") },
  ];
}
