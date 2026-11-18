// The page for checking input methods by hand. See ime.html.

import "@reticula/grid/grid.css";
import "./ime.css";
import { Grid } from "@reticula/grid";
import { CellEditor, type CellPosition, type EditorEvent } from "@reticula/grid/editor";
import { startEngine } from "../engine/client.ts";
import type { ImeHooks } from "./hooks.ts";

/** What to try, and what should happen. */
const CHECKS: readonly (readonly [string, string])[] = [
  ["选中一格，直接敲英文字母。", "进入编辑；字母出现在编辑器里，只出现一次。"],
  ["选中一格，切到中文，直接打拼音。", "进入编辑；正在组合的拼音显示在这一格的位置上。"],
  ["看候选框的位置。", "候选框贴着这一格。"],
  ["看第一次组合的首字母。", "不丢，不重复。"],
  ["组合中用方向键或数字键选候选。", "选中的格不动。"],
  ["组合中按回车或空格上屏。", "只上屏；不提交这一格，模式仍是“编辑”。"],
  ["组合中按 Esc。", "只影响这次组合，不退出编辑。输入法具体怎么表现，记在备注里。"],
  ["上屏之后再按回车。", "提交；选中的格下移；编辑器消失；接着直接打字还能进入编辑。"],
  ["编辑中、没有组合时按 Esc。", "取消编辑；“上次提交”不变。"],
  ["按 Shift 切换中英文。", "表格没有任何动作。"],
  ["按 F2。", "进入编辑，光标在末尾，可以马上用输入法接着打。"],
  ["用滚轮滚动表格，再打拼音。", "候选框的位置仍然贴着这一格。"],
  ["组合中用鼠标点别的格。", "组合被正常结束。是上屏了还是取消了，记在备注里。"],
  ["把浏览器缩放到 125% 和 150%，各打一次拼音。", "候选框的位置仍然对。"],
  ["选中一格，按 Ctrl+C、Ctrl+X、Ctrl+V。", "“事件”里各出现一行“剪贴板”。"],
];

const VERDICTS = [
  ["pass", "通过"],
  ["fail", "不通过"],
  ["skip", "未测"],
] as const;

function element<T extends HTMLElement = HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!found) {
    throw new Error(`ime.html has no #${id}`);
  }
  return found as T;
}

function columnName(col: number): string {
  let name = "";
  for (let rest = col; rest >= 0; rest = Math.floor(rest / 26) - 1) {
    name = String.fromCharCode(65 + (rest % 26)) + name;
  }
  return name;
}

const cellName = (cell: CellPosition) => `${columnName(cell.col)}${cell.row + 1}`;

// --- the grid and its editor ----------------------------------------------

const host = element("grid");
const events: EditorEvent[] = [];
/** What was typed into each cell. The engine does not take edits yet, so it is kept here. */
const typed = new Map<string, string>();
let lastCommit = "—";

const client = startEngine({
  onMessage(message) {
    if (message.kind === "ready") {
      grid.setSheet(message.sheet);
      grid.select({ row: 1, col: 3 });
      editor.focus();
    } else if (message.kind === "failed") {
      log(`引擎停止了：${message.message}`);
    }
  },
  onPage: (page) => grid.acceptPage(page),
});
const grid = new Grid(host, {
  subscribe: (subscription) => client.send({ kind: "subscribe", sheet: 0, ...subscription }),
});
const editor = new CellEditor(grid, host, {
  textOf: (cell) => typed.get(cellName(cell)) ?? grid.textAt(cell.row, cell.col) ?? "",
  onCommit(cell, text) {
    typed.set(cellName(cell), text);
    lastCommit = `${cellName(cell)} ← “${text}”`;
  },
  onEvent(event) {
    events.push(event);
    log(describe(event));
    showState();
    setTimeout(showState, 0);
  },
});
grid.watchFrames(showState);

// --- what the editor is doing ---------------------------------------------

function showState(): void {
  const editing = editor.editing;
  element("mode").textContent = editor.composing ? "编辑（组合中）" : editing ? "编辑" : "导航";
  const cell = editing ?? grid.selection;
  element("cell").textContent = cell ? cellName(cell) : "—";
  element("text").textContent = editor.text;
  element("committed").textContent = lastCommit;
}

