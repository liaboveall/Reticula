import { describe, expect, it } from "vitest";
import { placeholders, scanModule, slots } from "./scan.ts";

const IMPORT = 'import { t, tn } from "./i18n/index.ts"; import { tx } from "./i18n/react.ts";\n';

function scan(body: string) {
  return scanModule("module.tsx", IMPORT + body);
}

const bare = (body: string) => scan(body).bareTexts.map(({ text }) => text);
const misuses = (body: string) => scan(body).misuses.map(({ text }) => text);
const keys = (body: string) => scan(body).calls.map(({ key }) => key);

describe("calls", () => {
  it("are found with their texts, values and places", () => {
    const { calls, misuses } = scan(
      [
        "const f = (n) => [",
        '  t("a {x}", { x: 1, "y": 2 }),',
        "  t(`b`),",
        '  tn(n, "{n} c", "{n} cs"),',
        '  tx("<0>d</0>", { 0: <b /> }, params),',
        "];",
      ].join("\n"),
    );
    expect(misuses).toEqual([]);
    expect(calls).toEqual([
      { kind: "t", key: "a {x}", singular: null, params: ["x", "y"], slots: [], line: 3 },
      { kind: "t", key: "b", singular: null, params: [], slots: [], line: 4 },
      { kind: "tn", key: "{n} cs", singular: "{n} c", params: [], slots: [], line: 5 },
      { kind: "tx", key: "<0>d</0>", singular: null, params: null, slots: ["0"], line: 6 },
    ]);
  });

  it("are found under other names, and only if they are of the module", () => {
    const renamed = scanModule(
      "module.ts",
      'import { t as text } from "../i18n/index.ts";\nconst f = () => text("a");',
    );
    expect(renamed.calls.map(({ key }) => key)).toEqual(["a"]);
    const near = scanModule(
      "src/i18n/switch.ts",
      'import { t } from "./index.ts";\nconst f = () => t("a");',
    );
    expect(near.calls.map(({ key }) => key)).toEqual(["a"]);
    const other = scanModule("module.ts", 'import { t } from "./other.ts";\nconst f = () => t(x);');
    expect(other.calls).toEqual([]);
    expect(other.misuses).toEqual([]);
    expect(keys('const f = () => [a.t("b"), { t: 1 }, a.t];')).toEqual([]);
    expect(misuses('const f = () => [a.t("b"), { t: 1 }, a.t];')).toEqual([]);
  });

  it("need literals", () => {
    expect(misuses("const f = (x) => t(x);")).toHaveLength(1);
    expect(misuses(`const f = (x) => t(\`a \${x}\`);`)).toHaveLength(1);
    expect(misuses('const f = (x) => t(x ? "a" : "b");')).toHaveLength(1);
    expect(misuses('const f = (n) => tn(n, "a", b);')).toHaveLength(1);
    expect(misuses('const f = (n) => tn(n, a, "b");')).toHaveLength(1);
  });

  it("belong into functions", () => {
    expect(misuses('const A = t("a");')).toHaveLength(1);
    expect(misuses('const A = [{ name: t("a") }];')).toHaveLength(1);
    expect(misuses('class A { static b = t("a"); }')).toHaveLength(1);
    expect(misuses('class A { static { t("a"); } }')).toHaveLength(1);
    expect(misuses('class A { b = t("a"); c() { return t("d"); } }')).toEqual([]);
    expect(misuses('const a = () => t("a"); function b(c = t("c")) {}')).toEqual([]);
  });

  it("are the only use of the functions", () => {
    expect(misuses("const f = () => list.map(t);")).toHaveLength(1);
    expect(misuses("const other = t;")).toHaveLength(1);
    expect(misuses("const f = (t) => 1;")).toHaveLength(1);
    expect(misuses("const f = () => ({ t });")).toHaveLength(1);
    const whole = 'import * as i18n from "./i18n/index.ts";';
    expect(scanModule("module.ts", whole).misuses).toHaveLength(1);
    const again = 'export { t } from "./i18n/index.ts";';
    expect(scanModule("module.ts", again).misuses).toHaveLength(1);
    const later = 'const f = () => import("./i18n/index.ts");';
    expect(scanModule("module.ts", later).misuses).toHaveLength(1);
  });

  it("are not stored, sent or used as names of files", () => {
    for (const statement of [
      'createWorkbook({ title: t("a") })',
      'client.send({ kind: "x", list: [t("a")] })',
      'ref.current?.sendHost({ title: x || t("a") })',
      'downloadArrayBuffer(t("a"), bytes)',
      'triggerBrowserDownload(t("a"), bytes)',
      'onCreateDoc(t("a"), "empty")',
      'a.download = t("a")',
    ]) {
      expect(misuses(`const f = () => { ${statement}; };`), statement).toHaveLength(1);
    }
    for (const statement of [
      'setNotice(t("a"))',
      'onImportNotice(t("a"))',
      'send({ done: () => setNotice(t("a")) })',
      'other(t("a"))',
    ]) {
      expect(misuses(`const f = () => { ${statement}; };`), statement).toEqual([]);
    }
  });
});

