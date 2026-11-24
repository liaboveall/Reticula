/** What a file under `/assets/` is served as, by its extension. */
const TYPES: Readonly<Record<string, readonly string[]>> = {
  js: ["text/javascript", "application/javascript"],
  css: ["text/css"],
  wasm: ["application/wasm"],
  json: ["application/json"],
  svg: ["image/svg+xml"],
  png: ["image/png"],
  webp: ["image/webp"],
  woff2: ["font/woff2"],
};

/**
 * Whether a response is the asset its address names. A server without the file
 * may answer with the document every route shares, and a page that finds that
 * in its cache in place of a script stays broken until the cache is replaced.
 */
export function isAssetResponse(pathname: string, contentType: string | null): boolean {
  const type = (contentType ?? "").split(";")[0]?.trim().toLowerCase() ?? "";
  const extension = /\.([a-z0-9]+)$/i.exec(pathname)?.[1]?.toLowerCase() ?? "";
  const expected = Object.hasOwn(TYPES, extension) ? TYPES[extension] : undefined;
  return expected ? expected.includes(type) : type !== "text/html";
}
