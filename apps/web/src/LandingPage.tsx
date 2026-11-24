import { useEffect } from "react";
import { RENDERER_COMPARISON } from "./generated/benchmarks.ts";
import { t } from "./i18n/index.ts";
import { Architecture } from "./landing/Architecture.tsx";
import { ARTICLES, Articles } from "./landing/Articles.tsx";
import { Checks } from "./landing/Checks.tsx";
import { Footer } from "./landing/Footer.tsx";
import { Header } from "./landing/Header.tsx";
import { Hero, pitch } from "./landing/Hero.tsx";
import { Limits } from "./landing/Limits.tsx";
import { Numbers } from "./landing/Numbers.tsx";
import { prefetchEditorWhenIdle } from "./landing/prefetch.ts";
import { Renderers } from "./landing/Renderers.tsx";
import { SheetTabs } from "./landing/SheetTabs.tsx";
import { Tour } from "./landing/Tour.tsx";
import "./landing/landing.css";

export interface LandingPageProps {
  readonly onNavigate: (path: string) => void;
}

export function LandingPage({ onNavigate }: LandingPageProps) {
  useEffect(prefetchEditorWhenIdle, []);

  // What index.html has is in English.
  useEffect(() => {
    document.querySelector('meta[name="description"]')?.setAttribute("content", pitch().join(" "));
  });

  return (
    <div className="landing" data-testid="landing-page">
      <Header onNavigate={onNavigate} />
      <SheetTabs
        sections={[
          ["overview", t("Overview")],
          ["try", t("Try it")],
          ["numbers", t("Numbers")],
          ...(RENDERER_COMPARISON ? ([["renderers", t("Renderers")]] as const) : []),
          ["architecture", t("Architecture")],
          ["checks", t("Checks")],
          ["limits", t("Limits")],
        ]}
      />
      <main className="landing-main">
        <Hero onNavigate={onNavigate} />
        <Tour onNavigate={onNavigate} />
        <Numbers />
        <Renderers data={RENDERER_COMPARISON} />
        <Architecture />
        <Checks />
        <Limits />
        <Articles articles={ARTICLES} />
      </main>
      <Footer onNavigate={onNavigate} />
    </div>
  );
}
