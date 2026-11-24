import { Suspense } from "react";
import type { EditorHandle } from "../commands/handle.ts";
import { t } from "../i18n/index.ts";
import { lazyPart, withKeys } from "./load-later.ts";

const LazyNameBoxInput = lazyPart(({ NameBoxInput }) => NameBoxInput);

export interface NameBoxProps {
  readonly handle: EditorHandle;
  /** What is selected, or edited. */
  readonly text: string;
  readonly isEditing: boolean;
  readonly inactive: boolean;
  readonly onEdit: () => void;
  readonly onDone: (byKey: boolean) => void;
}

/** Shows what is selected, and takes what else to select. */
export function NameBox({ handle, text, isEditing, inactive, onEdit, onDone }: NameBoxProps) {
  const button = (
    <button
      type="button"
      className="name-box"
      data-testid="name-box"
      aria-label={t("Name box: {cell}", { cell: text })}
      aria-disabled={inactive || undefined}
      title={withKeys(t("Go to a Cell, Range or Name"), "goTo")}
      onClick={() => !inactive && onEdit()}
    >
      {text}
    </button>
  );
  return isEditing ? (
    <Suspense fallback={button}>
      <LazyNameBoxInput handle={handle} text={text} onDone={onDone} />
    </Suspense>
  ) : (
    button
  );
}
