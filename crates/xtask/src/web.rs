//! The front-end half: checks, the production build, and end-to-end tests.

mod bundle;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::compat::sha256_hex;
use crate::util::{self, Result, cargo, node, pnpm};
use crate::wasm::report_sizes;

const APP: &str = "@reticula/web";
const DIST: &str = "apps/web/dist";

/// TypeScript projects, each with its own `tsconfig.json`.
const TS_PROJECTS: [&str; 6] = [
    "packages/protocol",
    "packages/grid",
    "apps/web",
    "tests/e2e",
    "tests/stack",
    "tests/visual",
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

/// What the visual regression tests may take: they run before every push.
const VISUAL_BUDGET_SECONDS: f64 = 120.0;

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
    // Before the build of the service worker, which writes to the same directory.
    let closures = bundle::Closures::take(&dist_dir)?;
    bundle::inject_boot_script(&dist_dir, &closures)?;

    util::step("vite build (service worker)");
    pnpm()
        .args(["--filter", APP, "exec", "vite", "build", "--mode", "sw"])
        .run()?;
    inject_sw_manifest(&dist_dir)?;

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
    bundle::check_budgets(&dist_dir, &closures)
}

/// Computes a deterministic `BUILD_ID` and precache URL manifest from the built
/// `dist/` output and injects both into `dist/sw.js` prior to precompression
/// (`14-m1b.md` 7.4).
fn inject_sw_manifest(dist_dir: &Path) -> Result {
    let mut urls = vec![String::from("/index.html")];
    let assets_dir = dist_dir.join("assets");
    let mut asset_names = Vec::new();
    for entry in fs::read_dir(&assets_dir)
        .map_err(|e| format!("cannot read {}: {e}", assets_dir.display()))?
    {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.ends_with(".br")
            || name.ends_with(".gz")
            || name.starts_with("reticula_sim_wasm")
            || name.starts_with("reticula_xlsx_wasm")
        {
            continue;
        }
        asset_names.push(name.to_owned());
    }
    asset_names.sort();
    for name in asset_names {
        urls.push(format!("/assets/{name}"));
    }

    // Hash every precached file's path + raw content to derive `BUILD_ID`.
    let mut hash_input = Vec::new();
    for url in &urls {
        let rel = url.trim_start_matches('/');
        let file_path = dist_dir.join(rel);
        let bytes = fs::read(&file_path).map_err(|e| {
            format!(
                "cannot read {} for SW manifest hash: {e}",
                file_path.display()
            )
        })?;
        hash_input.extend_from_slice(url.as_bytes());
        hash_input.push(0);
        hash_input.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        hash_input.extend_from_slice(&bytes);
    }
    let full_hex = sha256_hex(&hash_input);
    let build_id = &full_hex[..16];
    let manifest_json =
        serde_json::to_string(&urls).map_err(|e| format!("serialize SW precache manifest: {e}"))?;

    let sw_path = dist_dir.join("sw.js");
    let sw_src = fs::read_to_string(&sw_path)
        .map_err(|e| format!("cannot read {}: {e}", sw_path.display()))?;

    let id_token = "__RETICULA_BUILD_ID__";
    if !sw_src.contains(id_token) {
        return Err(format!(
            "placeholder {id_token} not found in {}",
            sw_path.display()
        ));
    }
    let list_placeholder = [
        "[\"__RETICULA_PRECACHE__\"]",
        "['__RETICULA_PRECACHE__']",
        "[`__RETICULA_PRECACHE__`]",
    ]
    .into_iter()
    .find(|pat| sw_src.contains(pat))
    .ok_or_else(|| {
        format!(
            "placeholder [\"__RETICULA_PRECACHE__\"] not found in {}",
            sw_path.display()
        )
    })?;
    let updated = sw_src
        .replace(id_token, build_id)
        .replacen(list_placeholder, &manifest_json, 1);
    fs::write(&sw_path, updated).map_err(|e| format!("cannot write {}: {e}", sw_path.display()))?;
    println!(
        "  injected SW manifest: build_id={build_id}, {} precached file(s)",
        urls.len()
    );
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
    // The collaborative tests start this server against a real database.
    let database = crate::stack::test_database_url()?;
    util::step("server (debug, for the collaborative tests)");
    cargo()
        .args(["build", "--locked", "--package", "reticula-server"])
        .run()?;

    util::step("playwright");
    let mut test = pnpm()
        .args(["exec", "playwright", "test"])
        .env("RETICULA_TEST_DATABASE_URL", database);
    if let Some(browser) = browser {
        test = test.arg(format!("--project={browser}"));
    }
    test.run()
}

/// The end-to-end tests of the WebGPU renderer. They need neither the server
/// nor a database.
pub fn e2e_webgpu() -> Result {
    util::step("playwright (WebGPU)");
    pnpm()
        .args([
            "exec",
            "playwright",
            "test",
            "-c",
            "playwright.webgpu.config.ts",
        ])
        .run()
}

/// Golden images belong to one environment, the runner of the `visual` CI job,
/// which sets this: elsewhere the same text is drawn a little differently.
pub fn is_golden_environment() -> bool {
    std::env::var_os("RETICULA_GOLDEN").is_some_and(|value| value == "1")
}

/// The visual regression tests, on the pages `build_spikes` makes.
pub fn visual(update: bool) -> Result {
    util::step("playwright (visual regression)");
    if !is_golden_environment() {
        println!("  golden images: skipped, they are compared by the `visual` CI job only");
    }
    let mut test = pnpm().args([
        "exec",
        "playwright",
        "test",
        "-c",
        "playwright.visual.config.ts",
    ]);
    if update {
        test = test.env("RETICULA_GOLDEN_UPDATE", "1");
    }
    let started = Instant::now();
    test.run()?;
    let seconds = started.elapsed().as_secs_f64();
    println!("  visual regression: {seconds:.1} s (budget {VISUAL_BUDGET_SECONDS:.0} s)");
    if seconds > VISUAL_BUDGET_SECONDS {
        return Err(format!(
            "the visual regression tests took {seconds:.1} s, more than {VISUAL_BUDGET_SECONDS:.0} s"
        ));
    }
    Ok(())
}
