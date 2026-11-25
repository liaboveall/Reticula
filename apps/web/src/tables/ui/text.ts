import type {
  ImportReport,
  SkipReason,
  TableColumnType,
  TableCondition,
} from "@reticula/protocol/table";
import { fmtInt, t, tn } from "../../i18n/index.ts";
import type { TableFailure } from "../failure.ts";

// What there is to say about tables, in the language shown.

/** Why something could not be done. Empty when that was what was wanted. */
export function failureText({ code, row }: TableFailure): string {
  switch (code) {
    case "corrupt":
      return t("The file is damaged, or is not a data table.");
    case "io":
      return t("The file could not be read or written.");
    case "memory":
      return t("There is not enough memory for this.");
    case "limit":
      return t("This is more than a data table can hold or copy at once.");
    case "invalid":
      return t("The condition does not fit the column.");
    case "unclosedQuote":
      return t("The quote that begins in row {row} is never closed.", { row: fmtInt(row ?? 0) });
    case "fieldTooLong":
      return t("A field in quotes in row {row} is longer than 1 MiB.", { row: fmtInt(row ?? 0) });
    case "notUtf8":
      return t("Row {row} is not UTF-8. Choose the encoding the file is in.", {
        row: fmtInt(row ?? 0),
      });
    case "tooManyColumns":
      return t("The file has more than 1,024 columns.");
    case "empty":
      return t("There is nothing in the file that a data table could hold.");
    case "changed":
      return t("The file was changed while it was read.");
    case "notParquet":
      return t("This is not a Parquet file.");
    case "encrypted":
      return t("The file is encrypted.");
    case "unsupported":
      return t(
        "This browser cannot keep data tables: it does not let workers write files. Current versions of Chrome, Edge, Firefox and Safari do.",
      );
    case "blocked":
      return t("The data tables of this workbook are open in another tab.");
    case "yielded":
      return t("Another tab has the data tables of this workbook now.");
    case "closed":
      return t("The data table was closed.");
    case "notFound":
      return t("The data table is not there any more.");
    case "inUse":
      return t("The file is in use by another tab.");
    case "quota":
      return t("There is not enough storage left in this browser. Nothing of the table was kept.");
    case "busy":
      return t("Another file is being imported.");
    case "invalidName":
      return t("A name has 1 to 255 characters.");
    case "duplicateName":
      return t("Another data table has this name.");
    case "tooLarge":
      return t("No more than {n} cells can be copied at once.", { n: fmtInt(200_000) });
    case "denied":
      return t("You may not do this with the data tables of this workbook.");
    case "offline":
      return t("There is no connection, and not all of the data table is in this browser.");
    case "uploading":
      return t("The data table is still being uploaded.");
    case "accountFull":
      return t("The owner of the workbook has no room left for data tables on the server.");
    case "serverFull":
      return t("The server has no room left for data tables.");
    case "fileTooLarge":
      return t("The data table is larger than the server accepts.");
    case "refused":
      return t("The server does not accept the data table.");
    case "crashed":
      return t("What shows data tables has stopped. The rest of the workbook is not affected.");
    case "load":
      return t("What shows data tables could not be loaded. Reload the page to try again.");
    case "cancelled":
    case "superseded":
      return "";
  }
}

export function kindText(type: TableColumnType): string {
  switch (type.kind) {
    case "integer":
      return t("Integer");
    case "decimal":
      return t("Decimal ({n} places)", { n: type.scale });
    case "float":
      return t("Number|type");
    case "date":
      return t("Date");
    case "dateTime":
      return t("Date and time");
    case "boolean":
      return t("True or false");
    case "text":
      return t("Text|type");
  }
}

export function conditionText(condition: TableCondition): string {
  switch (condition.kind) {
    case "oneOf":
      return tn(
        condition.keys.length + (condition.blank ? 1 : 0),
        "one value chosen",
        "{n} values chosen",
      );
    case "contains":
      return t("contains “{text}”", { text: condition.text });
    case "startsWith":
      return t("begins with “{text}”", { text: condition.text });
    case "endsWith":
      return t("ends with “{text}”", { text: condition.text });
    case "number": {
      const { op, a, b = "" } = condition;
      const signs = {
        equal: "=",
        notEqual: "≠",
        greater: ">",
        atLeast: "≥",
        less: "<",
        atMost: "≤",
      } as const;
      return op === "between" ? t("between {a} and {b}", { a, b }) : `${signs[op]} ${a}`;
    }
    case "time": {
      const a = condition.a.replace("T", " ");
      const b = condition.b?.replace("T", " ") ?? "";
      switch (condition.op) {
        case "before":
          return t("before {a}", { a });
        case "after":
          return t("after {a}", { a });
        case "on":
          return t("on {a}", { a });
        case "between":
          return t("between {a} and {b}", { a, b });
      }
      return "";
    }
    case "top":
      return condition.bottom
        ? t("the {n} smallest", { n: fmtInt(condition.n) })
        : t("the {n} largest", { n: fmtInt(condition.n) });
    case "blank":
      return t("blank|cell");
    case "notBlank":
      return t("not blank");
  }
}

function skipText(reason: SkipReason): string {
  switch (reason) {
    case "nested":
      return t("lists, maps or structures");
    case "binary":
      return t("bytes that are not text");
    case "packing":
      return t("an encoding that cannot be read");
    case "type":
      return t("a type that data tables do not have");
  }
}

/** What there is to know about an import, a line each. */
export function reportLines(report: ImportReport): string[] {
  const lines: string[] = [];
  const count = (n: number, say: (n: string) => string) => {
    if (n > 0) {
      lines.push(say(fmtInt(n)));
    }
  };
  count(report.extraFields, (n) => t("{n} fields were dropped: their rows have too many.", { n }));
  count(report.shortRows, (n) => t("{n} rows were too short and were filled with blanks.", { n }));
  count(report.emptyLines, (n) => t("{n} empty lines were skipped.", { n }));
  for (const column of report.columns) {
    const { name } = column;
    count(column.misfits, (n) =>
      t("{name}: {n} values did not fit the type and are blank.", { name, n }),
    );
    count(column.cutTexts, (n) =>
      t("{name}: {n} texts were longer than 1 MiB and were cut.", { name, n }),
    );
    count(column.cutNanos, (n) => t("{name}: {n} times were cut to microseconds.", { name, n }));
    count(column.notFinite, (n) =>
      t("{name}: {n} values were not finite numbers and are blank.", { name, n }),
    );
    count(column.outOfRange, (n) =>
      t("{name}: {n} values are out of range and are blank.", { name, n }),
    );
  }
  for (const { name, reason } of report.skipped) {
    lines.push(t("{name} was left out: {reason}.", { name, reason: skipText(reason) }));
  }
  return lines;
}