function describe(event: EditorEvent): string {
  switch (event.kind) {
    case "started":
      return `进入编辑（${{ typing: "打字", composition: "输入法组合", F2: "F2", doubleClick: "双击", formulaBar: "公式栏" }[event.cause]}）`;
    case "committed":
      return `提交 ${cellName(event.cell)} ← “${event.text}”`;
    case "cancelled":
      return `取消编辑 ${cellName(event.cell)}`;
    case "composition":
      return `组合${{ start: "开始", update: "更新", end: "结束" }[event.phase]} “${event.data}”`;
    case "key": {
      const outcome = {
        "input method": "交给输入法",
        moved: "移动选中的格",
        committed: "提交",
        cancelled: "取消",
        swallowed: "不作处理",
        passed: "交给编辑器或浏览器",
      }[event.outcome];
      return `按键 ${event.key === " " ? "Space" : event.key} → ${outcome}`;
    }
    case "clipboard":
      return `剪贴板 ${event.type}`;
  }
}

const started = performance.now();

function log(line: string): void {
  const list = element("log");
  const item = document.createElement("li");
  const time = document.createElement("span");
  time.className = "time";
  time.textContent = `${((performance.now() - started) / 1000).toFixed(3).padStart(8)}  `;
  item.append(time, line);
  list.prepend(item);
  while (list.childElementCount > 300) {
    list.lastElementChild?.remove();
  }
}

// What the browser itself reports about text arriving, next to what the
// editor made of it.
for (const type of ["beforeinput", "input"] as const) {
  host.addEventListener(
    type,
    (event) => {
      if (event instanceof InputEvent) {
        const composing = event.isComposing ? "，组合中" : "";
        log(`  ${type} ${event.inputType} “${event.data ?? ""}”${composing}`);
      }
      // The editor takes the text in a moment later than the event.
      setTimeout(showState, 0);
    },
    { capture: true },
  );
}

element("clear").addEventListener("click", () => element("log").replaceChildren());

// --- the checklist ---------------------------------------------------------

const checklist = element("checklist");
CHECKS.forEach(([action, expected], index) => {
  const item = document.createElement("li");
  const what = document.createElement("div");
  what.textContent = action;
  const expect = document.createElement("div");
  expect.className = "expect";
  expect.textContent = `应当：${expected}`;
  const verdict = document.createElement("div");
  verdict.className = "verdict";
  for (const [value, label] of VERDICTS) {
    const choice = document.createElement("label");
    const radio = document.createElement("input");
    radio.type = "radio";
    radio.name = `check-${index}`;
    radio.value = value;
    radio.checked = value === "skip";
    radio.addEventListener("change", () => {
      item.dataset.verdict = value;
      report();
    });
    choice.append(radio, ` ${label}`);
    verdict.append(choice);
  }
  const note = document.createElement("input");
  note.type = "text";
  note.placeholder = "备注";
  note.autocomplete = "off";
  note.setAttribute("aria-label", `第 ${index + 1} 条的备注`);
  note.addEventListener("input", report);
  verdict.append(note);
  item.append(what, expect, verdict);
  checklist.append(item);
});

function report(): string {
  const lines = [
    "S10 输入法手测",
    `浏览器：${navigator.userAgent}`,
    `缩放（devicePixelRatio）：${window.devicePixelRatio}`,
    `输入法：${element<HTMLInputElement>("method").value || "（未填）"}`,
  ];
  CHECKS.forEach(([action], index) => {
    const item = checklist.children[index];
    const chosen = item?.querySelector<HTMLInputElement>("input[type=radio]:checked")?.value;
    const label = VERDICTS.find(([value]) => value === chosen)?.[1] ?? "未测";
    const note = item?.querySelector<HTMLInputElement>("input[type=text]")?.value ?? "";
    lines.push(`${index + 1}. [${label}] ${action}${note ? ` 备注：${note}` : ""}`);
  });
  const text = lines.join("\n");
  element<HTMLTextAreaElement>("report").value = text;
  return text;
}

element("method").addEventListener("input", report);
element("copy").addEventListener("click", () => {
  const text = report();
  // Where the clipboard cannot be written, the text is there to be selected.
  navigator.clipboard?.writeText(text).catch(() => element<HTMLTextAreaElement>("report").select());
});
report();
showState();

// --- for automated checks --------------------------------------------------

const hooks: ImeHooks = {
  events,
  state() {
    const box = host.querySelector(".reticula-editor");
    const bounds = box?.getBoundingClientRect();
    return {
      editing: editor.editing,
      composing: editor.composing,
      text: editor.text,
      selection: grid.selection,
      committed: Object.fromEntries(typed),
      shown: box instanceof HTMLElement && getComputedStyle(box).opacity === "1",
      box: { left: bounds?.left ?? 0, top: bounds?.top ?? 0 },
    };
  },
  grid: () => grid.stats(),
  textAt: (row, col) => grid.textAt(row, col),
};
window.__ime = hooks;