describe("bare texts", () => {
  it("are found in elements", () => {
    expect(bare("const f = () => <p>Some text</p>;")).toEqual(["Some text"]);
    expect(bare("const f = () => <p>× 1,000 → “”</p>;")).toEqual([]);
    expect(bare("const f = () => <p>文字</p>;")).toEqual(["文字"]);
    expect(bare(`const f = (x) => <p>{x ? "a" : \`b \${x}\`}{x && "c"}{x ?? "d"}</p>;`)).toEqual([
      "a",
      "b {}",
      "c",
      "d",
    ]);
    expect(bare("const f = () => <>Some text</>;")).toEqual(["Some text"]);
    expect(bare('const f = () => <p>{t("a")} {x}{" · "}</p>;')).toEqual([]);
  });

  it("are found in attributes that are read", () => {
    expect(bare('const f = () => <p title="a" className="b" data-testid="c" />;')).toEqual(["a"]);
    expect(bare(`const f = (x) => <p aria-label={\`a \${x}\`} id={\`b\${x}\`} />;`)).toEqual([
      "a {}",
    ]);
    expect(bare('const f = (x) => <p placeholder={x ? "a" : t("b")} />;')).toEqual(["a"]);
    expect(bare('const f = () => <img alt="a" /> || <option label="b" />;')).toEqual(["a", "b"]);
    expect(bare('const f = () => <p title={t("a")} />;')).toEqual([]);
  });

  it("are found where they are handed to what shows them", () => {
    expect(bare('const f = () => setError("a");')).toEqual(["a"]);
    expect(bare('const f = () => setCloudError(x instanceof Error ? x.message : "a");')).toEqual([
      "a",
    ]);
    expect(bare(`const f = () => props.onImportNotice(\`a \${x}\`);`)).toEqual(["a {}"]);
    expect(bare('const f = () => setError(t("a"));')).toEqual([]);
    expect(bare("const f = () => setError(null);")).toEqual([]);
    expect(bare('const A = [{ label: "a", id: "b" }];')).toEqual(["a"]);
  });

  it("are found where elements are given them one by one", () => {
    expect(bare('const f = (e) => e.setAttribute("aria-label", "a");')).toEqual(["a"]);
    expect(bare('const f = (e) => e.setAttribute("class", "a");')).toEqual([]);
    expect(bare('const f = (e) => { e.textContent = "a"; e.className = "b"; };')).toEqual(["a"]);
    expect(bare('const f = () => ({ "aria-label": "a", "data-x": "b" });')).toEqual(["a"]);
    expect(bare(`const f = (x) => window.confirm(\`delete \${x}?\`);`)).toEqual(["delete {}?"]);
    expect(bare('const f = (e) => { e.textContent = t("a"); e.title = x; };')).toEqual([]);
  });

  it("are found wherever they read like English", () => {
    expect(bare('const f = () => { return "Something went wrong"; };')).toEqual([
      "Something went wrong",
    ]);
    expect(bare(`const f = (x) => \`Deleted \${x}.\`;`)).toEqual(["Deleted {}."]);
    expect(bare('const f = () => fallback(e, "Could not load");')).toEqual(["Could not load"]);
    expect(
      bare('const a = "toolbar-btn primary"; const b = "M 2 3 L"; const c = "Escape";'),
    ).toEqual([]);
    expect(bare('const f = () => { throw new Error("Could not load"); };')).toEqual([]);
    expect(bare('const f = () => console.warn("Could not load");')).toEqual([]);
    expect(bare('const f = () => t("Could not load {x}", { x: "Some name" });')).toEqual([
      "Some name",
    ]);
  });

  it("are found once", () => {
    expect(bare('const f = () => <p title="Some text">{"Other text"}</p>;')).toEqual([
      "Other text",
      "Some text",
    ]);
  });
});

describe("imports", () => {
  it("are listed without types", () => {
    const { staticImports, dynamicImports } = scanModule(
      "module.ts",
      [
        'import a from "./a.ts";',
        'import type { B } from "./b.ts";',
        'import { type C, c } from "./c.ts";',
        'export * from "./d.ts";',
        'export type { E } from "./e.ts";',
        'const f = () => import("./f.ts");',
      ].join("\n"),
    );
    expect(staticImports).toEqual(["./a.ts", "./c.ts", "./d.ts"]);
    expect(dynamicImports).toEqual(["./f.ts"]);
  });

  it("are not guessed from what cannot be parsed", () => {
    expect(() => scanModule("module.ts", "const = ;")).toThrow("module.ts");
  });
});

describe("parts of texts", () => {
  it("are listed in order", () => {
    expect(placeholders("{b} {a} {b} {} { c }")).toEqual(["a", "b"]);
    expect(slots("<1/> <0>a</0> <b> </0")).toEqual(["</0>", "<0>", "<1/>"]);
  });
});
