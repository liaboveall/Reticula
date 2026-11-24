import type { AddressInfo, Server } from "node:net";

/**
 * The ports above 1023 that browsers refuse to load anything from
 * (https://fetch.spec.whatwg.org/#port-blocking). Where the system hands out
 * ports from 1024 on, they are among the ones it picks.
 */
const BLOCKED_PORTS: ReadonlySet<number> = new Set([
  ...[1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566],
  ...[6665, 6666, 6667, 6668, 6669, 6679, 6697, 10080],
]);

/** Has a server listen on a port of this machine that a browser can load from. */
export async function listen(server: Server): Promise<number> {
  for (;;) {
    await new Promise<void>((ready, failed) => {
      server.once("error", failed);
      server.listen(0, "127.0.0.1", () => {
        server.off("error", failed);
        ready();
      });
    });
    const { port } = server.address() as AddressInfo;
    if (!BLOCKED_PORTS.has(port)) {
      return port;
    }
    await new Promise<void>((closed) => server.close(() => closed()));
  }
}
