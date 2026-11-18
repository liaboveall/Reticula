//! The front-end half: checks, the production build, preload injection, size
//! budget verification, and end-to-end tests.

use std::fs;
use std::path::{Path, PathBuf};

use crate::util::{self, Result, node, pnpm};
use crate::wasm::report_sizes;

const APP: &str = "@reticula/web";
const DIST: &str = "apps/web/dist";

/// Hard upper bound on brotli-compressed core `.wasm` module (`1.2 MB`, `13-m1a.md` 6.4 & 8.2).
const MAX_CORE_WASM_BROTLI_BYTES: u64 = 1_200_000;
/// Upper bound on brotli-compressed first-screen JS (`index` + `worker`, `13-m1a.md` 6.4).
const MAX_FIRST_SCREEN_JS_BROTLI_BYTES: u64 = 165_000;

/// TypeScript projects, each with its own `tsconfig.json`.
const TS_PROJECTS: [&str; 4] = [
    "packages/protocol",
    "packages/grid",
    "apps/web",
    "tests/e2e",
];

pub fn check() -> Result {
    util::step("biome");
    pnpm().args(["exec", "biome", "ci"]).run()?;

    for project in TS_PROJECTS {
        util::step(&format!("tsc ({project})"));
        pnpm()
            .args(["exec", "tsc", "--noEmit", "--project", project])
            .run()?;
    }
    Ok(())
}

/// The front-end unit tests. They need neither a browser nor the engine.
pub fn test() -> Result {
    util::step("vitest");
    pnpm().args(["exec", "vitest", "run"]).run()
}

pub fn dev() -> Result {
    util::step("vite");
    pnpm().args(["--filter", APP, "exec", "vite"]).run()
}

pub fn build() -> Result {
    util::step("vite build");
    pnpm()
        .args(["--filter", APP, "exec", "vite", "build"])
        .run()?;

    let dist_dir = util::root().join(DIST);
    inject_preloads(&dist_dir)?;

    util::step("precompress");
    // Compression happens here, once, at the highest setting; servers only
    // ever hand out the precompressed files.
    const SCRIPT: &str = "\
const fs = require('node:fs'), path = require('node:path'), zlib = require('node:zlib');
const compressible = new Set(['.html', '.js', '.css', '.wasm', '.json', '.svg', '.txt']);
const walk = (dir) => fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) =>
  entry.isDirectory() ? walk(path.join(dir, entry.name)) : [path.join(dir, entry.name)]);
for (const file of walk(process.argv[1])) {
  if (!compressible.has(path.extname(file))) continue;
  const data = fs.readFileSync(file);
  fs.writeFileSync(file + '.gz', zlib.gzipSync(data, { level: 9 }));
  fs.writeFileSync(file + '.br', zlib.brotliCompressSync(data, { params: {
    [zlib.constants.BROTLI_PARAM_QUALITY]: 11,
    [zlib.constants.BROTLI_PARAM_SIZE_HINT]: data.length,
  } }));
  console.log(file);
}";
    let files = node().args(["--eval", SCRIPT, DIST]).output()?;
    let files: Vec<PathBuf> = files.lines().map(PathBuf::from).collect();
    let refs: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    report_sizes(&refs)?;
    check_budgets(&dist_dir)
}

