import { describe, expect, it, vi } from "vitest";
import { LoadRecovery } from "./routes-recovery.ts";

/** A tab whose session storage outlives its page loads. */
function tab() {
  const stored = new Map<string, string>();
  const storage = {
    getItem: (key: string) => stored.get(key) ?? null,
    setItem: (key: string, value: string) => {
      stored.set(key, value);
    },
  };
  const load = vi.fn<(target: string | null) => void>();
  let online = true;
  let leaving = false;
  return {
    load,
    leave: (value: boolean) => {
      leaving = value;
    },
    goOffline: () => {
      online = false;
    },
    open: (build: string) =>
      new LoadRecovery({ build, storage, isOnline: () => online, isLeaving: () => leaving, load }),
  };
}

describe("LoadRecovery", () => {
  it("loads the address again the first time, and reports the second", () => {
    const { load, open } = tab();
    const first = open("/assets/index-a.js");
    first.recover();
    expect(load).toHaveBeenCalledExactlyOnceWith(null);
    expect(first.state()).toBe("reloading");

    const second = open("/assets/index-a.js");
    second.recover();
    expect(load).toHaveBeenCalledTimes(1);
    expect(second.state()).toBe("stale");
  });

  it("goes to the page that was asked for", () => {
    const { load, open } = tab();
    open("/assets/index-a.js").recover("/app?hooks");
    expect(load).toHaveBeenCalledExactlyOnceWith("/app?hooks");
  });

  it("decides once, whoever asks", () => {
    const { load, open } = tab();
    const recovery = open("/assets/index-a.js");
    recovery.recover("/app");
    recovery.recover();
    recovery.clear();
    expect(load).toHaveBeenCalledTimes(1);
    expect(recovery.state()).toBe("reloading");
  });

  it("allows every build one reload, also when builds alternate", () => {
    const { load, open } = tab();
    for (const build of ["a", "b", "a", "b"]) {
      open(build).recover();
    }
    expect(load).toHaveBeenCalledTimes(2);
  });

  it("has something to say again after the browser brought the page back", () => {
    const { load, open } = tab();
    const recovery = open("a");
    recovery.recover("/app");
    recovery.restored();
    expect(recovery.state()).toBe("ok");
    recovery.recover("/app");
    expect(recovery.state()).toBe("stale");
    expect(load).toHaveBeenCalledTimes(1);
  });

  it("does not leave the page while there is no network", () => {
    const { load, open, goOffline } = tab();
    const recovery = open("a");
    goOffline();
    recovery.recover();
    expect(recovery.state()).toBe("offline");
    expect(load).not.toHaveBeenCalled();
  });

  it("does not get in the way of leaving the page", () => {
    const { load, leave, open } = tab();
    const recovery = open("a");
    leave(true);
    recovery.recover("/app");
    expect(recovery.state()).toBe("ok");
    expect(load).not.toHaveBeenCalled();
    leave(false);
    recovery.recover("/app");
    expect(load).toHaveBeenCalledExactlyOnceWith("/app");
  });

  it("does not reload when it could not remember that it did", () => {
    const load = vi.fn<(target: string | null) => void>();
    const host = { build: "a", isOnline: () => true, isLeaving: () => false, load };
    const refusing = {
      getItem: () => null,
      setItem: () => {
        throw new Error("denied");
      },
    };
    for (const storage of [null, refusing]) {
      const recovery = new LoadRecovery({ ...host, storage });
      recovery.recover();
      expect(recovery.state()).toBe("stale");
    }
    expect(load).not.toHaveBeenCalled();
  });

  it("tells failed loads from other errors", () => {
    const recovery = tab().open("a");
    const failure = new TypeError("Failed to fetch dynamically imported module");
    recovery.noteFailure(failure);
    recovery.noteFailure("not an object");
    expect(recovery.isLoadFailure(failure)).toBe(true);
    expect(recovery.isLoadFailure(new TypeError(failure.message))).toBe(false);
    expect(recovery.isLoadFailure("not an object")).toBe(false);
  });

  it("tells its listeners about changes, and only about those", () => {
    const { open, goOffline } = tab();
    goOffline();
    const recovery = open("a");
    const listener = vi.fn();
    const unsubscribe = recovery.subscribe(listener);
    recovery.recover();
    recovery.recover();
    expect(listener).toHaveBeenCalledTimes(1);
    recovery.clear();
    expect(recovery.state()).toBe("ok");
    expect(listener).toHaveBeenCalledTimes(2);
    unsubscribe();
    recovery.recover();
    expect(listener).toHaveBeenCalledTimes(2);
  });
});
