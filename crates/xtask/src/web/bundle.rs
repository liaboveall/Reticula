//! What the pages of the production build download before they can be used:
//! the static closures of their chunks, the inline script that preloads them
//! and the budgets they are held to.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::util::{self, Result, node};

/// The manifest's name for the document, and so for the entry chunk.
const ENTRY: &str = "index.html";
/// The page whose first screen includes the engine's worker.
const EDITOR: &str = "app";
const LANDING: &str = "landing";
/// The pages by the names `apps/web/src/routes.ts` gives them, with the modules
/// `routes-load.ts` loads for them.
const PAGES: [(&str, &str); 7] = [
    (LANDING, "src/LandingPage.tsx"),
    (EDITOR, "src/EditorPage.tsx"),
    ("workbooks", "src/pages/WorkbooksPage.tsx"),
    ("account", "src/pages/AccountPage.tsx"),
    ("privacy", "src/pages/PrivacyPage.tsx"),
    ("sim", "src/sim/SimLabPage.tsx"),
    ("benchmarks", "src/pages/BenchmarksPage.tsx"),
];
/// The set with what the engine's worker needs.
const ENGINE: &str = "engine";

/// The module with the rule for the language and the texts each page needs.
const LOCALE_MODULE: &str = "apps/web/src/i18n/locale-rule.ts";

/// Where `index.html` wants the inline script.
const BOOT_MARKER: &str = "<!-- boot -->";

/// The inline script. It takes two tables of file names: the sets of the
/// pages, one of which is loaded at once, and further sets, which depend on
/// something else than the page alone. It decides between the pages as
/// `normalizeAppRoute` does, and on the language in the very words of the app,
/// which take the place of `LOCALE_MARKER`.
///
/// Everything is requested the way it will be requested again (module scripts
/// and the worker's `fetch` are CORS requests without credentials for other
/// origins), so that nothing is downloaded twice.
const BOOT_SCRIPT: &str = r#"((p,x)=>{const s={},load=window.__reticulaPreload=k=>{for(const f of p[k]||x[k]||[])if(!s[f]){s[f]=1;const l=document.createElement("link");if(f.endsWith(".wasm")){l.rel="preload";l.as="fetch";l.type="application/wasm"}else l.rel="modulepreload";l.crossOrigin="";l.href="/assets/"+f;document.head.append(l)}},[,a,b]=location.pathname.split("/"),r=a=="join"&&b!=null?"app":Object.hasOwn(p,a)?a:"landing";load(r);if(r=="app")load("engine");if(__LOCALE__!="en")load("zh:"+r)})"#;
const LOCALE_MARKER: &str = "__LOCALE__";

/// Brotli-compressed sizes the build must not exceed, in bytes.
const MAX_EDITOR_JS: u64 = 180_000;
const MAX_LANDING_JS: u64 = 80_000;
const MAX_TOTAL_JS: u64 = 300_000;
const MAX_WEBGPU_JS: u64 = 16_000;
const MAX_ZH_JS: u64 = 40_000;
const MAX_CORE_WASM: u64 = 1_200_000;
const MAX_SIM_WASM: u64 = 800_000;
const MAX_XLSX_WASM: u64 = 900_000;

/// Chunks that are only ever loaded on demand, by the start of their names.
/// `vite.config.ts` names the first; the second are named after their modules.
const WEBGPU_PREFIX: &str = "webgpu-";
const ZH_PREFIX: &str = "zh-";

/// One chunk of the main build, as Vite's manifest describes it.
struct Chunk {
    file: String,
    imports: Vec<String>,
    dynamic_imports: Vec<String>,
}

/// The chunks of the main build by their names in Vite's manifest.
struct Manifest(BTreeMap<String, Chunk>);

impl Manifest {
    fn parse(json: &str) -> Result<Self> {
        let entries: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(json).map_err(|e| format!("cannot parse Vite's manifest: {e}"))?;
        let names = |value: &serde_json::Value| -> Vec<String> {
            let names = value.as_array().into_iter().flatten();
            names
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect()
        };
        entries
            .into_iter()
            .map(|(key, entry)| {
                let Some(file) = entry["file"].as_str() else {
                    return Err(format!("`{key}` has no file in Vite's manifest"));
                };
                let chunk = Chunk {
                    file: file.to_owned(),
                    imports: names(&entry["imports"]),
                    dynamic_imports: names(&entry["dynamicImports"]),
                };
                Ok((key, chunk))
            })
            .collect::<Result<_>>()
            .map(Self)
    }

