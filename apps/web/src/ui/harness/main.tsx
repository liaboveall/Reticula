/**
 * The dialogs and popovers by themselves, for the end-to-end tests. They are
 * run from the development server, where React's strict mode runs every effect
 * twice, as the production build does not.
 */

import { StrictMode, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { ActionButton } from "../ActionButton.tsx";
import { announce } from "../announcer.ts";
import { Dialog } from "../Dialog.tsx";
import { useDrawer } from "../drawer.ts";
import { isComposing } from "../focus.ts";
import { ListboxPopover } from "../ListboxPopover.tsx";
import { LiveRegions } from "../LiveRegions.tsx";
import { useOpener } from "../opener.ts";
import { Popover } from "../Popover.tsx";
import { Tabs } from "../Tabs.tsx";
import "../../app.css";
import "../ui.css";

const FRUIT = ["Apple", "Banana", "Blueberry", "Cherry"] as const;
type Fruit = (typeof FRUIT)[number];
const COLORS = ["Black", "Gray", "Silver", "White", "Red", "Green", "Blue"] as const;

function FruitList({ testId }: { readonly testId: string }) {
  const [fruit, setFruit] = useState<Fruit>("Banana");
  return (
    <ListboxPopover
      label="Fruit"
      options={FRUIT.map((value) => ({ value, text: value }))}
      value={fruit}
      onSelect={setFruit}
      testId={testId}
    />
  );
}

function Palette() {
  const [open, setOpen] = useState(false);
  const [color, setColor] = useState("none");
  return (
    <Popover
      label="Colors"
      open={open}
      onOpenChange={setOpen}
      layout={{ columns: 4 }}
      testId="palette"
      trigger={(props) => (
        <button {...props} type="button" data-testid="palette-button">
          Color: {color}
        </button>
      )}
    >
      <div style={{ display: "grid", gridTemplateColumns: "repeat(4, auto)" }}>
        {COLORS.map((name) => (
          <button
            key={name}
            type="button"
            tabIndex={-1}
            aria-pressed={name === color}
            onClick={() => {
              setColor(name);
              setOpen(false);
            }}
          >
            {name}
          </button>
        ))}
      </div>
    </Popover>
  );
}

function Plain({
  opener,
  onClose,
}: {
  readonly opener: React.RefObject<Element | null>;
  readonly onClose: () => void;
}) {
  const inputRef = useRef<HTMLInputElement>(null);
  const [items, setItems] = useState(["one", "two", "three"]);
  const [editing, setEditing] = useState(false);
  const [nested, setNested] = useState(false);
  const nestedOpener = useOpener(nested);
  const [uses, setUses] = useState(0);
  return (
    <Dialog
      labelledBy="plain-heading"
      closeOnBackdrop
      initialFocus={inputRef}
      opener={opener}
      fallbackFocus={() => document.getElementById("fallback")?.focus()}
      onClose={onClose}
      testId="plain"
    >
      <div className="dialog-header">
        <h2 id="plain-heading">Plain</h2>
        <button type="button" aria-label="Close" onClick={onClose}>
          ×
        </button>
      </div>
      <input ref={inputRef} aria-label="First" data-testid="first" />
      {editing ? (
        <input
          aria-label="Inline"
          data-testid="inline"
          onKeyDown={(event) => {
            if (event.key === "Escape" && !isComposing(event.nativeEvent)) {
              event.preventDefault();
              setEditing(false);
            }
          }}
        />
      ) : (
        <button type="button" data-testid="edit" onClick={() => setEditing(true)}>
          Edit
        </button>
      )}
      <ul>
        {items.map((item) => (
          <li key={item}>
            {item}{" "}
            <button
              type="button"
              data-testid={`remove-${item}`}
              onClick={() => setItems(items.filter((one) => one !== item))}
            >
              Remove {item}
            </button>
          </li>
        ))}
      </ul>
      <input type="file" aria-label="File" data-testid="file" />
      <button
        type="button"
        data-testid="announce-in-dialog"
        onClick={() => announce("From the dialog")}
      >
        Announce
      </button>
      <ActionButton
        inactive={uses === 1}
        data-testid="once"
        onClick={() => setUses((count) => count + 1)}
      >
        Used {uses} times
      </ActionButton>
      <FruitList testId="fruit-in-dialog" />
      <button type="button" data-testid="open-nested" onClick={() => setNested(true)}>
        Nested
      </button>
      {nested && (
        <Dialog
          label="Nested"
          opener={nestedOpener}
          onClose={() => setNested(false)}
          testId="nested"
        >
          <button type="button" onClick={() => announce("From the nested dialog")}>
            Announce above
          </button>
        </Dialog>
      )}
    </Dialog>
  );
}

function Drawer({
  open,
  opener,
  onClose,
}: {
  readonly open: boolean;
  readonly opener: React.RefObject<Element | null>;
  readonly onClose: () => void;
}) {
  const headingRef = useRef<HTMLHeadingElement>(null);
  const ref = useDrawer({
    open,
    initialFocus: headingRef,
    opener,
    onLeave: () => document.getElementById("fallback")?.focus(),
  });
  const [tab, setTab] = useState<"left" | "right">("left");
  return (
    <aside ref={ref} aria-label="Drawer" hidden={!open} data-testid="drawer">
      <h2 ref={headingRef} tabIndex={-1}>
        Drawer
      </h2>
      <Tabs
        label="Sides"
        tabs={[
          { key: "left", text: "Left", testId: "tab-left" },
          { key: "right", text: "Right", testId: "tab-right" },
        ]}
        selected={tab}
        onSelect={setTab}
        tabClassName="page-tab"
      >
        <p>The {tab} side</p>
      </Tabs>
      <button type="button" data-testid="close-drawer" onClick={onClose}>
        Close
      </button>
    </aside>
  );
}

type Shown = "plain" | "strict" | "vanishing" | null;

function Harness() {
  const [shown, setShown] = useState<Shown>(null);
  const opener = useOpener(shown !== null);
  const [closings, setClosings] = useState(0);
  const [drawer, setDrawer] = useState(false);
  const drawerOpener = useOpener(drawer);
  const close = () => {
    setShown(null);
    setClosings((count) => count + 1);
  };
  return (
    <>
      <LiveRegions />
      <main>
        <button type="button" data-testid="open-plain" onClick={() => setShown("plain")}>
          Plain
        </button>
        <button type="button" data-testid="open-strict" onClick={() => setShown("strict")}>
          Not closable
        </button>
        {shown !== "vanishing" && closings === 0 && (
          <button type="button" data-testid="open-vanishing" onClick={() => setShown("vanishing")}>
            Gone afterwards
          </button>
        )}
        <button type="button" id="fallback" data-testid="fallback">
          Fallback
        </button>
        <button type="button" data-testid="announce" onClick={() => announce("From the page")}>
          Announce
        </button>
        <button type="button" data-testid="toggle-drawer" onClick={() => setDrawer(!drawer)}>
          Drawer
        </button>
        <Palette />
        <FruitList testId="fruit" />
        <button type="button" data-testid="after">
          After
        </button>
        <p>
          Closed <span data-testid="closings">{closings}</span> times
        </p>
        <Drawer open={drawer} opener={drawerOpener} onClose={() => setDrawer(false)} />
      </main>
      {(shown === "plain" || shown === "vanishing") && <Plain opener={opener} onClose={close} />}
      {shown === "strict" && (
        <Dialog label="Choose" closable={false} opener={opener} onClose={close} testId="strict">
          <button type="button" data-testid="choose" onClick={close}>
            This one
          </button>
        </Dialog>
      )}
    </>
  );
}

const container = document.getElementById("root");
if (container) {
  createRoot(container).render(
    <StrictMode>
      <Harness />
    </StrictMode>,
  );
}
