import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  functionParameters,
  functionSummary,
  MESSAGE,
  NUMBER,
  translateEngineText,
} from "./engine-text.ts";
import { type Catalogue, fill } from "./index.ts";
import { placeholders } from "./scan.ts";
import { isEnglish, messagesIn } from "./scan-engine.ts";
import zhEngine from "./zh-engine.ts";
import zhFunctions from "./zh-functions.ts";

// What the engine, the workers and the server write is all translated. What
// Rust writes is listed by the code generator, what the workers write is read
// off their sources.

interface FunctionTexts {
  readonly name: string;
  readonly params: readonly string[];
  readonly summary: string;
}

interface UiMessages {
  /** By the files that write them. */
  readonly messages: Readonly<Record<string, readonly string[]>>;
  /** What a placeholder may have in it, by its name. */
  readonly values: Readonly<Record<string, readonly string[]>>;
  readonly functions: readonly FunctionTexts[];
}

const ROOT = fileURLToPath(new URL("../../../..", import.meta.url));
const read = (file: string) => readFileSync(ROOT + file, "utf8");

const fixture: UiMessages = JSON.parse(read("packages/protocol/src/generated/ui-messages.json"));

const ENGINE = "apps/web/src/engine";
const WRITERS = [
  ...readdirSync(ROOT + ENGINE)
    .filter((name) => name.endsWith(".ts") && !name.endsWith(".test.ts"))
    .map((name) => `${ENGINE}/${name}`),
  "packages/protocol/src/generated/collab-api.ts",
];

/** What reads like English in them and is no message. */
const NO_MESSAGES = new Set([
  // Titles, which are stored with the workbooks.
  "Sample sales data",
  "Unsynced changes - {title}",
  "Untitled",
  // Sent to the server.
  "StateDigest mismatch doc={docId} seq={seq} expected={expected} actual={actual}",
]);

const found = WRITERS.flatMap((file) => messagesIn(file, read(file)));

const messages = new Set([
  ...Object.values(fixture.messages).flat(),
  ...Object.values(fixture.values).flat(),
  ...found.filter((text) => !NO_MESSAGES.has(text)),
]);

/** The catalogues of the language shown. */
let shown: Readonly<Record<string, Catalogue>> = {};

vi.mock("./index.ts", async (original) => ({
  ...(await original<typeof import("./index.ts")>()),
  catalogueOf: (name: string) => shown[name],
}));

beforeEach(() => {
  shown = { "zh-engine": zhEngine, "zh-functions": zhFunctions };
});

describe("the messages", () => {
  it("are found", () => {
    expect(messages.size).toBeGreaterThan(400);
    expect(found).toContain("The engine worker stopped.");
    expect(found).toContain("Could not load snapshot #{snapshotSeq} (HTTP {status})");
    expect(found).toContain("Reticula API returned {status}");
    expect([...NO_MESSAGES].filter((text) => !found.includes(text))).toEqual([]);
  });

  it("are told from code", () => {
    const source = `
      import a from "not this one";
      console.warn("nor this");
      socket.close(1000, "nor that");
      const kind: "Open" | "all of them" = "open";
      throw new Error(\`\${doc.title} has \${rows} rows\`);
      post({ reason: "Try again.", code: "try_again", label: "Sort" });
    `;
    expect(messagesIn("a.ts", source)).toEqual(["{title} has {rows} rows", "Try again.", "Sort"]);
    expect(["sort", "SUM", "{a} {b}", "a=1 b=2"].filter(isEnglish)).toEqual([]);
  });

  it("are translated, and nothing else is", () => {
    const keys = Object.keys(zhEngine);
    expect([...messages].filter((message) => !Object.hasOwn(zhEngine, message))).toEqual([]);
    expect(keys.filter((key) => !messages.has(key))).toEqual([]);
  });

  it("keep what is put into them", () => {
    const different = Object.entries(zhEngine).filter(
      ([message, translation]) => placeholders(message).join() !== placeholders(translation).join(),
    );
    expect(different).toEqual([]);
  });
});

/** Something a message may have in a placeholder. */
function valueFor(name: string, at: number): string {
  if (name === "pos_hint") {
    return "at top";
  }
  if (name === "letters") {
    return "AB";
  }
  if (NUMBER.test(name)) {
    return `${at + 1}0${at}`;
  }
  return MESSAGE.test(name) ? "Select at least one row to move." : `“Größe” ${at}`;
}

