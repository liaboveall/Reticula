import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { type RendererHost, startUpgrade } from "./upgrade.ts";

const { attach, stop } = vi.hoisted(() => {
  const stop = vi.fn();
  return { stop, attach: vi.fn(() => stop) };
});
vi.mock("./webgpu/index.ts", () => ({ attach }));

let idle: (() => void)[] = [];

beforeEach(() => {
  idle = [];
  attach.mockClear();
  stop.mockClear();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function browser(navigator: object, window: object = {}) {
  vi.stubGlobal("navigator", navigator);
  vi.stubGlobal("window", {
    requestIdleCallback: (callback: () => void) => idle.push(callback),
    ...window,
  });
}

function fakeHost() {
  const reports: unknown[] = [];
  const host = { report: (reason: unknown) => reports.push(reason) } as unknown as RendererHost;
  return { host, reports };
}

async function runIdle() {
  for (const callback of idle.splice(0)) {
    callback();
  }
  await vi.dynamicImportSettled();
  await Promise.resolve();
}

const CHROMIUM = { gpu: {}, userAgentData: {} };

test("WebGPU is for browsers that have it and whose pictures are known", async () => {
  for (const [navigator, window, preference, reason] of [
    [CHROMIUM, {}, "auto", "starting"],
    [{ gpu: {} }, { mozInnerScreenX: 0 }, "auto", "starting"],
    // Safari.
    [{ gpu: {} }, {}, "auto", "engine"],
    [{ gpu: {} }, {}, "webgpu", "starting"],
    [{ userAgentData: {} }, {}, "auto", "unsupported"],
    [{}, {}, "webgpu", "unsupported"],
    [CHROMIUM, {}, "canvas2d", "preference"],
  ] as const) {
    browser(navigator, window);
    const { host, reports } = fakeHost();
    attach.mockClear();
    startUpgrade(host, preference);
    expect(reports, `${JSON.stringify(navigator)} ${preference}`).toEqual([reason]);
    expect(attach).not.toHaveBeenCalled();
    await runIdle();
    expect(attach).toHaveBeenCalledTimes(reason === "starting" ? 1 : 0);
  }
});

test("only whoever asks for it gets a software adapter", async () => {
  browser(CHROMIUM);
  const { host } = fakeHost();
  startUpgrade(host, "auto");
  startUpgrade(host, "webgpu");
  await runIdle();
  expect(attach.mock.calls).toEqual([
    [host, false],
    [host, true],
  ]);
});

test("it can be called off at any time", async () => {
  browser(CHROMIUM);
  const early = startUpgrade(fakeHost().host, "auto");
  early();
  await runIdle();
  expect(attach).not.toHaveBeenCalled();

  const late = startUpgrade(fakeHost().host, "auto");
  await runIdle();
  expect(stop).not.toHaveBeenCalled();
  late();
  expect(stop).toHaveBeenCalledTimes(1);
});

test("what fails after it was called off is not reported", async () => {
  browser(CHROMIUM);
  const { host, reports } = fakeHost();
  let call = () => {};
  attach.mockImplementationOnce(() => {
    // While the code was on its way: what has been chosen since has its own reason.
    call();
    throw new Error("a part of the app that is gone");
  });
  call = startUpgrade(host, "auto");
  await runIdle();
  expect(reports).toEqual(["starting"]);
});

test("what cannot be loaded leaves the grid as it is", async () => {
  browser(CHROMIUM);
  attach.mockImplementationOnce(() => {
    throw new Error("a part of the app that is gone");
  });
  const { host, reports } = fakeHost();
  startUpgrade(host, "auto")();
  startUpgrade(host, "auto");
  await runIdle();
  expect(reports).toEqual(["starting", "starting", "failed"]);
});