    fn chunk(&self, key: &str) -> Result<&Chunk> {
        self.0
            .get(key)
            .ok_or_else(|| format!("`{key}` is not a chunk of the build"))
    }

    /// The scripts that must be there before a chunk can run: its own and, in
    /// turn, the ones each imports statically. Names are relative to `assets/`.
    fn closure(&self, key: &str) -> Result<BTreeSet<String>> {
        let mut files = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut open = vec![key];
        while let Some(key) = open.pop() {
            if !seen.insert(key) {
                continue;
            }
            let chunk = self.chunk(key)?;
            files.insert(asset_name(&chunk.file)?.to_owned());
            open.extend(chunk.imports.iter().map(String::as_str));
        }
        Ok(files)
    }
}

/// The name of a file under `assets/`. The inline script spells these names
/// out, so they must not need escaping.
fn asset_name(file: &str) -> Result<&str> {
    let name = file
        .strip_prefix("assets/")
        .ok_or_else(|| format!("the build put `{file}` outside assets/"))?;
    if name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Ok(name)
    } else {
        Err(format!("unexpected characters in the file name `{name}`"))
    }
}

/// The scripts a built script imports statically, by the names it uses.
///
/// The worker's bundle is not in the manifest, so this reads the script
/// itself: every `from"./x.js"` and `import"./x.js"`. `import("./x.js")` does not
/// match, because a parenthesis comes before the name. A string with such a
/// text in it would match, which errs on the safe side.
fn static_imports(source: &str) -> BTreeSet<&str> {
    let mut found = BTreeSet::new();
    for quote in ['"', '\''] {
        let opening = format!("{quote}./");
        for (start, _) in source.match_indices(&opening) {
            let (before, after) = (&source[..start], &source[start + opening.len()..]);
            let Some((name, _)) = after.split_once(quote) else {
                continue;
            };
            let before = before.trim_end();
            let keyword = ["from", "import"].into_iter().any(|keyword| {
                before.strip_suffix(keyword).is_some_and(|rest| {
                    !rest.ends_with(|c: char| c.is_alphanumeric() || matches!(c, '_' | '$' | '.'))
                })
            });
            if keyword && has_extension(name, "js") {
                found.insert(name);
            }
        }
    }
    found
}

/// The static closure of a script outside the manifest. `read` gives the text
/// of a file under `assets/`.
fn script_closure(
    first: &str,
    mut read: impl FnMut(&str) -> Result<String>,
) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    let mut open = vec![first.to_owned()];
    while let Some(name) = open.pop() {
        if files.contains(&name) {
            continue;
        }
        let source = read(&name)?;
        open.extend(static_imports(&source).into_iter().map(str::to_owned));
        files.insert(name);
    }
    Ok(files)
}

fn has_extension(name: &str, extension: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|found| found.eq_ignore_ascii_case(extension))
}

/// What the app's own module says about languages. Nothing of it is written
/// down a second time here.
struct Languages {
    /// The source of the function that decides on the language.
    rule: String,
    /// The key under which the browser keeps the user's choice.
    key: String,
    /// The modules with the Chinese texts each page waits for.
    catalogues: BTreeMap<String, Vec<String>>,
}

impl Languages {
    /// Has Node.js read the module, which takes the types out of it.
    fn read() -> Result<Self> {
        const SCRIPT: &str = "\
const { pathToFileURL } = require('node:url');
import(pathToFileURL(process.argv[1])).then(({ detectLocale, LOCALE_KEY, CATALOGUES }) =>
  console.log(JSON.stringify([detectLocale.toString(), LOCALE_KEY, CATALOGUES])));";
        Self::parse(&node().args(["--eval", SCRIPT, LOCALE_MODULE]).output()?)
    }

    fn parse(json: &str) -> Result<Self> {
        let (rule, key, catalogues) =
            serde_json::from_str(json).map_err(|e| format!("cannot read {LOCALE_MODULE}: {e}"))?;
        Ok(Self {
            rule,
            key,
            catalogues,
        })
    }

