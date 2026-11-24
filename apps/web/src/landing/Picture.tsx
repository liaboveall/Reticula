import { getLocale } from "../i18n/index.ts";
import { SHOTS, type ShotName, shotUrl } from "./shots.ts";

export interface PictureProps {
  readonly shot: ShotName;
  readonly alt: string;
  /** Whether it is in view when the page opens. */
  readonly first?: boolean;
}

/** A picture of the product in the page's language and theme. */
export function Picture({ shot, alt, first = false }: PictureProps) {
  const language = getLocale() === "en" ? "en" : "zh";
  const { width, height } = SHOTS[shot];
  return (
    <picture>
      <source
        media="(prefers-color-scheme: dark)"
        srcSet={shotUrl(shot, language, "dark")}
        width={width}
        height={height}
      />
      <img
        className={`landing-shot landing-shot-${shot}`}
        src={shotUrl(shot, language, "light")}
        alt={alt}
        width={width}
        height={height}
        loading={first ? "eager" : "lazy"}
        fetchPriority={first ? "high" : "auto"}
        decoding="async"
      />
    </picture>
  );
}
