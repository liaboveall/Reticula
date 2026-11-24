import type { MouseEvent, ReactNode } from "react";
import { PRELOAD_EDITOR_ON_INTENT } from "./prefetch.ts";

export interface LinkProps {
  readonly to: string;
  readonly onNavigate: (path: string) => void;
  readonly className?: string;
  readonly testId?: string;
  readonly children: ReactNode;
}

/** Whether a click on a link asks for something else than following it here. */
export function isModified({ ctrlKey, metaKey, shiftKey, altKey }: MouseEvent): boolean {
  return ctrlKey || metaKey || shiftKey || altKey;
}

/**
 * A link to another page of the app. The editor is fetched as soon as somebody
 * reaches for the link to it.
 */
export function Link({ to, onNavigate, className, testId, children }: LinkProps) {
  return (
    <a
      href={to}
      className={className}
      data-testid={testId}
      {...(to === "/app" && PRELOAD_EDITOR_ON_INTENT)}
      onClick={(event) => {
        if (!isModified(event)) {
          event.preventDefault();
          onNavigate(to);
        }
      }}
    >
      {children}
    </a>
  );
}
