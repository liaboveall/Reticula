import { PROVENANCE, QUALITY } from "../generated/benchmarks.ts";
import { fmtInt, t } from "../i18n/index.ts";
import { OUTSIDE, sourceUrl } from "./links.ts";
import { measured, percent } from "./numbers.ts";
import { Source } from "./Source.tsx";

interface Check {
  readonly heading: string;
  readonly text: string;
  /** Where in the repository it is done. */
  readonly path: string;
}

export function Checks() {
  const q = QUALITY;
  const int = (value: number | null) => measured(value, fmtInt);
  const checks: readonly Check[] = [
    {
      heading: t("Formulas"),
      text: t(
        "{formulas} generated formulas: {agreement} agree with {libreoffice}. The rest is of {classes} explained kinds; {unexplained} are unexplained. Of {more} others, {ironcalc} agree with IronCalc and {formualizer} with Formualizer.",
        {
          formulas: int(q.diffFormulas),
          agreement: measured(q.diffAgreement, percent),
          libreoffice: q.libreOffice ?? "LibreOffice",
          classes: int(q.diffClasses),
          unexplained: int(q.diffUnexplained),
          more: int(q.rustFormulas),
          ironcalc: int(q.ironcalcMatches),
          formualizer: int(q.formualizerMatches),
        },
      ),
      path: "crates/kernel-bench",
    },
    {
      heading: t("Arithmetic"),
      text: t("{cases} cases recorded in Excel 16.0 test comparing, rounding and MOD.", {
        cases: fmtInt(q.excelNumericCases),
      }),
      path: "spikes/excel",
    },
    {
      heading: t("Sync"),
      text: t(
        "Lost and reordered messages, crashes and restored backups, in one seeded world: {invariants} invariants, {ci} seeds on each push, {nightly} each night.",
        {
          invariants: q.simInvariants,
          ci: fmtInt(q.simCiSeeds),
          nightly: fmtInt(q.simNightlySeeds),
        },
      ),
      path: "crates/sim",
    },
    {
      heading: t("Arbitrary input"),
      text: t("{targets} fuzz targets run each night.", { targets: q.fuzzTargets }),
      path: "fuzz/fuzz_targets",
    },
    {
      heading: t("The same bits"),
      text: t(
        "The kernel's self-check gives {digest} on x86-64 Windows and Linux, on Arm Linux, and in three browsers.",
        { digest: q.determinismDigest },
      ),
      path: "tests/determinism",
    },
    {
      heading: t("Older files"),
      text: t("{files} frozen files in {generations} formats are read and replayed on each push.", {
        files: q.compatFiles,
        generations: q.compatGenerations,
      }),
      path: "tests/compat",
    },
    {
      heading: t("Pictures"),
      text: t("{cases} pictures from both renderers are compared pixel by pixel.", {
        cases: q.visualCases,
      }),
      path: "tests/visual",
    },
    {
      heading: t("Browsers"),
      text: t("Chromium, Firefox and WebKit on each push, Safari each night."),
      path: "tests/e2e",
    },
  ];
  return (
    <section id="checks" className="landing-section" aria-labelledby="checks-heading">
      <h2 id="checks-heading">{t("How it is checked")}</h2>
      <div className="landing-rows">
        {checks.map(({ heading, text, path }, index) => (
          <article key={path}>
            <h3>{heading}</h3>
            <div>
              <p>{text}</p>
              {index === 0 && <Source of={PROVENANCE.diff} ran="anywhere" />}
            </div>
            <a href={sourceUrl(path)} {...OUTSIDE}>
              <code>{path}</code>
            </a>
          </article>
        ))}
      </div>
    </section>
  );
}
