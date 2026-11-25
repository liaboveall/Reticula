// A static server for the table measurements: cross-origin isolated, so that
// the clock is fine and the memory can be measured.

import { createReadStream, statSync } from "node:fs";
import { createServer } from "node:http";
import { extname, join, normalize } from "node:path";

const TYPES = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm", ".json": "application/json" };

/** Serves each prefix of `mounts` from its directory. */
export function serve(mounts, port = 0) {
  const log = [];
  const server = createServer((request, response) => {
    const path = normalize(decodeURIComponent(new URL(request.url, "http://x").pathname)).replaceAll("\\", "/");
    const mount = Object.keys(mounts).filter((prefix) => path.startsWith(prefix)).sort((a, b) => b.length - a.length)[0];
    const file = mount ? (typeof mounts[mount] === "function" ? mounts[mount](path) : join(mounts[mount], path.slice(mount.length))) : null;
    let size;
    try {
      const stat = statSync(file);
      if (!stat.isFile()) throw new Error();
      size = stat.size;
    } catch {
      log.push({ path, status: 404 });
      response.writeHead(404).end();
      return;
    }
    const headers = {
      "content-type": TYPES[extname(file)] ?? "application/octet-stream",
      "cross-origin-opener-policy": "same-origin",
      "cross-origin-embedder-policy": "require-corp",
      "cross-origin-resource-policy": "same-origin",
      "accept-ranges": "bytes",
      "cache-control": "no-store",
    };
    const range = /^bytes=(\d+)-(\d*)$/.exec(request.headers.range ?? "");
    const start = range ? Number(range[1]) : 0;
    const end = range && range[2] ? Math.min(Number(range[2]), size - 1) : size - 1;
    log.push({ path, status: range ? 206 : 200, method: request.method, bytes: request.method === "HEAD" ? 0 : end - start + 1 });
    response.writeHead(range ? 206 : 200, { ...headers, "content-length": end - start + 1, ...(range ? { "content-range": `bytes ${start}-${end}/${size}` } : {}) });
    if (request.method === "HEAD") {
      response.end();
      return;
    }
    createReadStream(file, { start, end }).pipe(response);
  });
  return new Promise((resolve) => server.listen(port, "127.0.0.1", () => resolve({ address: `http://127.0.0.1:${server.address().port}`, log, close: () => server.close() })));
}
