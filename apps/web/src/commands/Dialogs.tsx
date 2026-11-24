import { type KeyboardEvent, type RefObject, useId, useRef, useState } from "react";
import { flushSync } from "react-dom";
import type { EditorDialog } from "../editor/types.ts";
import { t, tn } from "../i18n/index.ts";
import { tx } from "../i18n/react.ts";
import { announce } from "../ui/announcer.ts";
import { Dialog } from "../ui/Dialog.tsx";
import { isComposing } from "../ui/focus.ts";
import { rovingTarget } from "../ui/roving.ts";
import type { EditorHandle } from "./handle.ts";
import { keysOf } from "./key-names.ts";
import { COMMANDS, GROUPS, isEnabled, run } from "./registry.ts";
import { findCommands } from "./search.ts";

export interface EditorDialogsProps {
  readonly handle: EditorHandle;
  readonly dialog: EditorDialog;
  readonly opener: RefObject<Element | null>;
  readonly onFocusGrid: () => void;
  readonly onClose: () => void;
}

export default function EditorDialogs(props: EditorDialogsProps) {
  const { dialog } = props;
  switch (dialog.kind) {
    case "palette":
      return <Palette {...props} />;
    case "help":
      return <KeyboardHelp {...props} />;
    case "size":
      return <SizeDialog {...props} axis={dialog.axis} />;
    case "deleteSheet":
      return <DeleteSheetDialog {...props} id={dialog.tab.id} name={dialog.tab.name} />;
  }
}

function Header({
  id,
  text,
  onClose,
}: {
  readonly id: string;
  readonly text: string;
  readonly onClose: () => void;
}) {
  return (
    <div className="dialog-header">
      <h2 id={id}>{text}</h2>
      <button type="button" className="tip-close" aria-label={t("Close")} onClick={onClose}>
        ×
      </button>
    </div>
  );
}

/** All commands, to be found by their names and run with Enter. */
function Palette({ handle, opener, onFocusGrid, onClose }: EditorDialogsProps) {
  const id = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const [query, setQuery] = useState("");
  const [at, setAt] = useState(0);
  const found = findCommands(query);
  const state = handle.state();

  const choose = (index: number) => {
    const command = found[index];
    if (command) {
      // The focus is back where it was when the command has it.
      flushSync(onClose);
      run(handle, command.id);
    }
  };

  const onKeyDown = (event: KeyboardEvent) => {
    if (isComposing(event.nativeEvent)) {
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      choose(at);
      return;
    }
    // Home and End are for the text.
    const to = /^Arrow/.test(event.key)
      ? rovingTarget(event.key, at, found.length, { wrap: true })
      : null;
    if (to !== null) {
      event.preventDefault();
      setAt(to);
      document.getElementById(`${id}-${to}`)?.scrollIntoView({ block: "nearest" });
    }
  };

  return (
    <Dialog
      label={t("Commands")}
      closeOnBackdrop
      initialFocus={inputRef}
      opener={opener}
      fallbackFocus={onFocusGrid}
      onClose={onClose}
      className="palette"
      testId="command-palette"
    >
      <input
        ref={inputRef}
        type="text"
        role="combobox"
        aria-label={t("Search commands")}
        aria-expanded={found.length > 0}
        aria-controls={`${id}-list`}
        aria-autocomplete="list"
        aria-activedescendant={found[at] && `${id}-${at}`}
        placeholder={t("Search commands")}
        autoComplete="off"
        spellCheck={false}
        data-testid="palette-input"
        value={query}
        onChange={(event) => {
          setQuery(event.target.value);
          setAt(0);
          const { length } = findCommands(event.target.value);
          announce(tn(length, "{n} command", "{n} commands"));
        }}
        onKeyDown={onKeyDown}
      />
      <div id={`${id}-list`} role="listbox" aria-label={t("Commands")} className="palette-list">
        {found.map((command, index) => {
          const keys = keysOf(command.id);
          return (
            <div
              key={command.id}
              id={`${id}-${index}`}
              role="option"
              tabIndex={-1}
              aria-selected={index === at}
              aria-disabled={!isEnabled(command, state) || undefined}
              className="palette-item"
              data-testid={`command-${command.id}`}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => choose(index)}
              onKeyDown={onKeyDown}
            >
              <span className="palette-group">{GROUPS[command.group]()}</span>
              <span className="palette-title">{command.title()}</span>
              {keys && <kbd>{keys}</kbd>}
            </div>
          );
        })}
      </div>
    </Dialog>
  );
}

