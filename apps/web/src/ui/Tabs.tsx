import { type KeyboardEvent, type ReactNode, useId } from "react";
import { rovingTarget } from "./roving.ts";

export interface TabItem<K extends string> {
  readonly key: K;
  readonly text: string;
  readonly testId?: string;
}

export interface TabsProps<K extends string> {
  readonly label: string;
  readonly tabs: readonly TabItem<K>[];
  readonly selected: K;
  readonly onSelect: (key: K) => void;
  readonly listClassName?: string;
  readonly tabClassName: string;
  readonly panelClassName?: string;
  /** What of the header goes beside the list of tabs, and around it. */
  readonly header?: (list: ReactNode) => ReactNode;
  readonly children: ReactNode;
}

/** A row of tabs and the panel of the selected one. The arrow keys select. */
export function Tabs<K extends string>({
  label,
  tabs,
  selected,
  onSelect,
  listClassName,
  tabClassName,
  panelClassName,
  header,
  children,
}: TabsProps<K>) {
  const id = useId();
  const tabId = (key: K) => `${id}-tab-${key}`;
  const panelId = `${id}-panel`;

  const onKeyDown = (event: KeyboardEvent, index: number) => {
    const to = rovingTarget(event.key, index, tabs.length, {
      orientation: "horizontal",
      wrap: true,
    });
    const tab = to === null ? undefined : tabs[to];
    if (tab && !event.ctrlKey && !event.metaKey && !event.altKey) {
      event.preventDefault();
      document.getElementById(tabId(tab.key))?.focus();
      onSelect(tab.key);
    }
  };

  const list = (
    <div className={listClassName} role="tablist" aria-label={label}>
      {tabs.map((tab, index) => {
        const isSelected = tab.key === selected;
        return (
          <button
            key={tab.key}
            type="button"
            id={tabId(tab.key)}
            role="tab"
            aria-selected={isSelected}
            aria-controls={panelId}
            tabIndex={isSelected ? 0 : -1}
            className={isSelected ? `${tabClassName} active` : tabClassName}
            data-testid={tab.testId}
            onClick={() => onSelect(tab.key)}
            onKeyDown={(event) => onKeyDown(event, index)}
          >
            {tab.text}
          </button>
        );
      })}
    </div>
  );
  return (
    <>
      {header ? header(list) : list}
      <div
        id={panelId}
        role="tabpanel"
        aria-labelledby={tabId(selected)}
        className={panelClassName}
      >
        {children}
      </div>
    </>
  );
}
