//! The front-end half: checks, the production build, and end-to-end tests.

use std::path::PathBuf;

use crate::util::{self, Result, node, pnpm};
use crate::wasm::report_sizes;

const APP: &str = "@reticula/web";
const DIST: &str = "apps/web/dist";

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
    let files: Vec<&std::path::Path> = files.iter().map(PathBuf::as_path).collect();
    report_sizes(&files)
}

pub fn e2e(browser: Option<&str>) -> Result {
    util::step("playwright");
    let mut test = pnpm().args(["exec", "playwright", "test"]);
    if let Some(browser) = browser {
        test = test.arg(format!("--project={browser}"));
    }
    test.run()
}
