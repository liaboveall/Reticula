import { createElement, Fragment } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

type I18n = typeof import("./index.ts") & typeof import("./react.ts");

const CORE = {
  "Insert row above": "在上方插入行",
  "Open|verb": "打开",
  "{n} cells": "{n} 个单元格",
  "{a} of {b}": "{b} 中的 {a}",
  "Press <0>Enter</0> for {what}": "按 <0>Enter</0> 以{what}",
};

/** What the modules with the texts do when they are loaded, until told otherwise. */
let core: () => Promise<object> | object;
let functions: () => Promise<object> | object;

interface Opened {
  readonly i18n: I18n;
  readonly html: { lang: string };
  readonly stored: Map<string, string>;
  readonly replaceState: ReturnType<typeof vi.fn>;
  /** Counts how often the page is rendered again. */
  readonly renders: () => number;
}

function Page({ useLocale }: Pick<I18n, "useLocale">) {
  return useLocale();
}

async function open(
  href: string,
  languages: readonly string[],
  stored = new Map<string, string>(),
) {
  const html = { lang: "en" };
  const { search } = new URL(href);
  const replaceState = vi.fn();
  vi.stubGlobal("document", { documentElement: html });
  vi.stubGlobal("window", {
    location: { search, href },
    localStorage: {
      getItem: (key: string) => stored.get(key) ?? null,
      setItem: (key: string, value: string) => stored.set(key, value),
    },
    navigator: { languages },
    history: { state: "state", replaceState },
  });
  let renders = 0;
  const react = await vi.importActual<typeof import("react")>("react");
  vi.doMock("react", () => ({
    ...react,
    useSyncExternalStore: (subscribe: (listener: () => void) => void, snapshot: () => number) => {
      let last = snapshot();
      subscribe(() => {
        if (snapshot() !== last) {
          last = snapshot();
          renders++;
        }
      });
      return last;
    },
  }));
  const i18n: I18n = { ...(await import("./index.ts")), ...(await import("./react.ts")) };
  renderToStaticMarkup(createElement(Page, { useLocale: i18n.useLocale }));
  const opened: Opened = { i18n, html, stored, replaceState, renders: () => renders };
  return opened;
}

const ENGLISH = "http://localhost/app";
const CHINESE = "http://localhost/app?lang=zh";

