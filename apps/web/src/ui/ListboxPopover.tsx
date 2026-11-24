import { useId, useState } from "react";
import { Popover } from "./Popover.tsx";

export interface ListboxOption<V extends string> {
  readonly value: V;
  readonly text: string;
  readonly testId?: string;
}

export interface ListboxPopoverProps<V extends string> {
  readonly label: string;
  readonly options: readonly ListboxOption<V>[];
  /** The chosen one, or a value none of them has. */
  readonly value: V;
  /** What to show for a value that is not among the options. */
  readonly placeholder?: string;
  readonly onSelect: (value: V) => void;
  readonly inactive?: boolean;
  readonly className?: string;
  readonly testId?: string;
}

/**
 * A button that shows what is chosen and opens the list to choose from. Unlike
 * a closed native select, it changes nothing until something is chosen.
 */
export function ListboxPopover<V extends string>({
  label,
  options,
  value,
  placeholder,
  onSelect,
  inactive = false,
  className,
  testId,
}: ListboxPopoverProps<V>) {
  const id = useId();
  const [open, setOpen] = useState(false);
  const chosen = options.find((option) => option.value === value);
  return (
    <Popover
      label={label}
      kind="listbox"
      open={open}
      onOpenChange={setOpen}
      className="listbox"
      trigger={(props) => (
        <button
          {...props}
          type="button"
          className={className ? `listbox-button ${className}` : "listbox-button"}
          aria-labelledby={`${id}-label ${id}-value`}
          aria-disabled={inactive || undefined}
          data-testid={testId}
          data-value={value}
        >
          <span id={`${id}-label`} className="visually-hidden">
            {label}
          </span>
          <span id={`${id}-value`}>{chosen?.text ?? placeholder ?? value}</span>
        </button>
      )}
    >
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          role="option"
          aria-selected={option.value === value}
          tabIndex={-1}
          className="listbox-option"
          data-testid={option.testId}
          onClick={() => {
            setOpen(false);
            onSelect(option.value);
          }}
        >
          {option.text}
        </button>
      ))}
    </Popover>
  );
}