    /// An expression for the language, on one line.
    fn expression(&self) -> Result<String> {
        let rule = self.rule.split_whitespace().collect::<Vec<_>>().join(" ");
        for (what, text) in [
            ("a comment", "//"),
            ("a comment", "/*"),
            ("an end tag", "</"),
        ] {
            if rule.contains(text) {
                return Err(format!(
                    "the rule in {LOCALE_MODULE} has {what}, which the inline script cannot contain"
                ));
            }
        }
        let key = serde_json::to_string(&self.key)
            .map_err(|e| format!("serialize the key of the language: {e}"))?;
        Ok(format!("({rule})(self,{key})"))
    }
}

/// What a build's pages need, worked out from its manifest and its files.
pub struct Closures {
    /// The entry chunk and what it imports, which every page starts with.
    entry: BTreeSet<String>,
    /// Each page's chunk and what it imports, by the page's name.
    pages: BTreeMap<&'static str, BTreeSet<String>>,
    worker: BTreeSet<String>,
    engine_wasm: String,
    /// The chunks with the Chinese texts of each page, by the page's name.
    texts: BTreeMap<&'static str, BTreeSet<String>>,
    languages: Languages,
}

impl Closures {
    /// Reads and removes the manifest, which is neither served nor compressed.
    pub fn take(dist: &Path) -> Result<Self> {
        let directory = dist.join(".vite");
        let path = directory.join("manifest.json");
        let json = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        fs::remove_dir_all(&directory)
            .map_err(|e| format!("cannot remove {}: {e}", directory.display()))?;

        let assets = dist.join("assets");
        let names = file_names(&assets)?;
        Self::new(
            &Manifest::parse(&json)?,
            &names,
            Languages::read()?,
            |name| {
                let path = assets.join(name);
                fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))
            },
        )
    }

    fn new(
        manifest: &Manifest,
        names: &[String],
        languages: Languages,
        read: impl FnMut(&str) -> Result<String>,
    ) -> Result<Self> {
        let loaded_on_demand = &manifest.chunk(ENTRY)?.dynamic_imports;
        if let Some(module) = loaded_on_demand.iter().find(|module| {
            module.ends_with("Page.tsx") && PAGES.iter().all(|(_, known)| known != module)
        }) {
            return Err(format!(
                "`{module}` is missing in the list of pages: nothing would preload it"
            ));
        }
        let entry = manifest.closure(ENTRY)?;
        let mut pages = BTreeMap::new();
        let mut texts = BTreeMap::new();
        for (page, module) in PAGES {
            if !loaded_on_demand.iter().any(|key| key == module) {
                return Err(format!(
                    "the entry does not load `{module}`: the list of pages is out of date"
                ));
            }
            pages.insert(page, manifest.closure(module)?);
            let Some(catalogues) = languages.catalogues.get(page) else {
                return Err(format!("{LOCALE_MODULE} has no texts for the {page} page"));
            };
            let mut files = BTreeSet::new();
            for catalogue in catalogues {
                files.extend(manifest.closure(&format!("src/i18n/{catalogue}.ts"))?);
            }
            texts.insert(page, files.difference(&entry).cloned().collect());
        }
        let worker = only(names, "worker-", "js")?;
        Ok(Self {
            entry,
            pages,
            worker: script_closure(worker, read)?,
            engine_wasm: only(names, "reticula_wasm_bg", "wasm")?.to_owned(),
            texts,
            languages,
        })
    }

    /// The scripts of a page's first screen.
    fn first_screen(&self, page: &str) -> BTreeSet<&str> {
        let mut scripts: BTreeSet<&str> = self.entry.iter().map(String::as_str).collect();
        scripts.extend(
            self.pages
                .get(page)
                .into_iter()
                .flatten()
                .map(String::as_str),
        );
        if page == EDITOR {
            scripts.extend(self.worker.iter().map(String::as_str));
        }
        scripts
    }

    /// The inline script, with the files of every page apart from the ones the
    /// document itself names.
    fn boot_script(&self) -> Result<String> {
        let pages: BTreeMap<&str, Vec<&String>> = self
            .pages
            .iter()
            .map(|(&page, files)| (page, files.difference(&self.entry).collect()))
            .collect();
        let mut engine: Vec<&String> = self.worker.iter().collect();
        engine.push(&self.engine_wasm);
        let mut others = BTreeMap::from([(String::from(ENGINE), engine)]);
        for (page, files) in &self.texts {
            others.insert(format!("zh:{page}"), files.iter().collect());
        }

        let error = |e| format!("serialize preload sets: {e}");
        let pages = serde_json::to_string(&pages).map_err(error)?;
        let others = serde_json::to_string(&others).map_err(error)?;
        let code = BOOT_SCRIPT.replace(LOCALE_MARKER, &self.languages.expression()?);
        Ok(format!("<script>{code}({pages},{others})</script>"))
    }
}

