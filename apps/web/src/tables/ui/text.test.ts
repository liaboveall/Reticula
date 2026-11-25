import type { ImportReport, TableCondition } from "@reticula/protocol/table";
import { describe, expect, test } from "vitest";
import type { TableFailureCode } from "../failure.ts";
import { conditionText, failureText, kindText, reportLines } from "./text.ts";

/** All there are: a new one has to be put into words, or the types are wrong. */
const CODES: Readonly<Record<TableFailureCode, true>> = {
  corrupt: true,
  io: true,
  memory: true,
  limit: true,
  invalid: true,
  unclosedQuote: true,
  fieldTooLong: true,
  notUtf8: true,
  tooManyColumns: true,
  empty: true,
  changed: true,
  notParquet: true,
  encrypted: true,
  unsupported: true,
  blocked: true,
  yielded: true,
  closed: true,
  notFound: true,
  inUse: true,
  quota: true,
  busy: true,
  cancelled: true,
  superseded: true,
  invalidName: true,
  duplicateName: true,
  tooLarge: true,
  denied: true,
  offline: true,
  uploading: true,
  accountFull: true,
  serverFull: true,
  fileTooLarge: true,
  refused: true,
  crashed: true,
  load: true,
};

describe("why something could not be done", () => {
  test("is a sentence, unless it was wanted", () => {
    const texts = (Object.keys(CODES) as TableFailureCode[]).map(
      (code) => [code, failureText({ code, row: 1_234 })] as const,
    );
    expect(texts.filter(([, text]) => text === "").map(([code]) => code)).toEqual([
      "cancelled",
      "superseded",
    ]);
    for (const [code, text] of texts) {
      expect(text, code).toMatch(/^(?:|[A-Z].*\.)$/);
    }
    expect(new Set(texts.map(([, text]) => text)).size).toBe(texts.length - 1);
  });

  test("says where in a file", () => {
    expect(failureText({ code: "unclosedQuote", row: 1_234 })).toBe(
      "The quote that begins in row 1,234 is never closed.",
    );
  });
});

describe("conditions", () => {
  test.each<[TableCondition, string]>([
    [{ kind: "oneOf", keys: ["a"], blank: false }, "one value chosen"],
    [{ kind: "oneOf", keys: ["a", "b"], blank: true }, "3 values chosen"],
    [{ kind: "oneOf", keys: [], blank: true }, "one value chosen"],
    [{ kind: "contains", text: "x" }, "contains “x”"],
    [{ kind: "number", op: "atLeast", a: "1.50" }, "≥ 1.50"],
    [{ kind: "number", op: "between", a: "1", b: "2" }, "between 1 and 2"],
    [{ kind: "time", op: "before", a: "2026-01-02T03:04:05" }, "before 2026-01-02 03:04:05"],
    [
      { kind: "time", op: "between", a: "2026-01-02", b: "2026-02-03" },
      "between 2026-01-02 and 2026-02-03",
    ],
    [{ kind: "top", n: 10, bottom: true }, "the 10 smallest"],
    [{ kind: "notBlank" }, "not blank"],
  ])("%j are told as %s", (condition, text) => {
    expect(conditionText(condition)).toBe(text);
  });
});

test("types are told apart", () => {
  expect(kindText({ kind: "decimal", scale: 2, fixed: true })).toBe("Decimal (2 places)");
  expect(kindText({ kind: "dateTime" })).toBe("Date and time");
});

test("a report has a line for everything that is not as it was in the file", () => {
  const column = {
    name: "price",
    type: { kind: "integer" },
    blanks: 5,
    misfits: 0,
    cutTexts: 0,
    cutNanos: 0,
    notFinite: 0,
    outOfRange: 0,
  } as const;
  const report: ImportReport = {
    columns: [column],
    skipped: [],
    extraFields: 0,
    shortRows: 0,
    emptyLines: 0,
  };
  expect(reportLines(report)).toEqual([]);
  expect(
    reportLines({
      columns: [
        { ...column, misfits: 1_500, cutTexts: 1, cutNanos: 2, notFinite: 3, outOfRange: 4 },
      ],
      skipped: [{ name: "tags", reason: "nested" }],
      extraFields: 1,
      shortRows: 2,
      emptyLines: 3,
    }),
  ).toEqual([
    "1 fields were dropped: their rows have too many.",
    "2 rows were too short and were filled with blanks.",
    "3 empty lines were skipped.",
    "price: 1,500 values did not fit the type and are blank.",
    "price: 1 texts were longer than 1 MiB and were cut.",
    "price: 2 times were cut to microseconds.",
    "price: 3 values were not finite numbers and are blank.",
    "price: 4 values are out of range and are blank.",
    "tags was left out: lists, maps or structures.",
  ]);
});
