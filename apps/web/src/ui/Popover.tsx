import {
  type KeyboardEvent,
  lazy,
  type MouseEvent,
  type ReactNode,
  type RefObject,
  Suspense,
  useId,
  useRef,
} from "react";
import type { PopupKind, PopupProps } from "./Popup.tsx";
import { resolved } from "./resolved.ts";

let popup: typeof import("./Popup.tsx") | null = null;

/** Loads what opens, which is not needed to show what it is opened with. */
export async function loadPopup(): Promise<typeof import("./Popup.tsx")> {
  popup ??= await import("./Popup.tsx");
  return popup;
}

const LazyPopup = lazy(() => (popup ? resolved(popup) : loadPopup()));

/** What the button that opens a popover has to have. */
export interface PopoverTriggerProps {
  readonly ref: RefObject<HTMLButtonElement | null>;
  readonly id: string;
  readonly "aria-haspopup": PopupKind;
  readonly "aria-expanded": boolean;
  readonly "aria-controls": string | undefined;
  readonly onClick: (event: MouseEvent) => void;
  readonly onKeyDown: (event: KeyboardEvent) => void;
}

export interface PopoverProps
  extends Pick<PopupProps, "label" | "layout" | "className" | "testId" | "children"> {
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
  readonly trigger: (props: PopoverTriggerProps) => ReactNode;
  readonly kind?: PopupKind;
}

/** Something to choose from, below the button it belongs to. */
export function Popover({
  open,
  onOpenChange,
  trigger,
  kind = "dialog",
  children,
  ...popup
}: PopoverProps) {
  const id = useId();
  const triggerRef = useRef<HTMLButtonElement>(null);
  const opens = (event: MouseEvent | KeyboardEvent) =>
    event.currentTarget.getAttribute("aria-disabled") !== "true";
  return (
    <div className="popover-wrap">
      {trigger({
        ref: triggerRef,
        id: `${id}-trigger`,
        "aria-haspopup": kind,
        "aria-expanded": open,
        "aria-controls": open ? id : undefined,
        onClick: (event) => opens(event) && onOpenChange(!open),
        onKeyDown: (event) => {
          if (!open && (event.key === "ArrowDown" || event.key === "ArrowUp") && opens(event)) {
            event.preventDefault();
            onOpenChange(true);
          }
        },
      })}
      {open && (
        <Suspense fallback={null}>
          <LazyPopup
            {...popup}
            id={id}
            kind={kind}
            anchor={triggerRef}
            onClose={() => onOpenChange(false)}
          >
            {children}
          </LazyPopup>
        </Suspense>
      )}
    </div>
  );
}
