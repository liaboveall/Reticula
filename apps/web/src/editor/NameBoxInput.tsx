import { type KeyboardEvent, useId, useLayoutEffect, useRef, useState } from "react";
import type { EditorHandle } from "../commands/handle.ts";
import { t } from "../i18n/index.ts";
import { announce } from "../ui/announcer.ts";
import { isComposing } from "../ui/focus.ts";
import { goTo } from "./name-box.ts";

export interface NameBoxInputProps {
  readonly handle: EditorHandle;
  /** What is selected. */
  readonly text: string;
  /** Called when it is over: with whether the keyboard ended it. */
  readonly onDone: (byKey: boolean) => void;
}

/** Takes a reference or a name, and goes there. */
export default function NameBoxInput({ handle, text, onDone }: NameBoxInputProps) {
  const id = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const [problem, setProblem] = useState<string | null>(null);

  useLayoutEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  const onKeyDown = async (event: KeyboardEvent<HTMLInputElement>) => {
    if (isComposing(event.nativeEvent)) {
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      onDone(true);
    } else if (event.key === "Enter") {
      event.preventDefault();
      const wrong = await goTo(handle, event.currentTarget.value);
      if (!inputRef.current) {
        // It was left meanwhile.
        return;
      }
      if (wrong === null) {
        onDone(true);
      } else {
        setProblem(wrong);
        announce(wrong, { politeness: "assertive" });
        inputRef.current?.select();
      }
    }
  };

  return (
    <>
      <input
        ref={inputRef}
        type="text"
        className="name-box"
        aria-label={t("Go to a Cell, Range or Name")}
        aria-invalid={problem !== null}
        aria-describedby={problem === null ? undefined : id}
        autoComplete="off"
        spellCheck={false}
        data-testid="name-box-input"
        defaultValue={text}
        onChange={() => setProblem(null)}
        onKeyDown={(event) => void onKeyDown(event)}
        onBlur={() => onDone(false)}
      />
      {problem !== null && (
        <span id={id} className="formula-diag-pill" data-testid="name-box-problem">
          {problem}
        </span>
      )}
    </>
  );
}
