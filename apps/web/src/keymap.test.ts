import { afterEach, describe, expect, it, vi } from "vitest";
import { keysOf, writtenKeysOf } from "./commands/key-names.ts";
import { commandFor, type FocusScope, focusScope, KEYS, type KeyPress } from "./keymap.ts";

/** A key with the modifiers the way a table or a test writes them. */
function press(keys: string, more: Partial<KeyPress> & { altGraph?: boolean } = {}): KeyPress {
  const parts = keys.split("+");
  const key = parts.pop() ?? "";
  const { altGraph = false, ...rest } = more;
  return {
    key: key === "Space" ? " " : /^[A-Z]$/.test(key) ? key.toLowerCase() : key,
    code: /^[A-Z]$/.test(key) ? `Key${key}` : key,
    keyCode: 0,
    ctrlKey: parts.includes("Ctrl"),
    metaKey: parts.includes("Cmd"),
    altKey: parts.includes("Alt"),
    shiftKey: parts.includes("Shift"),
    isComposing: false,
    defaultPrevented: false,
    getModifierState: (modifier) => modifier === "AltGraph" && altGraph,
    ...rest,
  };
}

const onPc = (keys: string) => keys.replace("Mod", "Ctrl");
const onMac = (keys: string) => keys.replace("Mod", "Cmd");

const ROWS = KEYS.map(([command, reach, keys, mac = keys]) => ({ command, reach, keys, mac }));

/** Where each kind of command is reached. */
const REACHED: Readonly<Record<string, readonly FocusScope[]>> = {
  anywhere: ["input", "editing", "grid", "page"],
  page: ["grid", "page"],
  grid: ["grid"],
  shown: [],
};

const SCOPES: readonly FocusScope[] = ["modal", "input", "editing", "grid", "page"];

describe("the keys", () => {
  it("are the ones that were agreed on", () => {
    const pc = ROWS.map(({ command, reach, keys }) => [command, reach, onPc(keys)].join(" "));
    expect(pc).toEqual([
      "nextRegion anywhere F6",
      "previousRegion anywhere Shift+F6",
      "palette page Ctrl+/",
      "goTo page Ctrl+G",
      "find page Ctrl+F",
      "replace page Ctrl+H",
      "undo page Ctrl+Z",
      "redo page Ctrl+Y",
      "redo page Ctrl+Shift+Z",
      "bold page Ctrl+B",
      "italic page Ctrl+I",
      "strikethrough page Ctrl+5",
      "nextSheet page Alt+PageDown",
      "previousSheet page Alt+PageUp",
      "contextMenu grid Shift+F10",
      "contextMenu grid ContextMenu",
      "selectRows grid Shift+Space",
      "selectCols grid Ctrl+Space",
      "fillDown grid Ctrl+D",
      "fillRight grid Ctrl+R",
      "filterColumn grid Alt+ArrowDown",
      "selectAll shown Ctrl+A",
      "cut shown Ctrl+X",
      "copy shown Ctrl+C",
      "paste shown Ctrl+V",
      "clearContents shown Delete",
    ]);
    const different = ROWS.filter(({ keys, mac }) => keys !== mac);
    expect(different.map(({ command, mac }) => `${command} ${onMac(mac)}`)).toEqual([
      "replace Cmd+Shift+H",
      "redo Cmd+Shift+Z",
      "strikethrough Cmd+Shift+X",
      "selectCols Ctrl+Space",
    ]);
  });

  it.each(ROWS)("$keys is $command where it applies, and nothing elsewhere", (row) => {
    const { command, reach, keys, mac } = row;
    for (const scope of SCOPES) {
      const expected = REACHED[reach]?.includes(scope) ? command : null;
      expect(commandFor(press(onPc(keys)), scope, false), `${scope} on a PC`).toBe(expected);
      expect(commandFor(press(onMac(mac)), scope, true), `${scope} on a Mac`).toBe(expected);
    }
  });

  it("are Cmd on a Mac and Ctrl elsewhere, and not the other way round", () => {
    for (const keys of ["B", "F", "H", "G", "D", "R", "Z", "/"]) {
      expect(commandFor(press(`Ctrl+${keys}`), "grid", true), keys).toBeNull();
      expect(commandFor(press(`Cmd+${keys}`), "grid", false), keys).toBeNull();
      expect(commandFor(press(`Ctrl+Cmd+${keys}`), "grid", false), keys).toBeNull();
      expect(commandFor(press(`Ctrl+Cmd+${keys}`), "grid", true), keys).toBeNull();
    }
  });

  it("are no commands with AltGr, which types characters on a German keyboard", () => {
    // As Windows reports AltGr+Q, which is @, and AltGr+7, which is {.
    for (const [key, code] of [
      ["@", "KeyQ"],
      ["{", "Digit7"],
      ["b", "KeyB"],
      ["PageDown", "PageDown"],
    ] as const) {
      const event = press("Ctrl+Alt+x", { key, code, altGraph: true });
      expect(commandFor(event, "grid", false), key).toBeNull();
    }
    // Even where the key is not told apart, Ctrl with Alt is none.
    for (const { keys } of ROWS.filter(({ keys }) => keys.startsWith("Mod+"))) {
      expect(commandFor(press(`Alt+${onPc(keys)}`), "grid", false), keys).toBeNull();
    }
  });

  it("are the input method's while it is composing", () => {
    for (const more of [{ isComposing: true }, { keyCode: 229 }]) {
      for (const { keys } of ROWS) {
        expect(commandFor(press(onPc(keys), more), "grid", false), keys).toBeNull();
      }
    }
  });

  it("are left to whoever has used them", () => {
    for (const { keys } of ROWS) {
      expect(commandFor(press(onPc(keys), { defaultPrevented: true }), "grid", false)).toBeNull();
    }
  });

  it("are found on other keyboards", () => {
    // Shift+7 on a German one, a letter of a Russian one, the 5 of a French one.
    const slash = press("Ctrl+Shift+/", { key: "/", code: "Digit7" });
    expect(commandFor(slash, "page", false)).toBe("palette");
    const russian = press("Ctrl+F", { key: "а", code: "KeyF" });
    expect(commandFor(russian, "page", false)).toBe("find");
    expect(commandFor(press("Ctrl+Shift+5"), "page", false)).toBe("strikethrough");
    expect(commandFor(press("Ctrl+B", { key: "B" }), "grid", false)).toBe("bold");
  });

  it("are not taken with a modifier too many", () => {
    expect(commandFor(press("Ctrl+Shift+B"), "grid", false)).toBeNull();
    expect(commandFor(press("Shift+ArrowDown"), "grid", false)).toBeNull();
    expect(commandFor(press("Alt+F6"), "grid", false)).toBeNull();
    expect(commandFor(press("Ctrl+PageDown"), "grid", false)).toBeNull();
    expect(commandFor(press("F"), "grid", false)).toBeNull();
  });
});

