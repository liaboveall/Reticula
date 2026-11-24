import { type MouseEvent, useEffect, useRef, useState } from "react";
import { t } from "../i18n/index.ts";
import { isModified } from "./Link.tsx";

export interface SheetTabsProps {
  /** The sections of the page by their ids, in order. */
  readonly sections: readonly (readonly [id: string, text: string])[];
}

/** Brings a section into view, and the keyboard with it. */
function goTo(id: string): boolean {
  const section = document.getElementById(id);
  if (!section) {
    return false;
  }
  section.scrollIntoView();
  const heading = section.querySelector<HTMLElement>("h1, h2");
  if (heading) {
    heading.tabIndex = -1;
    heading.focus({ preventScroll: true });
  }
  return true;
}

/** The last section that starts in the upper third of the window, or at the end of the page the last there is. */
function sectionInView(ids: readonly string[]): string {
  const { scrollHeight } = document.documentElement;
  if (window.scrollY > 0 && window.scrollY + window.innerHeight >= scrollHeight - 2) {
    return ids.at(-1) ?? "";
  }
  const started = ids.filter((id) => {
    const section = document.getElementById(id);
    return section && section.getBoundingClientRect().top <= window.innerHeight / 3;
  });
  return started.at(-1) ?? ids[0] ?? "";
}

/** The page's sections, as the sheets of a workbook. */
export function SheetTabs({ sections }: SheetTabsProps) {
  const [current, setCurrent] = useState(sections[0]?.[0] ?? "");
  const bar = useRef<HTMLElement>(null);
  const ids = sections.map(([id]) => id).join(" ");

  useEffect(() => {
    const order = ids.split(" ");
    const update = () => {
      setCurrent(sectionInView(order));
    };
    window.addEventListener("scroll", update, { passive: true });
    window.addEventListener("resize", update);
    return () => {
      window.removeEventListener("scroll", update);
      window.removeEventListener("resize", update);
    };
  }, [ids]);

  // The address may name one, which is only there now.
  useEffect(() => {
    const asked = window.location.hash.slice(1);
    if (ids.split(" ").includes(asked)) {
      document.getElementById(asked)?.scrollIntoView();
    }
  }, [ids]);

  // Where the bar is too narrow for all of them.
  useEffect(() => {
    const nav = bar.current;
    const tab = nav?.querySelector<HTMLElement>(`a[href="#${current}"]`);
    if (nav && tab) {
      nav.scrollLeft = tab.offsetLeft - (nav.clientWidth - tab.offsetWidth) / 2;
    }
  }, [current]);

  const open = (event: MouseEvent, id: string) => {
    if (isModified(event) || !goTo(id)) {
      return;
    }
    event.preventDefault();
    // Following the link would be one more page to go back through.
    window.history.replaceState(window.history.state, "", `#${id}`);
    setCurrent(id);
  };

  return (
    <nav ref={bar} className="landing-tabs" aria-label={t("Sections")} data-testid="landing-tabs">
      {sections.map(([id, text]) => (
        <a
          key={id}
          href={`#${id}`}
          className={id === current ? "sheet-tab active" : "sheet-tab"}
          aria-current={id === current ? "true" : undefined}
          onClick={(event) => open(event, id)}
        >
          {text}
        </a>
      ))}
    </nav>
  );
}
