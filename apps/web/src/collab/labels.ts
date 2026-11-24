import type { RemotePresenceOverlay } from "@reticula/grid";
import { t } from "../i18n/index.ts";

/** The label of the link that is made when viewing by link is allowed. It is stored with the link. */
export const LINK_VIEW_LABEL = "Link-view access";

export function shareLinkText(link: { readonly label: string; readonly role: string }): string {
  if (link.label === LINK_VIEW_LABEL) {
    return t("Link-view access");
  }
  return link.label || (link.role === "viewer" ? t("Viewer link") : t("Editor invite"));
}

/** A name to show. The ones the server gives out itself are not anybody's name. */
export function personText(name: string): string {
  switch (name) {
    case "Deleted user":
      return t("Deleted user");
    case "AI Analyst":
      return t("AI Analyst");
    case "Viewer":
      // Whoever views by a link without being signed in.
      return t("Viewer|person");
    default:
      return name;
  }
}

/** The cursors of others, for the grid, with their names as they are shown. */
export function withPersonTexts(
  presences: readonly RemotePresenceOverlay[],
): RemotePresenceOverlay[] {
  return presences.map((one) => ({ ...one, displayName: personText(one.displayName) }));
}
