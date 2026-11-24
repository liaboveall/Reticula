import { type ReactNode, type RefObject, useLayoutEffect, useRef } from "react";
import { registerDialogRegion } from "./announcer.ts";
import { isComposing, keepFocusIn, restoreFocus } from "./focus.ts";

interface DialogCommonProps {
  readonly onClose: () => void;
  /** Whether Escape closes it. Otherwise the user has to choose something. */
  readonly closable?: boolean;
  /** Whether pressing beside it closes it. */
  readonly closeOnBackdrop?: boolean;
  /** What has the focus at first. Without it, that is the dialog itself. */
  readonly initialFocus?: RefObject<HTMLElement | null>;
  /** What had the focus when it was opened (`useOpener`). It has it again afterwards. */
  readonly opener?: RefObject<Element | null> | undefined;
  /** Takes the focus when the opener is gone. */
  readonly fallbackFocus?: (() => void) | undefined;
  readonly describedBy?: string;
  readonly className?: string;
  readonly testId?: string;
  readonly children: ReactNode;
}

/** A dialog has a name: a heading of its own, or one it is given. */
export type DialogProps = DialogCommonProps &
  (
    | { readonly labelledBy: string; readonly label?: undefined }
    | { readonly label: string; readonly labelledBy?: undefined }
  );

/** The open ones, in the order they were opened. */
const openDialogs: HTMLDialogElement[] = [];

function isBeside(dialog: HTMLDialogElement, event: MouseEvent): boolean {
  if (event.target !== dialog) {
    return false;
  }
  const { left, right, top, bottom } = dialog.getBoundingClientRect();
  const { clientX: x, clientY: y } = event;
  return x < left || x > right || y < top || y > bottom;
}

/**
 * A modal dialog, for as long as it is rendered. The browser keeps the focus in
 * it and everything else inert; what it leaves open is settled here: where the
 * focus goes first and afterwards, and who an Escape is for.
 */
export function Dialog({
  label,
  labelledBy,
  onClose,
  closable = true,
  closeOnBackdrop = false,
  initialFocus,
  opener,
  fallbackFocus,
  describedBy,
  className,
  testId,
  children,
}: DialogProps) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const politeRef = useRef<HTMLDivElement>(null);
  const assertiveRef = useRef<HTMLDivElement>(null);
  const latest = useRef({ onClose, closable, closeOnBackdrop, fallbackFocus });
  useLayoutEffect(() => {
    latest.current = { onClose, closable, closeOnBackdrop, fallbackFocus };
  });

  useLayoutEffect(() => {
    const dialog = dialogRef.current;
    const polite = politeRef.current;
    const assertive = assertiveRef.current;
    if (!dialog || !polite || !assertive) {
      return;
    }
    const returnTo = opener?.current ?? document.activeElement;
    let escapeIsDown = false;
    let pressedBeside = false;

    const open = () => {
      dialog.showModal();
      (initialFocus?.current ?? dialog).focus();
    };

    // An Escape is for what is being edited in here first. Only afterwards is
    // it known whether that had a use for it, and the browser would not wait.
    const onAnyKeyDown = (event: KeyboardEvent) => {
      escapeIsDown = event.key === "Escape" || isComposing(event);
    };
    const onAnyKeyUp = () => {
      escapeIsDown = false;
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (
        event.key !== "Escape" ||
        event.defaultPrevented ||
        isComposing(event) ||
        openDialogs.at(-1) !== dialog
      ) {
        return;
      }
      event.preventDefault();
      if (latest.current.closable) {
        latest.current.onClose();
      }
    };
    // What the browser closes dialogs on besides: a gesture to go back, say.
    const onCancel = (event: Event) => {
      // The cancel event of a file input inside bubbles.
      if (event.target !== dialog) {
        return;
      }
      event.preventDefault();
      if (latest.current.closable && !escapeIsDown) {
        latest.current.onClose();
      }
    };
    // It comes a moment later, when the dialog may be open again: that is what
    // React's strict mode does to it, and no reason to close it for good.
    const onNativeClose = (event: Event) => {
      if (event.target !== dialog || dialog.open) {
        return;
      }
      if (latest.current.closable) {
        latest.current.onClose();
      } else {
        open();
      }
    };
    const onPointerDown = (event: PointerEvent) => {
      pressedBeside = isBeside(dialog, event);
    };
    const onClick = (event: MouseEvent) => {
      if (pressedBeside && latest.current.closeOnBackdrop && isBeside(dialog, event)) {
        latest.current.onClose();
      }
    };

    open();
    openDialogs.push(dialog);
    const unregister = registerDialogRegion({ polite, assertive, isOpen: () => dialog.open });
    const letFocusGo = keepFocusIn(dialog);
    window.addEventListener("keydown", onAnyKeyDown, { capture: true });
    window.addEventListener("keyup", onAnyKeyUp, { capture: true });
    document.addEventListener("keydown", onKeyDown);
    dialog.addEventListener("cancel", onCancel);
    dialog.addEventListener("close", onNativeClose);
    dialog.addEventListener("pointerdown", onPointerDown);
    dialog.addEventListener("click", onClick);
    return () => {
      window.removeEventListener("keydown", onAnyKeyDown, { capture: true });
      window.removeEventListener("keyup", onAnyKeyUp, { capture: true });
      document.removeEventListener("keydown", onKeyDown);
      dialog.removeEventListener("cancel", onCancel);
      dialog.removeEventListener("close", onNativeClose);
      dialog.removeEventListener("pointerdown", onPointerDown);
      dialog.removeEventListener("click", onClick);
      letFocusGo();
      unregister();
      openDialogs.splice(openDialogs.indexOf(dialog), 1);
      dialog.close();
      restoreFocus(returnTo, latest.current.fallbackFocus);
    };
  }, [initialFocus, opener]);

  return (
    <dialog
      ref={dialogRef}
      className={className ? `dialog ${className}` : "dialog"}
      aria-label={label}
      aria-labelledby={labelledBy}
      aria-describedby={describedBy}
      closedby={closable ? "closerequest" : "none"}
      tabIndex={-1}
      data-testid={testId}
    >
      {children}
      <div ref={politeRef} className="visually-hidden" role="status" />
      <div ref={assertiveRef} className="visually-hidden" role="alert" />
    </dialog>
  );
}