describe("where the focus is", () => {
  /** An element that is inside of what the selectors name. */
  class FakeElement {
    readonly #within: string;

    constructor(within: string) {
      this.#within = within;
    }

    closest(selectors: string): FakeElement | null {
      return selectors.split(", ").includes(this.#within) ? this : null;
    }
  }

  const scopeOf = (within: string, { inGrid = false, isEditing = false, modal = false } = {}) => {
    vi.stubGlobal("Element", FakeElement);
    vi.stubGlobal("document", { querySelector: () => (modal ? {} : null) });
    const target = new FakeElement(within) as unknown as Element;
    return focusScope(target, { contains: () => inGrid }, isEditing);
  };

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("is told apart", () => {
    expect(scopeOf("button")).toBe("page");
    expect(scopeOf("body")).toBe("page");
    for (const within of ["input", "textarea", "select", '[contenteditable="true"]']) {
      expect(scopeOf(within), within).toBe("input");
    }
    // The cell editor is there at all times, and text is only typed into it at some.
    expect(scopeOf('[contenteditable="true"]', { inGrid: true })).toBe("grid");
    expect(scopeOf('[contenteditable="true"]', { inGrid: true, isEditing: true })).toBe("editing");
    for (const within of ["button", "input"]) {
      expect(scopeOf(within, { modal: true }), within).toBe("modal");
    }
    vi.stubGlobal("document", { querySelector: () => null });
    expect(focusScope(null, null, false)).toBe("page");
  });
});

describe("the names of the keys", () => {
  it("are those on the keyboard", () => {
    expect(keysOf("bold", false)).toBe("Ctrl+B");
    expect(keysOf("bold", true)).toBe("⌘B");
    expect(keysOf("strikethrough", true)).toBe("⌘⇧X");
    expect(keysOf("filterColumn", false)).toBe("Alt+↓");
    expect(keysOf("filterColumn", true)).toBe("⌥↓");
    expect(keysOf("selectCols", false)).toBe("Ctrl+Space");
    expect(keysOf("selectCols", true)).toBe("⌃Space");
    expect(keysOf("redo", false)).toBe("Ctrl+Y");
    expect(keysOf("redo", true)).toBe("⌘⇧Z");
    expect(keysOf("clearContents", false)).toBe("Del");
    expect(keysOf("workbooks", false)).toBe("");
    expect(writtenKeysOf("palette", false)).toBe("Mod+/");
  });
});
