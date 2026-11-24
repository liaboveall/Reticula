import type { ComponentPropsWithRef } from "react";

export interface ActionButtonProps
  extends Omit<ComponentPropsWithRef<"button">, "disabled" | "aria-disabled"> {
  /** There is nothing for it to do right now, perhaps because of what it has just done. */
  readonly inactive: boolean;
}

/**
 * A button that can be out of use and still be where the keyboard is. A disabled
 * one loses the focus, and pressing a button is often what puts it out of use.
 */
export function ActionButton({
  inactive,
  type = "button",
  onClick,
  children,
  ...rest
}: ActionButtonProps) {
  return (
    <button
      {...rest}
      type={type}
      aria-disabled={inactive || undefined}
      onClick={(event) => {
        if (inactive) {
          event.preventDefault();
        } else {
          onClick?.(event);
        }
      }}
    >
      {children}
    </button>
  );
}
