import { t } from "../i18n/index.ts";

/**
 * What the simulator verifies, by its own numbers (`crates/sim/src/world.rs`):
 * the first seven as it runs, the others at the latest once the network is quiet.
 */
export function invariants(): readonly string[] {
  return [
    t("Every replica's visible state equals its confirmed state plus its pending edits, replayed"),
    t("Every acknowledged transaction is in the server log exactly once"),
    t("The server log has no gaps and keeps each replica's transactions in order"),
    t("Every confirmed state has the server's StateDigest at the same sequence number"),
    t("No two replicas ever create the same row, column or sheet ID"),
    t("Invalid and unauthorized frames are rejected and leave the server unchanged"),
    t("Applying remote edits in place gives the same state as replaying from scratch"),
    t("Once the network is quiet, every replica's digests match the server's"),
    t("The full log and the latest snapshot plus its tail both replay to the server's state"),
    t("Every terminally rejected transaction is kept in the local unsynced archive"),
    t("Committed sorts agree on the server, on every replica and after a rebuild from a snapshot"),
  ];
}

export type InvariantStatus = "idle" | "checking" | "verified" | "failed";

export function invariantStatusName(status: InvariantStatus): string {
  switch (status) {
    case "idle":
      return t("Idle");
    case "checking":
      return t("Checking…");
    case "verified":
      return t("Verified");
    case "failed":
      return t("Failed");
  }
}

/** The state of a replica's connection, for display. */
export function connectionName(connection: string): string {
  switch (connection) {
    case "initializing":
      return t("initializing|connection");
    case "offline":
      return t("offline|connection");
    case "handshaking":
      return t("handshaking|connection");
    case "awaitingSnapshot":
      return t("awaitingSnapshot|connection");
    case "online":
      return t("online|connection");
    default:
      return connection;
  }
}

/** A seed as the simulator takes it, or `null` if the text is none. */
export function parseSeed(text: string): bigint | null {
  try {
    const seed = BigInt(text);
    return seed === BigInt.asUintN(64, seed) ? seed : null;
  } catch {
    return null;
  }
}
