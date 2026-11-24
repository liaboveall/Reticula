import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ANNOUNCE_DELAY_MS, Announcer } from "./announcer.ts";

function region() {
  const history: string[] = [];
  const node = {
    get textContent(): string | null {
      return history.at(-1) ?? "";
    },
    set textContent(text: string | null) {
      history.push(text ?? "");
    },
  };
  return { polite: node, assertive: { textContent: "" as string | null }, history };
}

function dialog() {
  const state = { open: true };
  return { ...region(), isOpen: () => state.open, state };
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

const settle = () => vi.advanceTimersByTime(ANNOUNCE_DELAY_MS);

describe("announcements", () => {
  it("are read once they have waited", () => {
    const announcer = new Announcer();
    const page = region();
    announcer.registerPage(page);
    announcer.announce("B2");
    vi.advanceTimersByTime(ANNOUNCE_DELAY_MS - 1);
    expect(page.polite.textContent).toBe("");
    vi.advanceTimersByTime(1);
    expect(page.polite.textContent).toBe("B2");
  });

  it("follow one another no faster than that, the latest one at a time", () => {
    const announcer = new Announcer();
    const page = region();
    announcer.registerPage(page);
    // A key that is held down, every 50 ms.
    for (let row = 0; row < 8; row++) {
      announcer.announce(`B${row}`);
      vi.advanceTimersByTime(50);
    }
    settle();
    expect(page.history.filter((text) => text !== "")).toEqual(["B2", "B5", "B7"]);
  });

  it("are read once when they are made twice in a row", () => {
    const announcer = new Announcer();
    const page = region();
    announcer.registerPage(page);
    announcer.announce("Copied");
    announcer.announce("Copied");
    settle();
    settle();
    expect(page.history).toEqual(["", "Copied"]);
  });

  it("are read again when they are made again", () => {
    const announcer = new Announcer();
    const page = region();
    announcer.registerPage(page);
    announcer.announce("No matching cell found.");
    settle();
    announcer.announce("No matching cell found.");
    // Gone for a moment, which is what makes it a change.
    expect(page.polite.textContent).toBe("");
    settle();
    expect(page.history).toEqual(["", "No matching cell found.", "", "No matching cell found."]);
  });

  it("of either politeness leave the other alone", () => {
    const announcer = new Announcer();
    const page = region();
    announcer.registerPage(page);
    announcer.announce("Saved");
    announcer.announce("The engine has stopped", { politeness: "assertive" });
    settle();
    expect(page.polite.textContent).toBe("Saved");
    expect(page.assertive.textContent).toBe("The engine has stopped");
  });

  it("without a text are none", () => {
    const announcer = new Announcer();
    const page = region();
    announcer.registerPage(page);
    announcer.announce("");
    settle();
    expect(page.history).toEqual([]);
  });

  it("are made before there is anywhere to put them, without an error", () => {
    const announcer = new Announcer();
    announcer.announce("Early");
    expect(settle).not.toThrow();
  });
});

describe("the region of an announcement", () => {
  it("is the topmost open dialog's, not the page's, which is inert", () => {
    const announcer = new Announcer();
    const page = region();
    const lower = dialog();
    const upper = dialog();
    announcer.registerDialog(lower);
    announcer.registerDialog(upper);
    // The page registers late; it is under the dialogs all the same.
    announcer.registerPage(page);
    announcer.announce("Replaced 2 cell(s).");
    settle();
    expect(upper.polite.textContent).toBe("Replaced 2 cell(s).");
    expect([...lower.history, ...page.history]).toEqual([]);
  });

  it("is that of the dialog below when the topmost one is closed", () => {
    const announcer = new Announcer();
    const lower = dialog();
    const upper = dialog();
    announcer.registerDialog(lower);
    announcer.registerDialog(upper);
    upper.state.open = false;
    announcer.announce("First");
    settle();
    expect(lower.polite.textContent).toBe("First");
  });

  it("is chosen when the text is read, as the dialog may be closed by then", () => {
    const announcer = new Announcer();
    const page = region();
    announcer.registerPage(page);
    const upper = dialog();
    const close = announcer.registerDialog(upper);
    announcer.announce("Imported");
    close();
    settle();
    expect(page.polite.textContent).toBe("Imported");
    expect(upper.history.filter((text) => text !== "")).toEqual([]);
  });

  it("is given up only by whoever has it", () => {
    const announcer = new Announcer();
    const first = region();
    const second = region();
    const removeFirst = announcer.registerPage(first);
    announcer.registerPage(second);
    removeFirst();
    announcer.announce("Still here");
    settle();
    expect(second.polite.textContent).toBe("Still here");
  });
});
