import { laterNow, loadLater } from "../editor/load-later.ts";
import type { EditorHandle } from "./handle.ts";

/** Runs a command. The focus stays where it is, unless that is nowhere. */
export function runCommand(handle: EditorHandle, id: string, sheet?: number): void {
  const run = ({ run }: Pick<NonNullable<ReturnType<typeof laterNow>>, "run">) => {
    run(handle, id, sheet);
    const { activeElement, body } = document;
    if (activeElement === null || activeElement === body) {
      (handle.state().table?.view ?? handle.cellEditor())?.focus();
    }
  };
  const later = laterNow();
  if (later) {
    run(later);
  } else {
    void loadLater().then(run);
  }
}