/// The names of the files in a directory, in order.
fn file_names(directory: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in
        fs::read_dir(directory).map_err(|e| format!("cannot read {}: {e}", directory.display()))?
    {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// The one file whose name starts and ends like this.
fn only<'a>(names: &'a [String], prefix: &str, extension: &str) -> Result<&'a str> {
    let mut found = names
        .iter()
        .filter(|name| name.starts_with(prefix) && has_extension(name, extension));
    match (found.next(), found.next()) {
        (Some(name), None) => Ok(name),
        (None, _) => Err(format!("no `{prefix}*.{extension}` in dist/assets/")),
        (Some(_), Some(_)) => Err(format!("several `{prefix}*.{extension}` in dist/assets/")),
    }
}

/// Puts the inline script into `dist/index.html`.
pub fn inject_boot_script(dist: &Path, closures: &Closures) -> Result {
    let path = dist.join(ENTRY);
    let html =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let html = insert_boot_script(&html, &closures.boot_script()?)?;
    fs::write(&path, html).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// A script that comes after a style sheet waits for it, so the inline script
/// comes before the first, and before any other script.
fn insert_boot_script(html: &str, script: &str) -> Result<String> {
    let Some((before, after)) = html.split_once(BOOT_MARKER) else {
        return Err(format!("no `{BOOT_MARKER}` in {ENTRY}"));
    };
    for (what, text) in [("script", "<script"), ("style sheet", "stylesheet")] {
        if before.contains(text) {
            return Err(format!(
                "a {what} comes before the inline script in {ENTRY}"
            ));
        }
    }
    Ok(format!("{before}{script}{after}"))
}

/// A size and its limit; a size of nothing is only right for what may not exist.
struct Budget {
    label: &'static str,
    used: u64,
    limit: u64,
    optional: bool,
}

impl Budget {
    fn check(&self) -> Result {
        let Self {
            label, used, limit, ..
        } = self;
        if *used > *limit {
            return Err(format!("{label}: {used} B exceeds its budget of {limit} B"));
        }
        if *used == 0 && !self.optional {
            return Err(format!("{label}: nothing of the kind was built"));
        }
        Ok(())
    }
}

/// Prints the brotli-compressed sizes that have a budget, and checks them all.
pub fn check_budgets(dist: &Path, closures: &Closures) -> Result {
    util::step("bundle size budgets (brotli)");
    let assets = dist.join("assets");
    let mut sizes = BTreeMap::new();
    for name in file_names(&assets)? {
        if let Some(original) = name.strip_suffix(".br") {
            let size = file_size(&assets.join(&name))?;
            sizes.insert(original.to_owned(), size);
        }
    }
    let service_worker = file_size(&dist.join("sw.js.br"))?;
    let budgets = budgets(&sizes, service_worker, closures)?;
    for budget in &budgets {
        let Budget {
            label, used, limit, ..
        } = budget;
        if *used == 0 && budget.optional {
            println!("  {label:<26} {:>9} / {limit:>9} bytes", "none");
        } else {
            let share = *used as f64 / *limit as f64 * 100.0;
            println!("  {label:<26} {used:>9} / {limit:>9} bytes ({share:.1}%)");
        }
    }
    budgets.iter().try_for_each(Budget::check)
}

fn file_size(path: &Path) -> Result<u64> {
    fs::metadata(path)
        .map(|meta| meta.len())
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// `sizes` are the compressed sizes of the files under `assets/`, by their
/// uncompressed names.
fn budgets(
    sizes: &BTreeMap<String, u64>,
    service_worker: u64,
    closures: &Closures,
) -> Result<Vec<Budget>> {
    let first_screen = |page: &str| -> Result<u64> {
        let scripts = closures.first_screen(page);
        if let Some(script) = scripts
            .iter()
            .find(|name| name.starts_with(WEBGPU_PREFIX) || name.starts_with(ZH_PREFIX))
        {
            return Err(format!(
                "`{script}` is to be loaded on demand, but the {page} page needs it to start"
            ));
        }
        scripts.into_iter().try_fold(0, |sum: u64, name| {
            let size = sizes
                .get(name)
                .ok_or_else(|| format!("`{name}` has no compressed form"))?;
            Ok(sum.saturating_add(*size))
        })
    };
    let total = |prefix: &str, extension: &str| -> u64 {
        sizes
            .iter()
            .filter(|(name, _)| name.starts_with(prefix) && has_extension(name, extension))
            .map(|(_, size)| size)
            .sum()
    };
    let required = |label, used, limit| Budget {
        label,
        used,
        limit,
        optional: false,
    };
    let optional = |label, used, limit| Budget {
        label,
        used,
        limit,
        optional: true,
    };
    Ok(vec![
        required(
            "editor first-screen JS",
            first_screen(EDITOR)?,
            MAX_EDITOR_JS,
        ),
        required(
            "landing first-screen JS",
            first_screen(LANDING)?,
            MAX_LANDING_JS,
        ),
        optional(
            "WebGPU renderer JS",
            total(WEBGPU_PREFIX, "js"),
            MAX_WEBGPU_JS,
        ),
        optional("Chinese text JS", total(ZH_PREFIX, "js"), MAX_ZH_JS),
        required(
            "total JS",
            total("", "js").saturating_add(service_worker),
            MAX_TOTAL_JS,
        ),
        required("core WASM", total("reticula_wasm", "wasm"), MAX_CORE_WASM),
        required("sim WASM", total("reticula_sim_wasm", "wasm"), MAX_SIM_WASM),
        required(
            "xlsx WASM",
            total("reticula_xlsx_wasm", "wasm"),
            MAX_XLSX_WASM,
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build in which the editor shares a chunk with the landing page, loads
    /// a dialog on demand, and has a worker with an import of its own. The
    /// Chinese texts are loaded on demand, and there are more of them for the
    /// pages other than the editor.
    const MANIFEST: &str = r#"{
        "_shared-S.js": { "file": "assets/shared-S.js", "imports": ["index.html"] },
        "index.html": {
            "file": "assets/index-I.js", "src": "index.html", "isEntry": true,
            "imports": ["_runtime-R.js"],
            "dynamicImports": [
                "src/LandingPage.tsx", "src/EditorPage.tsx", "src/pages/WorkbooksPage.tsx",
                "src/pages/AccountPage.tsx", "src/pages/PrivacyPage.tsx",
                "src/sim/SimLabPage.tsx", "src/pages/BenchmarksPage.tsx"
            ],
            "css": ["assets/index-C.css"]
        },
        "_runtime-R.js": { "file": "assets/runtime-R.js" },
        "src/LandingPage.tsx": {
            "file": "assets/LandingPage-L.js", "imports": ["_shared-S.js", "index.html"]
        },
        "src/EditorPage.tsx": {
            "file": "assets/EditorPage-E.js", "imports": ["index.html", "_deep-D.js"],
            "dynamicImports": ["src/LocalModals.tsx"]
        },
        "_deep-D.js": { "file": "assets/deep-D.js", "imports": ["_shared-S.js"] },
        "src/LocalModals.tsx": {
            "file": "assets/LocalModals-M.js", "imports": ["src/EditorPage.tsx"]
        },
        "src/pages/WorkbooksPage.tsx": { "file": "assets/WorkbooksPage-W.js" },
        "src/pages/AccountPage.tsx": { "file": "assets/AccountPage-A.js" },
        "src/pages/PrivacyPage.tsx": { "file": "assets/PrivacyPage-P.js" },
        "src/sim/SimLabPage.tsx": { "file": "assets/SimLabPage-X.js" },
        "src/pages/BenchmarksPage.tsx": { "file": "assets/BenchmarksPage-B.js" },
        "src/i18n/zh-core.ts": {
            "file": "assets/zh-core-Z.js", "imports": ["_runtime-R.js"]
        },
        "src/i18n/zh-pages.ts": { "file": "assets/zh-pages-Z.js" }
    }"#;

    fn languages() -> Languages {
        let catalogues = PAGES
            .iter()
            .map(|&(page, _)| {
                let names: &[&str] = if page == EDITOR {
                    &["zh-core"]
                } else {
                    &["zh-core", "zh-pages"]
                };
                (
                    page.to_owned(),
                    names.iter().map(|&name| name.to_owned()).collect(),
                )
            })
            .collect();
        Languages {
            rule: String::from("function rule(host          , key)  {\n  return \"en\";\n}"),
            key: String::from("locale"),
            catalogues,
        }
    }

    fn built(manifest: &str) -> Result<Closures> {
        built_with(manifest, languages())
    }

    fn built_with(manifest: &str, languages: Languages) -> Result<Closures> {
        let names = [
            "reticula_wasm_bg-K.wasm",
            "reticula_sim_wasm_bg-K.wasm",
            "worker-W.js",
        ]
        .map(String::from);
        Closures::new(
            &Manifest::parse(manifest)?,
            &names,
            languages,
            |name| match name {
                "worker-W.js" => Ok(String::from(
                    "import{a as b}from\"./codec-C.js\";b(()=>import(`./on-demand-O.js`))",
                )),
                "codec-C.js" => Ok(String::from("export{a};")),
                other => Err(format!("`{other}` is not needed to start the worker")),
            },
        )
    }

    fn names(files: &[&str]) -> BTreeSet<String> {
        files.iter().map(|&file| file.to_owned()).collect()
    }

    /// Sizes for which every budget but the first screens' is met with room to spare.
    fn sizes(closures: &Closures) -> BTreeMap<String, u64> {
        let mut sizes = BTreeMap::new();
        for page in closures.pages.keys() {
            for script in closures.first_screen(page) {
                sizes.insert(script.to_owned(), 1_000);
            }
        }
        for module in ["reticula_wasm", "reticula_sim_wasm", "reticula_xlsx_wasm"] {
            sizes.insert(format!("{module}_bg-K.wasm"), 1_000);
        }
        sizes
    }

    fn used(budgets: &[Budget], label: &str) -> u64 {
        budgets
            .iter()
            .find(|budget| budget.label == label)
            .map(|budget| budget.used)
            .unwrap()
    }

    #[test]
    fn closures_follow_static_imports_only() {
        let manifest = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(
            manifest.closure("src/EditorPage.tsx").unwrap(),
            names(&[
                "EditorPage-E.js",
                "deep-D.js",
                "index-I.js",
                "runtime-R.js",
                "shared-S.js"
            ])
        );
        assert_eq!(
            manifest.closure(ENTRY).unwrap(),
            names(&["index-I.js", "runtime-R.js"])
        );
        assert!(manifest.closure("src/Gone.tsx").is_err());
    }

    #[test]
    fn a_chunk_that_cannot_be_named_in_the_document_is_an_error() {
        let odd = |file: &str| {
            let json = format!(r#"{{ "x": {{ "file": "{file}" }} }}"#);
            Manifest::parse(&json).unwrap().closure("x")
        };
        assert!(odd("assets/fine-1_a.js").is_ok());
        assert!(odd("elsewhere/x.js").is_err());
        assert!(odd("assets/</script>.js").is_err());
        assert!(Manifest::parse(r#"{ "x": {} }"#).is_err());
        assert!(Manifest::parse("[]").is_err());
    }

    #[test]
    fn static_imports_are_told_from_dynamic_ones() {
        let source = concat!(
            "import{a as b}from\"./one.js\";import\"./two.js\";import c from './three.js';",
            "export * from  \"./four.js\";",
            "const d=()=>import(\"./five.js\");e(()=>import(`./six.js`));",
            "f.from(\"./seven.js\");g=\"./eight.js\";h(\"./nine.css\");import\"./ten.css\";",
            "new URL(\"./eleven.js\",import.meta.url);$from\"./twelve.js\"",
        );
        assert_eq!(
            static_imports(source).into_iter().collect::<Vec<_>>(),
            ["four.js", "one.js", "three.js", "two.js"]
        );
    }

    #[test]
    fn a_script_closure_reads_every_file_once_and_survives_cycles() {
        let mut reads = Vec::new();
        let closure = script_closure("a.js", |name| {
            reads.push(name.to_owned());
            Ok(String::from(match name {
                "a.js" => "import\"./b.js\";import\"./c.js\";",
                "b.js" => "import\"./c.js\";import\"./a.js\";",
                _ => "",
            }))
        });
        assert_eq!(closure.unwrap(), names(&["a.js", "b.js", "c.js"]));
        reads.sort();
        assert_eq!(reads, ["a.js", "b.js", "c.js"]);
        assert!(script_closure("a.js", |_| Err(String::from("missing"))).is_err());
    }

    #[test]
    fn the_editor_starts_with_its_worker_and_the_other_pages_do_not() {
        let closures = built(MANIFEST).unwrap();
        assert_eq!(
            closures
                .first_screen(EDITOR)
                .into_iter()
                .collect::<Vec<_>>(),
            [
                "EditorPage-E.js",
                "codec-C.js",
                "deep-D.js",
                "index-I.js",
                "runtime-R.js",
                "shared-S.js",
                "worker-W.js"
            ]
        );
        assert_eq!(
            closures
                .first_screen(LANDING)
                .into_iter()
                .collect::<Vec<_>>(),
            [
                "LandingPage-L.js",
                "index-I.js",
                "runtime-R.js",
                "shared-S.js"
            ]
        );
    }

    #[test]
    fn the_list_of_pages_has_to_match_what_the_entry_loads() {
        let gone = MANIFEST.replace("src/sim/SimLabPage.tsx", "src/sim/Other.tsx");
        assert!(built(&gone).is_err_and(|e| e.contains("SimLabPage")));
        let new = MANIFEST.replace(
            "\"src/LandingPage.tsx\", \"src/EditorPage.tsx\"",
            "\"src/LandingPage.tsx\", \"src/EditorPage.tsx\", \"src/NewPage.tsx\"",
        );
        assert!(built(&new).is_err_and(|e| e.contains("NewPage")));
        // Other things may be loaded on demand as well.
        let more = new.replace("src/NewPage.tsx", "src/i18n/zh-core.ts");
        assert!(built(&more).is_ok());
    }

    #[test]
    fn the_boot_script_lists_what_the_document_does_not() {
        let script = built(MANIFEST).unwrap().boot_script().unwrap();
        assert!(script.starts_with("<script>(("), "{script}");
        assert!(script.ends_with(")</script>"), "{script}");
        assert!(
            script.contains(r#""app":["EditorPage-E.js","deep-D.js","shared-S.js"]"#),
            "{script}"
        );
        assert!(
            script.contains(r#""landing":["LandingPage-L.js","shared-S.js"]"#),
            "{script}"
        );
        assert!(
            script.contains(r#"{"engine":["codec-C.js","worker-W.js","reticula_wasm_bg-K.wasm"],"#),
            "{script}"
        );
        assert!(script.contains(r#""zh:app":["zh-core-Z.js"]"#), "{script}");
        assert!(
            script.contains(r#""zh:landing":["zh-core-Z.js","zh-pages-Z.js"]"#),
            "{script}"
        );
        // The entry and what it imports are in the document itself.
        assert!(!script.contains("index-I.js") && !script.contains("runtime-R.js"));
        assert!(!script.contains("on-demand") && !script.contains("LocalModals"));
    }

    #[test]
    fn the_boot_script_decides_on_the_language_as_the_app_does() {
        let script = built(MANIFEST).unwrap().boot_script().unwrap();
        assert!(
            script.contains(
                r#"if((function rule(host , key) { return "en"; })(self,"locale")!="en")load("zh:"+r)"#
            ),
            "{script}"
        );
        assert!(!script.contains(LOCALE_MARKER), "{script}");

        for rule in [
            "function rule() { // why\n}",
            "function rule() { /* why */ }",
            "function rule() { return \"</script>\"; }",
        ] {
            let languages = Languages {
                rule: rule.to_owned(),
                ..languages()
            };
            let closures = built_with(MANIFEST, languages).unwrap();
            assert!(closures.boot_script().is_err(), "{rule}");
        }
    }

    #[test]
    fn every_page_has_its_texts() {
        let mut incomplete = languages();
        incomplete.catalogues.remove("sim");
        assert!(built_with(MANIFEST, incomplete).is_err_and(|e| e.contains("sim")));
        let mut unknown = languages();
        unknown
            .catalogues
            .insert(String::from("sim"), vec![String::from("zh-gone")]);
        assert!(built_with(MANIFEST, unknown).is_err_and(|e| e.contains("zh-gone")));
        assert!(Languages::parse(r#"["rule","key"]"#).is_err());
        assert!(Languages::parse(r#"["rule","key",{"app":["zh-core"]}]"#).is_ok());
    }

    #[test]
    fn the_boot_script_comes_before_every_script_and_style_sheet() {
        let page = |head: &str| insert_boot_script(head, "<script>boot</script>");
        assert_eq!(
            page("<meta><!-- boot --><script src=x></script><link rel=\"stylesheet\">").unwrap(),
            "<meta><script>boot</script><script src=x></script><link rel=\"stylesheet\">"
        );
        assert!(page("<link rel=\"stylesheet\"><!-- boot -->").is_err());
        assert!(page("<script src=x></script><!-- boot -->").is_err());
        assert!(page("<meta>").is_err());
    }

    #[test]
    fn budgets_add_up_the_closures() {
        let closures = built(MANIFEST).unwrap();
        let mut sizes = sizes(&closures);
        sizes.insert(String::from("LocalModals-M.js"), 10);
        sizes.insert(String::from("zh-core-Z.js"), 3);
        sizes.insert(String::from("zh-pages-Z.js"), 4);
        sizes.insert(String::from("webgpu-G.js"), 5);
        sizes.insert(String::from("index-C.css"), 1_000);
        let budgets = budgets(&sizes, 100, &closures).unwrap();
        assert_eq!(used(&budgets, "editor first-screen JS"), 7_000);
        assert_eq!(used(&budgets, "landing first-screen JS"), 4_000);
        assert_eq!(used(&budgets, "WebGPU renderer JS"), 5);
        assert_eq!(used(&budgets, "Chinese text JS"), 7);
        assert_eq!(used(&budgets, "total JS"), 13_000 + 10 + 7 + 5 + 100);
        assert_eq!(used(&budgets, "core WASM"), 1_000);
        assert!(budgets.iter().try_for_each(Budget::check).is_ok());
    }

    #[test]
    fn budgets_fail_when_exceeded_or_when_something_is_missing() {
        let closures = built(MANIFEST).unwrap();
        let outcome = |change: &dyn Fn(&mut BTreeMap<String, u64>)| {
            let mut sizes = sizes(&closures);
            change(&mut sizes);
            budgets(&sizes, 100, &closures)?
                .iter()
                .try_for_each(Budget::check)
        };
        // The chunks that only exist later are not missed.
        assert_eq!(outcome(&|_| {}), Ok(()));
        for (file, size, label) in [
            ("worker-W.js", MAX_EDITOR_JS, "editor"),
            ("LandingPage-L.js", MAX_LANDING_JS, "landing"),
            ("webgpu-G.js", MAX_WEBGPU_JS + 1, "WebGPU"),
            ("zh-core-Z.js", MAX_ZH_JS + 1, "Chinese"),
            ("SimLabPage-X.js", MAX_TOTAL_JS, "total"),
            ("reticula_wasm_bg-K.wasm", MAX_CORE_WASM + 1, "core"),
            ("reticula_sim_wasm_bg-K.wasm", MAX_SIM_WASM + 1, "sim"),
            ("reticula_xlsx_wasm_bg-K.wasm", MAX_XLSX_WASM + 1, "xlsx"),
        ] {
            let error = outcome(&|sizes| {
                sizes.insert(file.to_owned(), size);
            });
            assert!(error.is_err_and(|e| e.starts_with(label)), "{file}");
        }
        for file in ["EditorPage-E.js", "reticula_sim_wasm_bg-K.wasm"] {
            let error = outcome(&|sizes| {
                sizes.remove(file);
            });
            assert!(error.is_err(), "{file}");
        }
    }

    #[test]
    fn chunks_for_later_may_not_be_part_of_a_first_screen() {
        for name in ["webgpu-G", "zh-core-Z"] {
            let eager = MANIFEST.replace("deep-D", name);
            let closures = built(&eager).unwrap();
            let error = budgets(&sizes(&closures), 100, &closures);
            assert!(error.is_err_and(|e| e.contains(name)));
        }
    }

    #[test]
    fn worker_and_engine_must_be_unambiguous() {
        let names =
            ["worker-A.js", "worker-B.js", "worker-C.js.br", "other.wasm"].map(String::from);
        assert!(only(&names, "worker-", "js").is_err());
        assert_eq!(only(&names[1..], "worker-", "js"), Ok("worker-B.js"));
        assert!(only(&names, "reticula_wasm_bg", "wasm").is_err());
    }
}
