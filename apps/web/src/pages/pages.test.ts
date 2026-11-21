import { describe, expect, it } from "vitest";
import { formatBytes, normalizeAppRoute, pickNearestSnapshotSeq } from "./helpers.ts";

describe("M2b shell helpers", () => {
  it("normalizes routes accurately", () => {
    expect(normalizeAppRoute("/")).toBe("landing");
    expect(normalizeAppRoute("/app")).toBe("app");
    expect(normalizeAppRoute("/join/abc123")).toBe("app");
    expect(normalizeAppRoute("/workbooks")).toBe("workbooks");
    expect(normalizeAppRoute("/account")).toBe("account");
    expect(normalizeAppRoute("/privacy")).toBe("privacy");
    expect(normalizeAppRoute("/sim")).toBe("sim");
  });

  it("picks nearest base snapshot seq <= targetSeq", () => {
    expect(pickNearestSnapshotSeq([], "10")).toBeNull();
    expect(pickNearestSnapshotSeq(["5", "10"], "3")).toBeNull();
    expect(pickNearestSnapshotSeq(["0"], "12")).toBe("0");
    const seqs = ["0", "250", "5000", "10000"];
    expect(pickNearestSnapshotSeq(seqs, "0")).toBe("0");
    expect(pickNearestSnapshotSeq(seqs, "249")).toBe("0");
    expect(pickNearestSnapshotSeq(seqs, "250")).toBe("250");
    expect(pickNearestSnapshotSeq(seqs, "4999")).toBe("250");
    expect(pickNearestSnapshotSeq(seqs, "5000")).toBe("5000");
    expect(pickNearestSnapshotSeq(seqs, "15000")).toBe("10000");
  });

  it("formats storage sizes readably", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(2048)).toBe("2.0 KB");
    expect(formatBytes(268_435_456)).toBe("256.00 MB");
  });
});
