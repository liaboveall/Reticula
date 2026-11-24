import { tabbablesIn } from "../ui/focus.ts";

/** The parts of the editor, in the order the keys go through them. */
const REGIONS = [
  ".titlebar",
  ".collab-unsynced-banner",
  ".formatbar",
  ".banners",
  ".formulabar",
  ".sheet .reticula-grid",
  ".lock-overlay",
  ".sheetbar",
  ".statusbar",
  ".app aside",
];

/** What had the focus in each of them last. */
const left = new WeakMap<Element, HTMLElement>();

/** Where the focus goes in a part of the editor, if it has any place for it. */
function entryOf(region: Element): HTMLElement | null {
  if (region.closest("[inert]") !== null) {
    return null;
  }
  const main = region.matches(".reticula-grid") ? region : region.querySelector(".cm-content");
  if (main instanceof HTMLElement) {
    return main;
  }
  const stops = tabbablesIn(region);
  const last = left.get(region);
  return (last && stops.includes(last) ? last : stops[0]) ?? null;
}

/** Takes the focus to the next part of the editor, or the previous one. */
export function cycleRegions(direction: 1 | -1): void {
  const regions = REGIONS.flatMap((region) => [...document.querySelectorAll(region)]);
  const { activeElement } = document;
  const from = regions.findIndex((region) => region.contains(activeElement));
  const current = regions[from];
  if (current && activeElement instanceof HTMLElement) {
    left.set(current, activeElement);
  }
  const { length } = regions;
  const first = from === -1 && direction === -1 ? 0 : from;
  for (let step = 1; step <= length; step++) {
    const region = regions[(first + direction * step + length * step) % length];
    const entry = region && entryOf(region);
    if (entry) {
      entry.focus();
      return;
    }
  }
}
