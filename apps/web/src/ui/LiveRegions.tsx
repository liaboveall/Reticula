import { useLayoutEffect, useRef } from "react";
import { registerPageRegion } from "./announcer.ts";

/** Where the page has things read out. There is one of these, at the root of the app. */
export function LiveRegions() {
  const polite = useRef<HTMLDivElement>(null);
  const assertive = useRef<HTMLDivElement>(null);

  useLayoutEffect(
    () =>
      polite.current && assertive.current
        ? registerPageRegion({ polite: polite.current, assertive: assertive.current })
        : undefined,
    [],
  );

  return (
    <>
      <div ref={polite} className="visually-hidden" role="status" data-testid="page-status" />
      <div ref={assertive} className="visually-hidden" role="alert" data-testid="page-alert" />
    </>
  );
}
