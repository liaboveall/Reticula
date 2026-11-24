// The development server, for what only a development build of React shows: its
// strict mode runs every effect twice.
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer } from "vite";

export interface DevServer {
  readonly origin: string;
  stop(): Promise<void>;
}

export async function startDevServer(): Promise<DevServer> {
  const root = resolve("apps/web");
  // Of its own: the one in the working tree may be in use, by another worker or by whoever is developing.
  const cacheDir = await mkdtemp(join(tmpdir(), "reticula-e2e-vite-"));
  const server = await createServer({
    root,
    configFile: resolve(root, "vite.config.ts"),
    cacheDir,
    // It would complain of every request to the server of the app, which nobody has started.
    logLevel: "silent",
    optimizeDeps: { entries: ["ui.html", "index.html"] },
    server: { host: "127.0.0.1", port: 0, strictPort: false, hmr: false, watch: null },
  });
  await server.listen();
  const address = server.httpServer?.address();
  if (!address || typeof address === "string") {
    throw new Error("the development server listens nowhere");
  }
  return {
    origin: `http://127.0.0.1:${address.port}`,
    async stop() {
      await server.close();
      await rm(cacheDir, { recursive: true, force: true });
    },
  };
}
