import type { ReactNode } from "react";
import { t } from "../i18n/index.ts";

export interface PageLinkProps {
  readonly to: string;
  readonly onNavigate: (path: string) => void;
  readonly className?: string;
  readonly children?: ReactNode;
}

/** A link to another page of the app. */
export function PageLink({ to, onNavigate, className, children }: PageLinkProps) {
  return (
    <a
      href={to}
      className={className}
      onClick={(e) => {
        e.preventDefault();
        onNavigate(to);
      }}
    >
      {children}
    </a>
  );
}

export interface PageNavProps {
  readonly onNavigate: (path: string) => void;
  /** What the link to the front page reads. */
  readonly brand?: string;
  /** Paths and what their links read. */
  readonly links: readonly (readonly [path: string, text: string])[];
}

/** The bar at the top of every page but the editor. */
export function PageNav({ onNavigate, brand = "Reticula", links }: PageNavProps) {
  return (
    <nav className="page-nav" aria-label={t("Pages")}>
      <PageLink to="/" className="page-brand" onNavigate={onNavigate}>
        {brand}
      </PageLink>
      <div className="page-nav-links">
        {links.map(([path, text]) => (
          <PageLink key={path} to={path} className="toolbar-btn" onNavigate={onNavigate}>
            {text}
          </PageLink>
        ))}
      </div>
    </nav>
  );
}
