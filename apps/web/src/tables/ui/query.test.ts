import type { TableColumn, TableQuery } from "@reticula/protocol/table";
import { describe, expect, test } from "vitest";
import {
  applies,
  conditionOn,
  filteredBy,
  fitting,
  initialWidth,
  isPlain,
  markTexts,
  parseTarget,
  sortedAlsoBy,
  sortedBy,
  unsorted,
} from "./query.ts";

const NONE: TableQuery = { filters: [], order: [] };
const column = (kind: "text" | "integer" | "date"): TableColumn => ({
  name: kind,
  type: { kind },
  blanks: 0,
  dictionary: null,
});

describe("an order", () => {
  test("is by one column, then by more of them", () => {
    const one = sortedBy(sortedBy(NONE, 4, false), 2, true);
    expect(one.order).toEqual([{ column: 2, descending: true }]);
    const two = sortedAlsoBy(one, 5);
    expect(two.order).toEqual([
      { column: 2, descending: true },
      { column: 5, descending: false },
    ]);
    expect(sortedAlsoBy(two, 2).order[0]).toEqual({ column: 2, descending: false });
    expect(unsorted(two, 2).order).toEqual([{ column: 5, descending: false }]);
    expect(isPlain(unsorted(two))).toBe(true);
  });
});

describe("conditions", () => {
  test("are one for a column, and the others stay", () => {
    const one = filteredBy(NONE, 1, { kind: "blank" });
    const two = filteredBy(one, 3, { kind: "contains", text: "x" });
    const changed = filteredBy(two, 1, { kind: "notBlank" });
    expect(conditionOn(changed, 1)).toEqual({ kind: "notBlank" });
    expect(changed.filters).toHaveLength(2);
    expect(conditionOn(filteredBy(changed, 1, null), 1)).toBeNull();
    expect(conditionOn(NONE, 1)).toBeNull();
  });

  test("are for columns of some kinds", () => {
    expect(applies({ kind: "contains", text: "" }, "text")).toBe(true);
    expect(applies({ kind: "contains", text: "" }, "integer")).toBe(false);
    expect(applies({ kind: "number", op: "less", a: "1" }, "decimal")).toBe(true);
    expect(applies({ kind: "number", op: "less", a: "1" }, "date")).toBe(false);
    expect(applies({ kind: "time", op: "on", a: "2026-01-01" }, "dateTime")).toBe(true);
    expect(applies({ kind: "top", n: 3, bottom: false }, "boolean")).toBe(false);
    expect(applies({ kind: "oneOf", keys: [], blank: true }, "boolean")).toBe(true);
  });

  test("of another table are left out", () => {
    const query: TableQuery = {
      filters: [
        { column: 0, condition: { kind: "contains", text: "a" } },
        { column: 1, condition: { kind: "contains", text: "a" } },
        { column: 9, condition: { kind: "blank" } },
      ],
      order: [
        { column: 1, descending: true },
        { column: 1, descending: false },
        { column: 3, descending: false },
      ],
    };
    expect(fitting(query, [column("text"), column("integer"), column("date")])).toEqual({
      filters: [{ column: 0, condition: { kind: "contains", text: "a" } }],
      order: [{ column: 1, descending: true }],
    });
  });
});

describe("what is typed into the name box", () => {
  const NAMES = ["id", "Amount", "c", "unit price", "2024", "Straße"];
  test.each([
    ["1", { row: 0, col: null }],
    ["10,000,000", { row: 9_999_999, col: null }],
    [" 12 ", { row: 11, col: null }],
    ["amount", { row: null, col: 1 }],
    ["AMOUNT 7", { row: 6, col: 1 }],
    // The column that is called so, and not the third one.
    ["C", { row: null, col: 2 }],
    ["B", { row: null, col: 1 }],
    ["b12", { row: 11, col: 1 }],
    ["unit price", { row: null, col: 3 }],
    ["unit price 3", { row: 2, col: 3 }],
    ["straße 2", { row: 1, col: 5 }],
    ["a", { row: null, col: 0 }],
  ])("means %j", (text, target) => {
    expect(parseTarget(text, NAMES, 10_000_000)).toEqual(target);
  });

  test.each(["", "0", "10,000,001", "G", "ZZZ", "amount 0", "nothing", "amount 99999999", "-1"])(
    "does not mean anything: %j",
    (text) => {
      expect(parseTarget(text, NAMES, 10_000_000)).toBeNull();
    },
  );
});

describe("copied cells", () => {
  test("that are texts are marked, column by column", () => {
    const html = "<table><tbody><tr><td>1</td><td>00123</td></tr><tr><td>2</td><td></td></tr>";
    const marked = markTexts(html, [false, true]);
    expect(marked.match(/<td[^>]*>/g)).toEqual([
      "<td>",
      expect.stringContaining("data-reticula-text"),
      "<td>",
      expect.stringContaining('mso-number-format:"\\@"'),
    ]);
    expect(marked.replace(/<td[^>]*>/g, "<td>")).toBe(html);
    expect(markTexts(html, [false, false])).toBe(html);
    // Texts cannot have tags in them.
    expect(markTexts("<tr><td>&lt;td&gt;</td></tr>", [true])).toContain(">&lt;td&gt;</td>");
  });
});

describe("a column", () => {
  test("is as wide as what it is for takes, and as its name does", () => {
    const named = (name: string, kind: "integer" | "dateTime") =>
      initialWidth({ name, type: { kind } });
    expect(named("id", "integer")).toBe(112);
    expect(named("at", "dateTime")).toBe(170);
    expect(named("customer number", "integer")).toBeGreaterThan(150);
    expect(named("名".repeat(10), "integer")).toBeGreaterThan(named("n".repeat(10), "integer"));
    expect(named("n".repeat(1_000), "integer")).toBe(240);
  });
});
