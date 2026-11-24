import { ApiError } from "@reticula/protocol/collab-api";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { tx } from "../i18n/react.ts";
import { normalizeAppRoute } from "../routes.ts";
import { apiErrorMessage, pickNearestSnapshotSeq } from "./helpers.ts";
import { LanguageSwitch } from "./LanguageSwitch.tsx";
import { roleName, visibilityName } from "./labels.ts";
import { PageLink, PageNav } from "./PageNav.tsx";

describe("M2b shell helpers", () => {
  it("normalizes routes accurately", () => {
    expect(normalizeAppRoute("/")).toBe("landing");
    expect(normalizeAppRoute("/app")).toBe("app");
    expect(normalizeAppRoute("/join/abc123")).toBe("app");
    expect(normalizeAppRoute("/workbooks")).toBe("workbooks");
    expect(normalizeAppRoute("/account")).toBe("account");
    expect(normalizeAppRoute("/privacy")).toBe("privacy");
    expect(normalizeAppRoute("/sim")).toBe("sim");
    expect(normalizeAppRoute("/benchmarks")).toBe("benchmarks");
    expect(normalizeAppRoute("/benchmarks/")).toBe("benchmarks");
  });

  it("picks nearest base snapshot seq <= targetSeq", () => {
    expect(pickNearestSnapshotSeq([], "10")).toBeNull();
    expect(pickNearestSnapshotSeq(["5", "10"], "3")).toBeNull();
    expect(pickNearestSnapshotSeq(["0"], "12")).toBe("0");
    const seqs = ["0", "250", "5000", "10000"];
    expect(pickNearestSnapshotSeq(seqs, "0")).toBe("0");
    expect(pickNearestSnapshotSeq(seqs, "249")).toBe("0");
    expect(pickNearestSnapshotSeq(seqs, "250")).toBe("250");
    expect(pickNearestSnapshotSeq(seqs, "4999")).toBe("250");
    expect(pickNearestSnapshotSeq(seqs, "5000")).toBe("5000");
    expect(pickNearestSnapshotSeq(seqs, "15000")).toBe("10000");
  });

  it("says what went wrong with a request", () => {
    expect(apiErrorMessage(new ApiError(401), "Could not open")).toBe(
      "Sign in required to perform this action.",
    );
    expect(apiErrorMessage(new ApiError(500), "Could not open")).toBe("Could not open (HTTP 500)");
    expect(apiErrorMessage(new Error("the network is down"), "Could not open")).toBe(
      "the network is down",
    );
    expect(apiErrorMessage("?", "Could not open")).toBe("Could not open");
  });

  it("does not put the browser's English between Chinese texts", async () => {
    const offline = new TypeError("Failed to fetch");
    expect(apiErrorMessage(offline, "Could not open")).toBe("Failed to fetch");
    vi.resetModules();
    vi.doMock("../i18n/index.ts", () => ({ getLocale: () => "zh-CN" }));
    const { errorText } = await import("./errors.ts");
    vi.doUnmock("../i18n/index.ts");
    vi.resetModules();
    expect(errorText(offline, "无法打开")).toBe("无法打开");
    expect(errorText(new DOMException("The request is not allowed"), "无法打开")).toBe("无法打开");
    expect(errorText(new Error("what the engine says"), "无法打开")).toBe("what the engine says");
  });

  it("has names for what the server calls by a word", () => {
    expect(["owner", "editor", "viewer", "revoked", "admin"].map(roleName)).toEqual([
      "owner",
      "editor",
      "viewer",
      "revoked",
      "admin",
    ]);
    expect(["private", "link_view", "public"].map(visibilityName)).toEqual([
      "private",
      "link_view",
      "public",
    ]);
  });

  it("links the pages, also from within a text", () => {
    const onNavigate = () => {};
    const links = [["/app", "Editor"]] as const;
    expect(renderToStaticMarkup(createElement(PageNav, { onNavigate, links }))).toBe(
      '<nav class="page-nav"><a href="/" class="page-brand">Reticula</a>' +
        '<div class="page-nav-links"><a href="/app" class="toolbar-btn">Editor</a></div></nav>',
    );
    const link = createElement(PageLink, { to: "/account", onNavigate });
    const text = tx(
      "You can permanently erase your profile, login session, and all owned cloud workbooks at any time from the <0>Account page</0>. Because this is a personal project with no commercial SLA guarantee, export important workbooks regularly as <1>.rtcl</1> snapshots.",
      { 0: link, 1: createElement("code") },
    );
    expect(renderToStaticMarkup(createElement("p", null, ...text))).toContain(
      'from the <a href="/account">Account page</a>. Because',
    );
  });

  it("offers the languages by their own names", () => {
    expect(renderToStaticMarkup(createElement(LanguageSwitch))).toBe(
      '<fieldset class="page-language" data-testid="language-switch"><legend>Language</legend>' +
        '<button type="button" lang="en" class="page-tab active" aria-pressed="true">English</button>' +
        '<button type="button" lang="zh-CN" class="page-tab" aria-pressed="false">简体中文</button></fieldset>',
    );
  });
});