beforeEach(() => {
  vi.resetModules();
  vi.useFakeTimers();
  core = () => CORE;
  functions = () => ({});
  vi.doMock("./zh-core.ts", async () => ({ default: await core() }));
  vi.doMock("./zh-engine.ts", () => ({ default: {} }));
  vi.doMock("./zh-functions.ts", async () => ({ default: await functions() }));
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("in English", () => {
  it("shows the keys and loads nothing", async () => {
    core = () => {
      throw new Error("must not be loaded");
    };
    const { i18n, html } = await open(ENGLISH, ["en-US"]);
    await i18n.initLocale("app");
    expect(i18n.getLocale()).toBe("en");
    expect(html.lang).toBe("en");
    expect(i18n.t("Insert row above")).toBe("Insert row above");
    expect(i18n.t("Open|verb")).toBe("Open");
    expect(i18n.t("a | b")).toBe("a | b");
  });

  it("fills in values, without looking at them", async () => {
    const { i18n } = await open(ENGLISH, ["en-US"]);
    expect(i18n.t("{a} of {b}", { a: "{b}", b: 1 })).toBe("{b} of 1");
    expect(i18n.t("{a} of {b}", { b: 0 })).toBe("{a} of 0");
  });

  it("has a singular", async () => {
    const { i18n } = await open(ENGLISH, ["en-US"]);
    const cells = (n: number) => i18n.tn(n, "{n} cell", "{n} cells");
    expect([0, 1, 2, 1000].map(cells)).toEqual(["0 cells", "1 cell", "2 cells", "1,000 cells"]);
    expect(i18n.tn(1, "{n} cell in {sheet}", "{n} cells in {sheet}", { sheet: "A" })).toBe(
      "1 cell in A",
    );
  });

  it("formats as it always has", async () => {
    const { i18n } = await open(ENGLISH, ["en-US"]);
    expect(i18n.fmtInt(1_000_001)).toBe("1,000,001");
    expect(i18n.fmtNumber(1234.5, 2)).toBe("1,234.50");
    expect(i18n.fmtMs(0.04)).toBe("0.0 ms");
    expect(i18n.fmtMs(1234.56)).toBe("1,234.6 ms");
    const sizes = [-1, 0, 1, 1023, 1024, 1536, 1048575, 1048576, 123456789, 5e9, 3e12, Number.NaN];
    expect(sizes.map(i18n.fmtBytes)).toEqual([
      ...["0 B", "0 B", "1 B", "1023 B", "1.0 KB", "1.5 KB", "1024.0 KB", "1.00 MB"],
      ...["117.74 MB", "4.66 GB", "2793.97 GB", "0 B"],
    ]);
    const date = new Date(2026, 10, 24, 15, 4, 5);
    expect(i18n.fmtDateTime(date)).toBe(date.toLocaleString("en-US"));
    expect(i18n.fmtDateTime(date.getTime(), { year: "numeric" })).toBe("2026");
  });
});

describe("in Chinese", () => {
  it("shows the translations, and English where there is none", async () => {
    const { i18n, html } = await open(ENGLISH, ["zh-CN", "en"]);
    await i18n.initLocale("app");
    expect(i18n.getLocale()).toBe("zh-CN");
    expect(html.lang).toBe("zh-CN");
    expect(i18n.t("Insert row above")).toBe("在上方插入行");
    expect(i18n.t("Open|verb")).toBe("打开");
    expect(i18n.t("{a} of {b}", { a: "x", b: "{a}" })).toBe("{a} 中的 x");
    expect(i18n.tn(1, "{n} cell", "{n} cells")).toBe("1 个单元格");
    expect(i18n.t("Close|verb")).toBe("Close");
  });

  it("formats numbers and dates", async () => {
    const { i18n } = await open(CHINESE, []);
    await i18n.initLocale("app");
    expect(i18n.fmtInt(1_000_001)).toBe("1,000,001");
    expect(i18n.fmtMs(1.25)).toBe("1.3 毫秒");
    expect(i18n.fmtBytes(1536)).toBe("1.5 KB");
    expect(i18n.fmtDateTime(new Date(2026, 10, 24, 15, 4, 5))).toBe("2026/11/24 15:04:05");
  });

  it("puts elements where the translation has them", async () => {
    const { i18n } = await open(CHINESE, []);
    const line = () =>
      renderToStaticMarkup(
        createElement(
          Fragment,
          null,
          i18n.tx("Press <0>Enter</0> for {what}", { 0: createElement("kbd") }, { what: "<1>" }),
          i18n.tx("<0/>, then <1/>", {
            0: createElement("i", null, "x"),
            1: createElement("br"),
          }),
        ),
      );
    expect(line()).toBe("Press <kbd>Enter</kbd> for &lt;1&gt;<i>x</i>, then <br/>");
    await i18n.initLocale("app");
    expect(line()).toBe("按 <kbd>Enter</kbd> 以&lt;1&gt;<i>x</i>, then <br/>");
  });

  it("starts in English when the texts take too long, and changes over", async () => {
    let arrive = (_texts: object) => {};
    core = () => new Promise<object>((resolve) => (arrive = resolve));
    const { i18n, html, renders } = await open(CHINESE, []);
    let started = false;
    const start = i18n.initLocale("app").then(() => {
      started = true;
    });
    await vi.advanceTimersByTimeAsync(1_499);
    expect(started).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    await start;
    expect(i18n.getLocale()).toBe("en");
    expect(i18n.t("Insert row above")).toBe("Insert row above");

    arrive(CORE);
    await vi.waitFor(() => expect(renders()).toBe(1));
    expect(i18n.t("Insert row above")).toBe("在上方插入行");
    expect(html.lang).toBe("zh-CN");
  });

  it("starts in English at once when the texts cannot be loaded, and keeps trying", async () => {
    let attempts = 0;
    core = () => {
      attempts++;
      throw new Error("offline");
    };
    const { i18n, renders } = await open(CHINESE, []);
    await i18n.initLocale("app");
    expect(attempts).toBe(1);
    expect(i18n.getLocale()).toBe("en");
    await vi.advanceTimersByTimeAsync(2_000);
    await vi.waitFor(() => expect(attempts).toBe(2));
    expect(renders()).toBe(0);

    core = () => CORE;
    await vi.advanceTimersByTimeAsync(4_000);
    await vi.waitFor(() => expect(i18n.getLocale()).toBe("zh-CN"));
    expect(renders()).toBe(1);
  });

  it("does not wait for texts that may come later", async () => {
    let arrive = (_texts: object) => {};
    functions = () => new Promise<object>((resolve) => (arrive = resolve));
    const { i18n, renders } = await open(CHINESE, []);
    await i18n.initLocale("app");
    expect(i18n.t("Insert row above")).toBe("在上方插入行");
    const before = renders();
    await vi.waitFor(() => {
      arrive({ SUM: "求和" });
      expect(renders()).toBe(before + 1);
    });
    expect(i18n.catalogueOf("zh-functions")).toEqual({ SUM: "求和" });
    expect(i18n.t("SUM")).toBe("SUM");
    await i18n.setLocale("en");
    expect(i18n.catalogueOf("zh-functions")).toBeUndefined();
  });
});

describe("the choice", () => {
  it("changes the language of the page and is kept", async () => {
    const { i18n, html, stored, replaceState, renders } = await open(ENGLISH, ["en-US"]);
    await i18n.initLocale("app");
    await i18n.setLocale("zh-CN");
    expect(stored.get("reticula:locale")).toBe("zh-CN");
    expect(i18n.t("Insert row above")).toBe("在上方插入行");
    expect(html.lang).toBe("zh-CN");
    const inChinese = renders();
    expect(inChinese).toBeGreaterThan(0);
    await i18n.setLocale("en");
    expect(stored.get("reticula:locale")).toBe("en");
    expect(i18n.t("Insert row above")).toBe("Insert row above");
    expect(html.lang).toBe("en");
    expect(renders()).toBe(inChinese + 1);
    expect(replaceState).not.toHaveBeenCalled();
  });

  it("replaces what the address asks for", async () => {
    const { i18n, replaceState } = await open("http://localhost/app?doc=a&lang=en", ["zh"]);
    await i18n.initLocale("app");
    expect(i18n.getLocale()).toBe("en");
    await i18n.setLocale("zh-CN");
    expect(String(replaceState.mock.calls[0]?.[2])).toBe("http://localhost/app?doc=a&lang=zh");
  });

  it("stops what another choice has begun", async () => {
    core = () => {
      throw new Error("offline");
    };
    const { i18n, renders } = await open(CHINESE, []);
    await i18n.initLocale("app");
    await i18n.setLocale("en");
    core = () => CORE;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(i18n.getLocale()).toBe("en");
    expect(renders()).toBe(0);
  });

  it("renders nothing again on the way to another page", async () => {
    const { i18n, renders } = await open(ENGLISH, ["en-US"]);
    await i18n.initLocale("landing");
    await i18n.enterRoute("app");
    expect(renders()).toBe(0);
  });
});
