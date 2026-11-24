import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { SHOTS, type ShotName, shotUrl } from "./shots.ts";

const PUBLIC = fileURLToPath(new URL("../../public", import.meta.url));
const MEDIA = join(PUBLIC, "media");
/** What the picture on other sites is called, which the page itself does not show. */
const PREVIEW = "og.webp";

/** The size a WebP file says it has, in its extended header. */
function sizeOf(path: string): { width: number; height: number } {
  const file = readFileSync(path);
  expect(file.toString("latin1", 0, 4), path).toBe("RIFF");
  expect(file.toString("latin1", 8, 16), path).toBe("WEBPVP8X");
  return { width: file.readUIntLE(24, 3) + 1, height: file.readUIntLE(27, 3) + 1 };
}

const pictures = (Object.keys(SHOTS) as ShotName[]).flatMap((name) =>
  (["en", "zh"] as const).flatMap((language) =>
    (["light", "dark"] as const).map((theme) => ({ name, url: shotUrl(name, language, theme) })),
  ),
);

describe("the pictures of the product", () => {
  it.each(pictures)("$url is as large as the page says", ({ name, url }) => {
    const { width, height, scale } = SHOTS[name];
    expect(sizeOf(join(PUBLIC, url))).toEqual({ width: width * scale, height: height * scale });
  });

  it("are the only ones there, with the one other sites show", () => {
    const expected = new Set(pictures.map(({ url }) => url.replace("/media/", "")));
    expect(readdirSync(MEDIA).sort()).toEqual([...expected, PREVIEW].sort());
    expect(sizeOf(join(MEDIA, PREVIEW))).toEqual({ width: 1200, height: 630 });
  });

  it("take no more than 600 KB together", () => {
    const sizes = readdirSync(MEDIA).map((file) => statSync(join(MEDIA, file)).size);
    expect(sizes.reduce((sum, size) => sum + size, 0)).toBeLessThanOrEqual(600_000);
  });

  it("are no larger than they have to be in either language", () => {
    // Those without words are the same file.
    expect(shotUrl("sheet", "zh", "dark")).toBe(shotUrl("sheet", "en", "dark"));
    expect(shotUrl("sim", "zh", "dark")).not.toBe(shotUrl("sim", "en", "dark"));
  });
});
