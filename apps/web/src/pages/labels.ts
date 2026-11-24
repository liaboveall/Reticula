import { t } from "../i18n/index.ts";

/** What a member may do with a workbook, for display. */
export function roleName(role: string): string {
  switch (role) {
    case "owner":
      return t("owner|role");
    case "editor":
      return t("editor|role");
    case "viewer":
      return t("viewer|role");
    case "revoked":
      return t("revoked|role");
    default:
      return role;
  }
}

export function visibilityName(visibility: string): string {
  switch (visibility) {
    case "private":
      return t("private|visibility");
    case "link_view":
      return t("link_view|visibility");
    default:
      return visibility;
  }
}
