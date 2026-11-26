import { beforeEach, describe, expect, test, vi } from "vitest";

const init = vi.fn<(options: unknown) => Promise<unknown>>();

vi.mock("@reticula/protocol/xlsx", () => ({
  init: (options: unknown) => init(options),
  importXlsx: vi.fn(),
  exportXlsxSnapshot: vi.fn(),
  patchXlsxSnapshot: vi.fn(),
}));
vi.mock("@reticula/protocol/xlsx-wasm?url", () => ({ default: "/xlsx.wasm" }));

beforeEach(() => {
  vi.resetModules();
  init.mockReset();
});

describe("the module for Excel files", () => {
  test("is loaded again after a load that failed", async () => {
    const { ensureXlsxReady } = await import("./xlsxBridge.ts");
    init.mockRejectedValueOnce(new TypeError("Failed to fetch"));
    await expect(ensureXlsxReady()).rejects.toThrow("Failed to fetch");
    init.mockResolvedValueOnce({});
    await expect(ensureXlsxReady()).resolves.toBeUndefined();
    expect(init).toHaveBeenCalledTimes(2);
  });

  test("is loaded once when it loads", async () => {
    const { ensureXlsxReady } = await import("./xlsxBridge.ts");
    init.mockResolvedValue({});
    await Promise.all([ensureXlsxReady(), ensureXlsxReady()]);
    await ensureXlsxReady();
    expect(init).toHaveBeenCalledTimes(1);
  });

  test("is not loaded twice by those who wait for a load that fails", async () => {
    const { ensureXlsxReady } = await import("./xlsxBridge.ts");
    init.mockRejectedValueOnce(new TypeError("Failed to fetch"));
    const both = await Promise.allSettled([ensureXlsxReady(), ensureXlsxReady()]);
    expect(both.map(({ status }) => status)).toEqual(["rejected", "rejected"]);
    expect(init).toHaveBeenCalledTimes(1);
  });
});
