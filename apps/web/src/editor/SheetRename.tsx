import type { SheetTabInfo } from "@reticula/protocol";
import { type RefObject, useLayoutEffect, useRef } from "react";
import { t } from "../i18n/index.ts";
import { isComposing } from "../ui/focus.ts";

export interface SheetRenameProps {
  readonly tab: SheetTabInfo;
  /** The tabs, one of which it covers. */
  readonly list: RefObject<HTMLElement | null>;
  /** Called once, with what the sheet is to be called, if that is to be changed. */
  readonly onDone: (name: string | null, byKey: boolean) => void;
}

/** Over a worksheet's tab, while it is renamed: in a list of tabs there is nothing but tabs. */
export default function SheetRename({ tab, list, onDone }: SheetRenameProps) {
  const inputRef = useRef<HTMLInputElement>(null);
  const isOver = useRef(false);

  useLayoutEffect(() => {
    const input = inputRef.current;
    const covered = list.current?.querySelector<HTMLElement>(`[data-testid="sheet-tab-${tab.id}"]`);
    if (input && covered) {
      input.style.left = `${covered.offsetLeft}px`;
      input.style.minWidth = `${covered.offsetWidth}px`;
    }
    input?.focus();
    input?.select();
  }, [list, tab.id]);

  // The input loses the focus when it goes, and that is not the end of it once more.
  const finish = (name: string | null, byKey: boolean) => {
    if (!isOver.current) {
      isOver.current = true;
      const trimmed = name?.trim();
      onDone(trimmed && trimmed !== tab.name ? trimmed : null, byKey);
    }
  };

  return (
    <input
      ref={inputRef}
      type="text"
      className="sheet-rename-input"
      aria-label={t("New name of {title}", { title: tab.name })}
      data-testid="rename-sheet-input"
      defaultValue={tab.name}
      maxLength={31}
      onBlur={(event) => finish(event.target.value, false)}
      onKeyDown={(event) => {
        if (isComposing(event.nativeEvent)) {
          return;
        }
        if (event.key === "Enter") {
          event.preventDefault();
          finish(event.currentTarget.value, true);
        } else if (event.key === "Escape") {
          event.preventDefault();
          finish(null, true);
        }
      }}
    />
  );
}
