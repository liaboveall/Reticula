//! Installing the pinned tools.

use crate::util::{self, Flags, Result, cargo, pnpm};

/// A command-line tool built from a crate at an exact version.
pub struct Tool {
    /// The crate `cargo install` builds.
    pub krate: &'static str,
    /// The name the CI workflows install a prebuilt binary under.
    pub prebuilt: &'static str,
    pub version: &'static str,
}

/// The single source of truth for tool versions. CI installs prebuilt binaries
/// of the same versions; [`check_workflow_pins`] keeps the two in step.
pub const TOOLS: [Tool; 3] = [
    // Must equal the version of the `wasm-bindgen` crate in Cargo.toml.
    Tool {
        krate: "wasm-bindgen-cli",
        prebuilt: "wasm-bindgen",
        version: "0.2.130",
    },
    Tool {
        krate: "cargo-nextest",
        prebuilt: "cargo-nextest",
        version: "0.9.152",
    },
    Tool {
        krate: "cargo-deny",
        prebuilt: "cargo-deny",
        version: "0.20.2",
    },
];

pub fn run(flags: &Flags) -> Result {
    // Into the repository, not the user's global cargo directory. `cargo
    // install` does nothing when the requested version is already there.
    for tool in &TOOLS {
        util::step(&format!("{} {}", tool.krate, tool.version));
        cargo()
            .args([
                "install",
                "--locked",
                "--root",
                ".tools",
                "--version",
                tool.version,
            ])
            .arg(tool.krate)
            .run()?;
    }

    util::step("JavaScript dependencies");
    pnpm().args(["install", "--frozen-lockfile"]).run()?;

    if util::root().join("lefthook.yml").is_file() {
        util::step("git hooks");
        pnpm().args(["exec", "lefthook", "install"]).run()?;
    }

    if flags.has("browsers") {
        util::step("Playwright browsers");
        pnpm()
            .args([
                "exec",
                "playwright",
                "install",
                "chromium",
                "firefox",
                "webkit",
            ])
            .run()?;
    }
    Ok(())
}

/// Fails if a workflow installs one of [`TOOLS`] at a different version.
pub fn check_workflow_pins() -> Result {
    let directory = util::root().join(".github/workflows");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for tool in &TOOLS {
            let prefix = format!("{}@", tool.prebuilt);
            for (_, rest) in text.match_indices(&prefix).map(|(at, _)| text.split_at(at)) {
                let pinned = rest.trim_start_matches(&prefix);
                let version: String = pinned
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                if version != tool.version {
                    return Err(format!(
                        "{} installs {} {version}, but xtask pins {}",
                        path.display(),
                        tool.prebuilt,
                        tool.version
                    ));
                }
            }
        }
    }
    Ok(())
}
