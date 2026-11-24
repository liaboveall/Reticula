import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

interface Link {
  rel: string;
  href: string;
  crossOrigin: string | null;
  onload: () => void;
  onerror: () => void;
  remove: () => void;
}

let links: Link[];
let reported: unknown[];
let prevent: boolean;
let loadChunk: typeof import("./load-chunk.ts").loadChunk;

/** The links for an address that are in the document. */
const linked = (href: string) => links.filter((link) => link.href === href);

beforeEach(async () => {
  links = [];
  reported = [];
  prevent = false;
  vi.stubGlobal("document", {
    createElement: () => {
      const link = {
        remove: () => {
          links = links.filter((one) => one !== link);
        },
      };
      return link;
    },
    head: { append: (link: Link) => links.push(link) },
  });
  vi.stubGlobal("window", {
    dispatchEvent: (event: Event & { payload: unknown }) => {
      expect(event.type).toBe("vite:preloadError");
      reported.push(event.payload);
      if (prevent) {
        event.preventDefault();
      }
    },
  });
  vi.resetModules();
  ({ loadChunk } = await import("./load-chunk.ts"));
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("loadChunk", () => {
  it("loads a chunk without style sheets at once", async () => {
    expect(await loadChunk(async () => "chunk")).toBe("chunk");
    expect(links).toEqual([]);
  });

  it("has the style sheets before the chunk", async () => {
    const load = vi.fn(async () => "chunk");
    const loading = loadChunk(load, ["assets/a.css", "assets/b.css"]);
    expect(links).toMatchObject([
      { rel: "stylesheet", crossOrigin: "", href: "/assets/a.css" },
      { rel: "stylesheet", crossOrigin: "", href: "/assets/b.css" },
    ]);
    linked("/assets/a.css")[0]?.onload();
    await Promise.resolve();
    expect(load).not.toHaveBeenCalled();
    linked("/assets/b.css")[0]?.onload();
    expect(await loading).toBe("chunk");
  });

  it("asks for a style sheet once", async () => {
    const first = loadChunk(async () => 1, ["assets/a.css"]);
    const second = loadChunk(async () => 2, ["assets/a.css"]);
    expect(links).toHaveLength(1);
    links[0]?.onload();
    expect(await Promise.all([first, second])).toEqual([1, 2]);
    expect(await loadChunk(async () => 3, ["assets/a.css"])).toBe(3);
    expect(links).toHaveLength(1);
  });

  it("reports a style sheet that cannot be loaded, and asks for it again", async () => {
    const load = vi.fn(async () => "chunk");
    const loading = loadChunk(load, ["assets/a.css"]);
    linked("/assets/a.css")[0]?.onerror();
    await expect(loading).rejects.toThrow("Unable to preload CSS for /assets/a.css");
    expect(reported).toHaveLength(1);
    expect(load).not.toHaveBeenCalled();
    expect(links).toEqual([]);
    const again = loadChunk(load, ["assets/a.css"]);
    linked("/assets/a.css")[0]?.onload();
    expect(await again).toBe("chunk");
  });

  it("reports a chunk that cannot be loaded", async () => {
    const error = new TypeError("Failed to fetch dynamically imported module");
    await expect(loadChunk(() => Promise.reject(error))).rejects.toBe(error);
    expect(reported).toEqual([error]);
  });

  it("goes on when whoever listens says so", async () => {
    prevent = true;
    const loading = loadChunk(async () => "chunk", ["assets/a.css"]);
    linked("/assets/a.css")[0]?.onerror();
    expect(await loading).toBe("chunk");
    expect(await loadChunk(() => Promise.reject(new Error("gone")))).toBeUndefined();
    expect(reported).toHaveLength(2);
  });
});