/** What the keys do that have no command of their own. */
function gridKeys(): readonly (readonly [keys: string, text: string])[] {
  const mod = keysOf("selectAll").replace(/A$/, "");
  return [
    ["← ↑ → ↓", t("Move, and extend the selection with Shift")],
    [`${mod}← ↑ → ↓`, t("Go to the end of the data")],
    [`Home / End / ${mod}Home / ${mod}End`, t("Go to the ends of the row and of the sheet")],
    ["PageUp / PageDown", t("Go a page up or down")],
    ["F2", t("Edit the cell")],
    ["F4", t("Change $ in a reference")],
    ["Enter / Tab", t("Accept, and go down or right")],
    ["Esc", t("Cancel editing, dragging or the copied range")],
  ];
}

function KeyboardHelp({ opener, onFocusGrid, onClose }: EditorDialogsProps) {
  const id = useId();
  const commands = COMMANDS.flatMap(({ id: command, title }) => {
    const keys = keysOf(command);
    return keys ? [[keys, title()] as const] : [];
  });
  return (
    <Dialog
      labelledBy={id}
      closeOnBackdrop
      opener={opener}
      fallbackFocus={onFocusGrid}
      onClose={onClose}
      testId="keyboard-help"
    >
      <Header id={id} text={t("Keyboard Shortcuts")} onClose={onClose} />
      <table className="keys-table">
        <tbody>
          {[...commands, ...gridKeys()].map(([keys, text]) => (
            <tr key={text}>
              <th scope="row">{text}</th>
              <td>
                <kbd>{keys}</kbd>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </Dialog>
  );
}

function SizeDialog({
  handle,
  axis,
  opener,
  onFocusGrid,
  onClose,
}: EditorDialogsProps & { readonly axis: "row" | "col" }) {
  const id = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const [current] = useState(() => {
    const { rowPx = null, colPx = null } = handle.grid()?.sizeOfSelection() ?? {};
    return axis === "row" ? rowPx : colPx;
  });
  const [text, setText] = useState(current === null ? "" : String(current));
  const size = Number(text);
  const isValid = text.trim() !== "" && Number.isFinite(size) && size > 0;

  const apply = (px: number | null) => {
    const grid = handle.grid();
    flushSync(onClose);
    // It may have become read-only since this was opened; the grid would show another size.
    const done =
      !handle.state().readOnly &&
      (axis === "row" ? grid?.resizeSelectedRows(px) : grid?.resizeSelectedCols(px));
    if (done === false) {
      announce(t("The size could not be changed."), { politeness: "assertive" });
    }
  };

  return (
    <Dialog
      labelledBy={id}
      initialFocus={inputRef}
      opener={opener}
      fallbackFocus={onFocusGrid}
      onClose={onClose}
      className="small-dialog"
      testId="size-dialog"
    >
      <Header
        id={id}
        text={axis === "row" ? t("Row Height") : t("Column Width")}
        onClose={onClose}
      />
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (isValid) {
            apply(size);
          }
        }}
      >
        <label className="m3a-field">
          <span>{t("Pixels")}</span>
          <input
            ref={inputRef}
            type="number"
            min={1}
            max={1200}
            step={1}
            data-testid="size-input"
            placeholder={current === null ? t("Several sizes") : undefined}
            value={text}
            onFocus={(event) => event.target.select()}
            onChange={(event) => setText(event.target.value)}
          />
        </label>
        <div className="dialog-footer">
          <button
            type="button"
            className="toolbar-btn"
            data-testid="size-default"
            onClick={() => apply(null)}
          >
            {t("Default size")}
          </button>
          <button
            type="submit"
            className="toolbar-btn primary-btn"
            aria-disabled={!isValid || undefined}
            data-testid="size-apply"
          >
            {t("Apply|size")}
          </button>
        </div>
      </form>
    </Dialog>
  );
}

function DeleteSheetDialog({
  handle,
  id: sheet,
  name,
  opener,
  onFocusGrid,
  onClose,
}: EditorDialogsProps & { readonly id: number; readonly name: string }) {
  const id = useId();
  const cancelRef = useRef<HTMLButtonElement>(null);
  return (
    <Dialog
      labelledBy={id}
      describedBy={`${id}-text`}
      initialFocus={cancelRef}
      opener={opener}
      fallbackFocus={onFocusGrid}
      onClose={onClose}
      className="small-dialog"
      testId="delete-sheet-dialog"
    >
      <Header id={id} text={t("Delete Sheet")} onClose={onClose} />
      <p id={`${id}-text`}>
        {tx(
          "Delete <0>{name}</0> and everything on it? This can be undone.",
          { 0: <strong /> },
          { name },
        )}
      </p>
      <div className="dialog-footer">
        <button ref={cancelRef} type="button" className="toolbar-btn" onClick={onClose}>
          {t("Cancel")}
        </button>
        <button
          type="button"
          className="toolbar-btn primary-btn"
          data-testid="delete-sheet-confirm"
          onClick={() => {
            flushSync(onClose);
            handle.mutate({ kind: "deleteSheet", sheet }, { action: "deletedSheet" });
          }}
        >
          {t("Delete")}
        </button>
      </div>
    </Dialog>
  );
}