/// Injects `<link rel="modulepreload">` for the engine worker script and
/// `<link rel="preload" as="fetch" type="application/wasm" crossorigin>` for the
/// hashed `.wasm` binary into `dist/index.html` (`13-m1a.md` 6.4).
fn inject_preloads(dist_dir: &Path) -> Result {
    let assets_dir = dist_dir.join("assets");
    let mut worker_js: Option<String> = None;
    let mut wasm_bin: Option<String> = None;

    for entry in fs::read_dir(&assets_dir)
        .map_err(|e| format!("cannot read {}: {e}", assets_dir.display()))?
    {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with("worker-")
            && Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("js"))
        {
            worker_js = Some(name.to_owned());
        } else if Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wasm"))
        {
            wasm_bin = Some(name.to_owned());
        }
    }

    let worker_js =
        worker_js.ok_or_else(|| String::from("no `worker-*.js` found in dist/assets/"))?;
    let wasm_bin = wasm_bin.ok_or_else(|| String::from("no `*.wasm` found in dist/assets/"))?;

    let index_path = dist_dir.join("index.html");
    let html = fs::read_to_string(&index_path)
        .map_err(|e| format!("cannot read {}: {e}", index_path.display()))?;
    if html.contains("type=\"application/wasm\"") {
        return Ok(());
    }
    let preload_tags = format!(
        "    <link rel=\"modulepreload\" crossorigin href=\"/assets/{worker_js}\">\n\
         \x20   <link rel=\"preload\" as=\"fetch\" type=\"application/wasm\" crossorigin href=\"/assets/{wasm_bin}\">\n  </head>"
    );
    let updated = html.replacen("  </head>", &preload_tags, 1);
    if updated == html {
        return Err(format!(
            "failed to locate `</head>` in {} for preload injection",
            index_path.display()
        ));
    }
    fs::write(&index_path, updated)
        .map_err(|e| format!("cannot write {}: {e}", index_path.display()))
}

fn check_budgets(dist_dir: &Path) -> Result {
    util::step("bundle size budgets (brotli)");
    let assets_dir = dist_dir.join("assets");
    let mut wasm_br: u64 = 0;
    let mut js_br: u64 = 0;

    for entry in fs::read_dir(&assets_dir)
        .map_err(|e| format!("cannot read {}: {e}", assets_dir.display()))?
    {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let len = entry
            .metadata()
            .map_err(|e| format!("stat {name}: {e}"))?
            .len();
        if name.ends_with(".wasm.br") {
            wasm_br = wasm_br.saturating_add(len);
        } else if name.ends_with(".js.br") {
            js_br = js_br.saturating_add(len);
        }
    }

    println!(
        "  core WASM (brotli):        {:>8} / {:>8} bytes ({:.1}%)",
        wasm_br,
        MAX_CORE_WASM_BROTLI_BYTES,
        (wasm_br as f64 / MAX_CORE_WASM_BROTLI_BYTES as f64) * 100.0
    );
    println!(
        "  first-screen JS (brotli):  {:>8} / {:>8} bytes ({:.1}%)",
        js_br,
        MAX_FIRST_SCREEN_JS_BROTLI_BYTES,
        (js_br as f64 / MAX_FIRST_SCREEN_JS_BROTLI_BYTES as f64) * 100.0
    );

    if wasm_br > MAX_CORE_WASM_BROTLI_BYTES {
        return Err(format!(
            "core WASM brotli size ({wasm_br} B) exceeds budget ({MAX_CORE_WASM_BROTLI_BYTES} B)"
        ));
    }
    if js_br > MAX_FIRST_SCREEN_JS_BROTLI_BYTES {
        return Err(format!(
            "first-screen JS brotli size ({js_br} B) exceeds budget ({MAX_FIRST_SCREEN_JS_BROTLI_BYTES} B)"
        ));
    }
    Ok(())
}

/// Builds the pages that experiments are run on, with the app's build
/// settings but apart from the app: the page that is measured, and the page
/// for checking input methods, which the end-to-end tests of the cell editor
/// also use.
pub fn build_spikes() -> Result {
    util::step("vite build (pages for experiments)");
    pnpm()
        .args(["--filter", APP, "exec", "vite", "build", "--mode", "spikes"])
        .run()
}

pub fn e2e(browser: Option<&str>) -> Result {
    util::step("playwright");
    let mut test = pnpm().args(["exec", "playwright", "test"]);
    if let Some(browser) = browser {
        test = test.arg(format!("--project={browser}"));
    }
    test.run()
}
