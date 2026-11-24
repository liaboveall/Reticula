import type { RendererComparison } from "../generated/benchmarks.ts";
import { fmtInt, t } from "../i18n/index.ts";
import { RENDERER_NAMES } from "./names.ts";
import { measured, ms, percent } from "./numbers.ts";
import { Source } from "./Source.tsx";

export interface RenderersProps {
  readonly data: RendererComparison | null;
}

/** The two renderers under the same load, once that has been measured. */
export function Renderers({ data }: RenderersProps) {
  if (!data) {
    return null;
  }
  const most = Math.max(0, ...data.results.map((result) => result.workP99Ms ?? 0));
  return (
    <section id="renderers" className="landing-section" aria-labelledby="renderers-heading">
      <h2 id="renderers-heading">{t("Canvas 2D and WebGPU")}</h2>
      <p className="landing-intro">
        {t("{cells} cells in view, {width} × {height}, scrolling five screens a second.", {
          cells: fmtInt(data.visibleCells),
          width: data.width,
          height: data.height,
        })}
      </p>
      <table className="landing-renderers">
        <thead>
          <tr>
            <th scope="col">{t("Renderers")}</th>
            <th scope="col">{t("Frame work, p99")}</th>
            <th scope="col">{t("Median")}</th>
            <th scope="col">{t("Late frames")}</th>
          </tr>
        </thead>
        <tbody>
          {data.results.map(({ renderer, workP50Ms, workP99Ms, lateFrames }) => (
            <tr key={renderer}>
              <th scope="row">{RENDERER_NAMES[renderer]}</th>
              <td>
                <span
                  className="landing-bar"
                  style={{ inlineSize: `${most > 0 ? ((workP99Ms ?? 0) / most) * 60 : 0}%` }}
                />
                {measured(workP99Ms, ms)}
              </td>
              <td>{measured(workP50Ms, ms)}</td>
              <td>{measured(lateFrames, percent)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <Source of={data.provenance} />
    </section>
  );
}
