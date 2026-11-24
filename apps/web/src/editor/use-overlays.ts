import { useEffect, useState } from "react";
import type { ColorPopoverKind, ContextMenuState } from "./types.ts";

/**
 * What floats above the editor until something else is clicked or Escape is
 * pressed: the context menu and the toolbar's color popovers.
 */
export function useOverlays() {
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [colorPopover, setColorPopover] = useState<ColorPopoverKind | null>(null);

  useEffect(() => {
    if (!contextMenu && !colorPopover) {
      return;
    }
    const dismiss = (event: MouseEvent) => {
      const target = event.target;
      if (
        target instanceof Element &&
        (target.closest(".context-menu") || target.closest(".color-popover-wrap"))
      ) {
        return;
      }
      setContextMenu(null);
      setColorPopover(null);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setContextMenu(null);
        setColorPopover(null);
      }
    };
    window.addEventListener("mousedown", dismiss);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", dismiss);
      window.removeEventListener("keydown", onKey);
    };
  }, [contextMenu, colorPopover]);

  return { contextMenu, setContextMenu, colorPopover, setColorPopover };
}
