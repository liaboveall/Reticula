import { describe, expect, it } from "vitest";
import { isAssetResponse } from "./sw-assets.ts";

describe("isAssetResponse", () => {
  it("accepts what the server and the preview server send", () => {
    expect(isAssetResponse("/assets/index-a1.js", "text/javascript")).toBe(true);
    expect(isAssetResponse("/assets/index-a1.js", "text/javascript; charset=utf-8")).toBe(true);
    expect(isAssetResponse("/assets/index-a1.js", "application/javascript")).toBe(true);
    expect(isAssetResponse("/assets/index-a1.css", "text/css; charset=utf-8")).toBe(true);
    expect(isAssetResponse("/assets/engine_bg-a1.wasm", "application/wasm")).toBe(true);
    expect(isAssetResponse("/assets/shot-a1.webp", "image/webp")).toBe(true);
  });

  it("rejects the document in place of a file", () => {
    for (const name of ["index-a1.js", "index-a1.css", "engine_bg-a1.wasm", "font-a1.ttf"]) {
      expect(isAssetResponse(`/assets/${name}`, "text/html; charset=utf-8")).toBe(false);
    }
  });

  it("rejects a file of another kind, and one of no kind", () => {
    expect(isAssetResponse("/assets/index-a1.js", "text/css")).toBe(false);
    expect(isAssetResponse("/assets/engine_bg-a1.wasm", "application/octet-stream")).toBe(false);
    expect(isAssetResponse("/assets/index-a1.js", null)).toBe(false);
  });

  it("takes anything but a document for a kind it does not know", () => {
    expect(isAssetResponse("/assets/font-a1.ttf", "font/ttf")).toBe(true);
    expect(isAssetResponse("/assets/no-extension", "application/octet-stream")).toBe(true);
    expect(isAssetResponse("/assets/x.constructor", "text/html")).toBe(false);
  });
});
