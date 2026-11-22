//! The front-end half: checks, the production build, preload injection, size
//! budget verification, and end-to-end tests.

use std::fs;
use std::path::{Path, PathBuf};

use crate::compat::sha256_hex;
use crate::util::{self, Result, cargo, node, pnpm};
use crate::wasm::report_sizes;

const APP: &str = "@reticula/web";
const DIST: &str = "apps/web/dist";

/// Hard upper bound on brotli-compressed core `.wasm` module (`1.2 MB`, `14-m1b.md` 8.3).
const MAX_CORE_WASM_BROTLI_BYTES: u64 = 1_200_000;
/// Hard upper bound on brotli-compressed on-demand simulator `.wasm` module (`800 KB`, `16-m2b.md`).
const MAX_SIM_WASM_BROTLI_BYTES: u64 = 800_000;
/// Hard upper bound on brotli-compressed on-demand XLSX `.wasm` module (`900 KB`, `17-m3a.md` 8.2).
const MAX_XLSX_WASM_BROTLI_BYTES: u64 = 900_000;
/// Upper bound on brotli-compressed first-screen critical-path JS (`165 KB`, `14-m1b.md` 8.3).
const MAX_FIRST_SCREEN_JS_BROTLI_BYTES: u64 = 165_000;
/// Upper bound on brotli-compressed total JS including lazy chunks & service worker (`220 KB`, `14-m1b.md` 8.3).
const MAX_TOTAL_JS_BROTLI_BYTES: u64 = 220_000;

/// TypeScript projects, each with its own `tsconfig.json`.
const TS_PROJECTS: [&str; 5] = [
    "packages/protocol",
    "packages/grid",
    "apps/web",
    "tests/e2e",
    "tests/stack",
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
        } else if name.starts_with("reticula_wasm_bg")
            && Path::new(name)
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

fn check_budgets(dist_dir: &Path) -> Result {
    util::step("bundle size budgets (brotli)");
    let index_path = dist_dir.join("index.html");
    let index_html = fs::read_to_string(&index_path)
        .map_err(|e| format!("cannot read {}: {e}", index_path.display()))?;

    let assets_dir = dist_dir.join("assets");
    let mut core_wasm_br: u64 = 0;
    let mut sim_wasm_br: u64 = 0;
    let mut xlsx_wasm_br: u64 = 0;
    let mut first_screen_js_br: u64 = 0;
    let mut total_js_br: u64 = 0;

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
            if name.starts_with("reticula_sim_wasm") {
                sim_wasm_br = sim_wasm_br.saturating_add(len);
            } else if name.starts_with("reticula_xlsx_wasm") {
                xlsx_wasm_br = xlsx_wasm_br.saturating_add(len);
            } else {
                core_wasm_br = core_wasm_br.saturating_add(len);
            }
        } else if let Some(js_name) = name.strip_suffix(".br")
            && js_name.ends_with(".js")
        {
            total_js_br = total_js_br.saturating_add(len);
            if index_html.contains(js_name) {
                first_screen_js_br = first_screen_js_br.saturating_add(len);
            }
        }
    }

    let sw_br_path = dist_dir.join("sw.js.br");
    if let Ok(meta) = fs::metadata(&sw_br_path) {
        total_js_br = total_js_br.saturating_add(meta.len());
    }

    println!(
        "  core WASM (brotli):        {:>8} / {:>8} bytes ({:.1}%)",
        core_wasm_br,
        MAX_CORE_WASM_BROTLI_BYTES,
        (core_wasm_br as f64 / MAX_CORE_WASM_BROTLI_BYTES as f64) * 100.0
    );
    println!(
        "  sim WASM (brotli):         {:>8} / {:>8} bytes ({:.1}%)",
        sim_wasm_br,
        MAX_SIM_WASM_BROTLI_BYTES,
        (sim_wasm_br as f64 / MAX_SIM_WASM_BROTLI_BYTES as f64) * 100.0
    );
    println!(
        "  xlsx WASM (brotli):        {:>8} / {:>8} bytes ({:.1}%)",
        xlsx_wasm_br,
        MAX_XLSX_WASM_BROTLI_BYTES,
        (xlsx_wasm_br as f64 / MAX_XLSX_WASM_BROTLI_BYTES as f64) * 100.0
    );
    println!(
        "  first-screen JS (brotli):  {:>8} / {:>8} bytes ({:.1}%)",
        first_screen_js_br,
        MAX_FIRST_SCREEN_JS_BROTLI_BYTES,
        (first_screen_js_br as f64 / MAX_FIRST_SCREEN_JS_BROTLI_BYTES as f64) * 100.0
    );
    println!(
        "  total JS (brotli):         {:>8} / {:>8} bytes ({:.1}%)",
        total_js_br,
        MAX_TOTAL_JS_BROTLI_BYTES,
        (total_js_br as f64 / MAX_TOTAL_JS_BROTLI_BYTES as f64) * 100.0
    );

    if core_wasm_br == 0 || core_wasm_br > MAX_CORE_WASM_BROTLI_BYTES {
        return Err(format!(
            "core WASM brotli size ({core_wasm_br} B) out of budget (1..={MAX_CORE_WASM_BROTLI_BYTES} B)"
        ));
    }
    if sim_wasm_br == 0 || sim_wasm_br > MAX_SIM_WASM_BROTLI_BYTES {
        return Err(format!(
            "sim WASM brotli size ({sim_wasm_br} B) out of budget (1..={MAX_SIM_WASM_BROTLI_BYTES} B)"
        ));
    }
    if xlsx_wasm_br == 0 || xlsx_wasm_br > MAX_XLSX_WASM_BROTLI_BYTES {
        return Err(format!(
            "xlsx WASM brotli size ({xlsx_wasm_br} B) out of budget (1..={MAX_XLSX_WASM_BROTLI_BYTES} B)"
        ));
    }
    if first_screen_js_br > MAX_FIRST_SCREEN_JS_BROTLI_BYTES {
        return Err(format!(
            "first-screen JS brotli size ({first_screen_js_br} B) exceeds budget ({MAX_FIRST_SCREEN_JS_BROTLI_BYTES} B)"
        ));
    }
    if total_js_br > MAX_TOTAL_JS_BROTLI_BYTES {
        return Err(format!(
            "total JS brotli size ({total_js_br} B) exceeds budget ({MAX_TOTAL_JS_BROTLI_BYTES} B)"
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