describe("translateEngineText", () => {
  it.each(Object.entries(zhEngine))("knows %j by what is written", (message, translation) => {
    const written: Record<string, string> = {};
    const read: Record<string, string> = {};
    for (const [at, name] of placeholders(message).entries()) {
      const value = valueFor(name, at);
      written[name] = value;
      read[name] = zhEngine[value] ?? value;
    }
    expect(translateEngineText(fill(message, written))).toBe(fill(translation, read));
  });

  it.each([
    ["Delete rows", "删除行"],
    ["Delete Rates", "删除名称 Rates"],
    ["Delete 2 row(s) on Delete rows", "在 Delete rows 删除 2 行"],
    ["Insert row", "插入行"],
    ["Insert 1000000 rows", "插入 1000000 行"],
    ["Edit 'Q1 Plan'!B2", "编辑 'Q1 Plan'!B2"],
    ["Edit Rates!C5", "编辑 Rates!C5"],
    ["Edit R2C3", "编辑 R2C3"],
    ["Edit Q1 Plan!B2:C9 (12 cells)", "编辑 Q1 Plan!B2:C9（12 个单元格）"],
    ["Undo: Insert 3 rows", "撤销：插入 3 行"],
    ["Redo: Undo: Delete Rates", "重做：撤销：删除名称 Rates"],
    ["Undo Edit R2C3", "撤销：编辑 R2C3"],
    ["Merge: AI: add totals", "合并：AI: add totals"],
    ["Invalid token in formula (unterminated text).", "公式中有无效的内容（文本缺少右引号）。"],
    [
      "Invalid named range formula: Invalid token in formula (malformed number).",
      "名称的公式无效：公式中有无效的内容（数字格式不正确）。",
    ],
    ["SUM expects at least 1 argument(s), got 0.", "SUM 至少需要 1 个参数，当前有 0 个。"],
    ["ABS expects 1 argument, got 2.", "ABS 需要 1 个参数，当前有 2 个。"],
    ["Unknown function 'SU{M}'.", "未知的函数“SU{M}”。"],
    ["merge rejected: InvalidOp", "合并被拒绝：无效的操作"],
    [
      "merge rejected: NonContiguousClientSeq { expected: 5 }",
      "合并被拒绝：事务序号不连续，应为 5",
    ],
    ["branch is already merged", "分支已处于“已合并”状态"],
    ["workbook title must be between 1 and 120 characters", "工作簿标题须为 1 到 120 个字符"],
    ["Insert 2 row(s) on Q1 at top at row 5", "在 Q1 at top 第 5 行处插入 2 行"],
    ["Insert 1 row(s) on Q1 Plan after anchor row", "在 Q1 Plan 基准行之后插入 1 行"],
    ["agent run exceeded budget: reached max_steps (24)", "运行超过预算：达到步数上限 (24)"],
    ["XLSX read error: something\nelse", "读取 XLSX 出错：something\nelse"],
    ["LLM provider error: upstream HTTP 502: ", "模型服务出错：上游返回 HTTP 502："],
    ["Merge AI draft (1/3)", "合并 AI 草稿 (1/3)"],
    ["Undo: Merge: totals (2/3)", "撤销：合并：totals (2/3)"],
    ["cancelled", "已取消"],
    ["Q1 col B col AB", "Q1 col B AB 列"],
  ])("reads %j", (text, translation) => {
    expect(translateEngineText(text)).toBe(translation);
    // The second time it is remembered.
    expect(translateEngineText(text)).toBe(translation);
  });

  it.each([
    "",
    "Insert many rows",
    "Failed to fetch",
    "constructor",
    "Insert {count} rows, please",
    "'My col B'!C5",
    "Quarterly (1/3)",
    "x".repeat(5_000),
  ])("leaves %j as it is", (text) => {
    expect(translateEngineText(text)).toBe(text);
  });

  it("does not follow messages in messages forever", () => {
    const text = `${"Undo: ".repeat(100)}Paste`;
    expect(translateEngineText(text)).toMatch(/^(?:撤销：){5}(?:Undo: )+Paste$/);
  });

  it("keeps the English as it is when English is shown", () => {
    expect(translateEngineText("Delete rows")).toBe("删除行");
    shown = {};
    for (const message of messages) {
      expect(translateEngineText(message)).toBe(message);
    }
  });

  it("forgets what another catalogue has made of a text", () => {
    expect(translateEngineText("Paste")).toBe("粘贴");
    shown = { "zh-engine": { Paste: "贴上" } };
    expect(translateEngineText("Paste")).toBe("贴上");
    expect(translateEngineText("Delete rows")).toBe("Delete rows");
  });

  it("stays right when more comes than it remembers", () => {
    for (let n = 0; n < 2_000; n++) {
      expect(translateEngineText(`Insert ${n} rows`)).toBe(`插入 ${n} 行`);
    }
  });
});

describe("the functions of formulas", () => {
  const { functions } = fixture;
  const parameters = new Set(functions.flatMap(({ params }) => params));

  it("are all explained in Chinese", () => {
    expect(functions).toHaveLength(200);
    for (const { name, summary } of functions) {
      expect(functionSummary(name, summary), name).toMatch(/\p{Script=Han}.*。$/u);
    }
  });

  it("have names for their parameters", () => {
    const unnamed = [...parameters].filter(
      (param) => param !== "..." && functionParameters(param) === param && param !== "k",
    );
    expect(unnamed).toEqual([]);
  });

  it("have no explanations that are not needed", () => {
    const names = new Set([
      ...functions.map(({ name }) => name),
      ...[...parameters].flatMap((param) => {
        const name = param.replace(/[[\]]/g, "");
        return [name, name.replace(/\d+$/, "")];
      }),
    ]);
    expect(Object.keys(zhFunctions).filter((key) => !names.has(key))).toEqual([]);
  });

  it("show their parameters as they are written", () => {
    expect(functionParameters("[number2]")).toBe("[数值2]");
    expect(functionParameters("(range, criteria, [sum_range])")).toBe("(区域, 条件, [求和区域])");
    expect(functionParameters("(expression, value1, result1, [value2_or_default], ...)")).toBe(
      "(表达式, 值1, 结果1, [值2或默认值], ...)",
    );
    expect(functionParameters("(ref_text, [a1])")).toBe("(引用文本, [A1 样式])");
    expect(functionParameters("()")).toBe("()");
  });

  it("are in English when English is shown", () => {
    shown = {};
    for (const { name, params, summary } of functions) {
      expect(functionSummary(name, summary)).toBe(summary);
      expect(functionParameters(params.join(", "))).toBe(params.join(", "));
    }
  });

  it("may be ones this page does not know", () => {
    expect(functionSummary("NOVEL", "Does something new.")).toBe("Does something new.");
    expect(functionParameters("novelty")).toBe("novelty");
    expect(functionSummary("constructor", "Nothing.")).toBe("Nothing.");
  });
});
