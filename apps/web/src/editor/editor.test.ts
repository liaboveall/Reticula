import type { GridStats, Selection } from "@reticula/grid";
import type { SheetInfo } from "@reticula/protocol";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { type Activity, activityText } from "./activity.ts";
import { StatusBar, type StatusBarProps } from "./StatusBar.tsx";
import type { EngineStatus } from "./types.ts";

// What is loaded later may arrive when the test is over.
vi.mock("../i18n/zh-engine.ts", () => ({ default: {} }));
vi.mock("../i18n/zh-functions.ts", () => ({ default: {} }));

const ACTIVITIES: readonly (readonly [Activity, string, string])[] = [
  [
    { kind: "done", action: "insertedRows", ms: 0.26, recalculated: 0 },
    "inserted rows in 0.3 ms",
    "已插入行，用时 0.3 毫秒",
  ],
  [
    { kind: "done", action: "pasted", ms: 1234, recalculated: 100_001 },
    "pasted in 1,234.0 ms, recalculated 100,001 cells",
    "已粘贴，用时 1,234.0 毫秒，重算 100,001 个单元格",
  ],
  [
    { kind: "done", action: "updatedCell", ms: 0, recalculated: 0 },
    "updated cell in 0.0 ms",
    "已更新单元格，用时 0.0 毫秒",
  ],
  [
    { kind: "recalculating", done: 4096, total: 100_001 },
    "recalculating 4,096 / 100,001…",
    "正在重算 4,096 / 100,001…",
  ],
  [
    { kind: "recalculated", cells: 100_001, ms: 12.34 },
    "recalculated 100,001 cells in 12.3 ms",
    "已重算 100,001 个单元格，用时 12.3 毫秒",
  ],
  [{ kind: "replaced", cells: 2 }, "replaced 2 cell(s)", "已替换 2 个单元格"],
];

const STATUSES: readonly EngineStatus[] = [
  "starting",
  "ready",
  "failed",
  "blocked",
  "takingOver",
  "yielded",
  "safeMode",
];

async function inChinese(): Promise<void> {
  const { initLocale } = await import("../i18n/index.ts");
  vi.stubGlobal("document", { documentElement: {} });
  vi.stubGlobal("window", {
    location: { search: "?lang=zh" },
    localStorage: { getItem: () => null },
    navigator: { languages: [] },
  });
  await initLocale("app");
}

async function inEnglish(): Promise<void> {
  const { initLocale } = await import("../i18n/index.ts");
  vi.stubGlobal("window", {
    location: { search: "" },
    localStorage: { getItem: () => null },
    navigator: { languages: ["en-US"] },
  });
  await initLocale("app");
}

beforeEach(() => {
  vi.stubGlobal("document", { documentElement: {} });
});

afterEach(async () => {
  await inEnglish();
  vi.unstubAllGlobals();
});

describe("what the engine has done", () => {
  it("is put into words", async () => {
    expect(ACTIVITIES.map(([activity]) => activityText(activity))).toEqual(
      ACTIVITIES.map(([, english]) => english),
    );
    await inChinese();
    expect(ACTIVITIES.map(([activity]) => activityText(activity))).toEqual(
      ACTIVITIES.map(([, , chinese]) => chinese),
    );
  });
});

const bar = (props: Partial<StatusBarProps>) =>
  renderToStaticMarkup(
    createElement(StatusBar, {
      status: "ready",
      opening: false,
      sheet: null,
      selection: null,
      activity: null,
      stats: null,
      kernelVersion: "1.2.3",
      zoom: null,
      ...props,
    }),
  );

describe("the status bar", () => {
  it("shows the raw status in English and keeps it at hand in Chinese", async () => {
    const statusOf = (status: EngineStatus) =>
      /data-status="(\w+)">([^<]+)</.exec(bar({ status }))?.slice(1);
    expect(STATUSES.map(statusOf)).toEqual(STATUSES.map((status) => [status, status]));
    await inChinese();
    for (const status of STATUSES) {
      const [raw, text] = statusOf(status) ?? [];
      expect(raw).toBe(status);
      expect(text).toMatch(/^\p{Script=Han}+$/u);
    }
  });

  it("says what there is to say about the sheet", () => {
    const sheet = { seed: 7, generationMs: 321.04 } as SheetInfo;
    const stats = { frameWork: { samples: 3, p50: 0.12, p99: 2 } } as GridStats;
    const selection = { name: "B2", text: "x" } as Selection;
    const [activity] = ACTIVITIES[0] ?? [];
    const text = bar({
      sheet,
      stats,
      selection,
      activity: activity ? activityText(activity) : null,
    }).replace(/<[^>]+>/g, "|");
    expect(text.split(/\|+/).filter(Boolean)).toEqual([
      "ready",
      "B2 · x",
      "inserted rows in 0.3 ms",
      "seed 7",
      "generated in 321.0 ms",
      "frame 0.1 ms · p99 2.0 ms",
      "kernel 1.2.3",
    ]);
    expect(bar({ opening: true, status: "starting" })).toContain("Opening local workbook…");
  });
});
