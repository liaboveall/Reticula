//! The English texts that Rust shows to users, for the front end to translate.
//!
//! A message is written where it arises and translated where it is shown, so the
//! translations have to follow whatever is written. The list is read off the
//! sources: every literal in the files of [`SOURCES`] that reads like English is a
//! message, other than what is named there as something else. A new message is in
//! the list as soon as it is written: `--check` fails until the list is generated
//! again, and the tests of the front end fail until the message is translated.
//!
//! What cannot be read off a literal is listed by running the code: the values a
//! message may print, and the functions of formulas.

use std::collections::BTreeMap;
use std::path::Path;

use reticula_formula::functions::catalog::FUNCTION_CATALOG;
use reticula_wire::v2::WireRejectReasonV1;
use serde_json::json;

use super::rust_source::literals;
use crate::util::{self, Result};

/// The crates whose messages get to users. A file that is in neither list below
/// has no literal that reads like English, or is an error.
const CRATES: &[&str] = &[
    "agent",
    "core",
    "formula",
    "server",
    "sim-wasm",
    "sync",
    "wasm",
    "wire",
    "xlsx",
    "xlsx-wasm",
];

/// The files with messages, and what in them reads like English and is none.
const SOURCES: &[(&str, &[&str])] = &[
    ("crates/agent/src/diff.rs", &[]),
    (
        "crates/agent/src/lib.rs",
        &[
            "RETICULA_LLM_ENABLED must be one of: true | false | 1 | 0",
            "RETICULA_LLM_TIMEOUT_SECS (must be a positive integer)",
            "RETICULA_LLM_BASE_URL (must be http:// or https://)",
            "RETICULA_LLM_MODEL (must be non-empty when RETICULA_LLM_ENABLED=true)",
            "RETICULA_LLM_API_KEY (must be non-empty when RETICULA_LLM_ENABLED=true)",
            "invalid LLM configuration: {missing}",
            "configuration error: {0}",
        ],
    ),
    (
        "crates/agent/src/llm.rs",
        &[
            "failed to build HTTP client: {e}",
            "Accept",
            // The provider for tests.
            "insert row",
            "Inspecting the workbook structure before inserting a subtotal row.",
            "Inserting a new row at row 2 for the adjustment factor.",
            "Populating the newly inserted row A2=25 and referencing it from existing cell B1.",
            "Verified formulas and finishing draft.",
            "Inserted row 2 with value 25 and updated B1 to =A2*2.",
            "Inserted 1 row at row 2",
            "Set B1 = A2*2 (evaluates to 50)",
            "Reading existing cells A1:B4 to compute summary updates.",
            "Updating A2 and writing a total formula in B2.",
            "Draft ready for review.",
            "Updated A2 to 420 and added 10% uplift formula in B2.",
            "Inspecting workbook overview to identify data columns and existing formulas.",
            "Dry-running summary formula before writing to draft.",
            "Writing summary label and formula into C1:D1 on the AI draft branch.",
            "AI Total",
            "Checking workbook for calculation errors.",
            "All formulas verified with zero errors.",
            "Added summary total in C1:D1 (=SUM(A1:A10)) with zero formula errors.",
            "C1: AI Total",
            "Finalizing scripted run.",
            "Completed scripted run.",
        ],
    ),
    (
        "crates/agent/src/runner.rs",
        &[
            // What the model is told.
            "You are Reticula AI Analyst, a precise spreadsheet engineer operating on an isolated draft branch.*",
            "Invalid JSON arguments for `{name}`: {err}",
            "\n[Guardrail warning: you called this tool with identical arguments 3 times. Change approach or call `finish`.]",
        ],
    ),
    ("crates/formula/src/lang_service.rs", &[]),
    ("crates/formula/src/search.rs", &[]),
    (
        "crates/server/src/agent.rs",
        &[
            // The name of an author, which is stored.
            "AI Analyst",
            // Told to the model.
            "row ID space exhausted",
            "column ID space exhausted",
            "sheet ID space exhausted",
            "draft branch commit failed: {e}",
            "local draft engine apply failed: {e}",
        ],
    ),
    (
        "crates/server/src/docs.rs",
        &[
            // Answered with a status of its own.
            "document actor is unavailable",
            "server document memory budget is saturated",
            "access revoked before session join",
            "handshake rejected: {0}",
        ],
    ),
    (
        "crates/server/src/http.rs",
        &[
            "collaboration storage unavailable\n",
            "client error fields are missing or too long",
        ],
    ),
    (
        "crates/server/src/http/agent.rs",
        // A title, which is stored.
        &["Draft branch"],
    ),
    (
        "crates/server/src/http/sync.rs",
        &["expected a WebSocket upgrade"],
    ),
    (
        "crates/server/src/store.rs",
        &[
            // Answered with a status of its own.
            "blocking snapshot task failed: {0}",
            "workbook does not exist",
            "access denied",
            "the actor's replica counter or log head no longer matches persistent state",
            "invalid persistent value: {0}",
            "base snapshot seq=0 missing on disk",
        ],
    ),
    ("crates/server/src/store/agent.rs", &[]),
    ("crates/server/src/store/collaboration.rs", &[]),
    ("crates/server/src/store/history.rs", &[]),
    (
        "crates/sim-wasm/src/lib.rs",
        // The names of the replicas.
        &["Alice", "Carol"],
    ),
    ("crates/wasm/src/clipboard.rs", &["<table data-reticula=*"]),
    ("crates/wasm/src/collab.rs", &[]),
    ("crates/wasm/src/history.rs", &[]),
    (
        "crates/wasm/src/lib.rs",
        &[
            "a 64-bit unsigned integer as a decimal string or non-negative integer",
            // The name of a worksheet.
            "Imported",
        ],
    ),
    ("crates/wasm/src/query.rs", &[]),
    (
        "crates/xlsx/src/export.rs",
        &["yyyy-mm-dd hh:mm:ss", "yyyy-mm-dd hh:mm"],
    ),
    ("crates/xlsx/src/import.rs", &[]),
    ("crates/xlsx/src/lib.rs", &[]),
    (
        "crates/xlsx/src/patch.rs",
        &["Override", "Relationship", "Target"],
    ),
    ("crates/xlsx/src/xml_scan.rs", &["Relationship", "Target"]),
    ("crates/xlsx-wasm/src/lib.rs", &[]),
];

/// The files in which nothing is a message, whatever it reads like.
const SILENT: &[&str] = &[
    // Read by the model, and by whoever runs the evaluation.
    "crates/agent/src/context.rs",
    "crates/agent/src/eval.rs",
    "crates/agent/src/tools.rs",
    // The recorded demos, which are in English.
    "crates/agent/src/replay.rs",
    // Formats of dates, and data that is not as it should be.
    "crates/core/src/format.rs",
    "crates/core/src/store.rs",
    "crates/formula/src/functions/text_conv.rs",
    // The content of a workbook for a test.
    "crates/formula/src/determinism.rs",
    // Listed by running the code.
    "crates/formula/src/functions/catalog.rs",
    // For whoever operates the server, or answers to a navigation.
    "crates/server/src/http/auth.rs",
    "crates/server/src/main.rs",
    "crates/server/src/ops.rs",
    // The benchmarks.
    "crates/wasm/src/bench.rs",
    "crates/wasm/src/bench_sync.rs",
];

/// Calls and macros whose texts are for a log, for a database, for the other end
/// of a connection, or for whoever reads the code.
fn is_internal_call(name: &str) -> bool {
    const PREFIXES: &[&str] = &["assert", "debug_assert", "sqlx::", "tracing::"];
    const NAMES: &[&str] = &[
        "HeaderValue::from_static",
        // A value in the database that is not as it should be.
        "StoreError::Invalid",
        "close_ws",
        "debug_struct",
        "eprintln!",
        "expect",
        "integer",
        "panic!",
        "println!",
        "send_close",
        "unreachable!",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || NAMES.contains(&name)
        || NAMES.contains(&name.trim_start_matches("std::"))
}

/// A text without its `{placeholders}`.
fn without_placeholders(text: &str) -> String {
    let mut depth = 0_usize;
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Whether a text has several words, or is a single one such as a label has.
fn is_english(text: &str) -> bool {
    let parts: Vec<&str> = text.split_whitespace().collect();
    match parts.as_slice() {
        [] => false,
        [label] => {
            let mut chars = label.chars();
            label.len() >= 4
                && chars.next().is_some_and(|c| c.is_ascii_uppercase())
                && chars.all(|c| c.is_ascii_lowercase() || c == '-')
        }
        _ => without_placeholders(text).split_whitespace().any(|part| {
            part.chars().filter(char::is_ascii_alphabetic).count() >= 2
                && !part.contains(['_', '='])
        }),
    }
}

/// The messages of a file: what reads like English in it, each once and in order.
fn messages_in(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for literal in literals(source) {
        let is_shown = !literal.calls.iter().any(|call| is_internal_call(call));
        if is_shown && is_english(&literal.text) {
            for message in singular_and_plural(literal.text) {
                if !found.contains(&message) {
                    found.push(message);
                }
            }
        }
    }
    found
}

/// The two texts of a message that puts an "s" where it belongs.
fn singular_and_plural(message: String) -> Vec<String> {
    const ENDING: &str = "{plural}";
    if message.contains(ENDING) {
        vec![message.replace(ENDING, ""), message.replace(ENDING, "s")]
    } else {
        vec![message]
    }
}

/// Takes out what is no message, and reports what is not there to be taken out.
fn without(quiet: &[&str], path: &str, messages: &mut Vec<String>) -> Result {
    for text in quiet {
        let before = messages.len();
        messages.retain(|message| match text.strip_suffix('*') {
            Some(start) => !message.starts_with(start),
            None => message != text,
        });
        if messages.len() == before {
            return Err(format!("{path} does not contain {text:?} any more"));
        }
    }
    Ok(())
}

/// The messages of each file in [`SOURCES`].
fn messages() -> Result<BTreeMap<String, Vec<String>>> {
    let root = util::root();
    let mut found = BTreeMap::new();
    let mut unlisted = Vec::new();
    let mut paths = Vec::new();
    for name in CRATES {
        rust_files(&root, &format!("crates/{name}/src"), &mut paths)?;
    }
    for path in &paths {
        if SILENT.contains(&path.as_str()) {
            continue;
        }
        let source = read(&root, path)?;
        let mut messages = messages_in(&source);
        match SOURCES.iter().find(|(listed, _)| listed == path) {
            Some((_, quiet)) => {
                without(quiet, path, &mut messages)?;
                found.insert(path.clone(), messages);
            }
            None => unlisted.extend(messages.iter().map(|text| format!("{path}: {text:?}"))),
        }
    }
    if !unlisted.is_empty() {
        return Err(format!(
            "these files are in neither list of crates/xtask/src/codegen/messages.rs:\n{}",
            unlisted.join("\n")
        ));
    }
    let listed = SOURCES.iter().map(|(path, _)| path).chain(SILENT);
    if let Some(gone) = listed
        .into_iter()
        .find(|path| !paths.iter().any(|p| p == *path))
    {
        return Err(format!("{gone} does not exist any more"));
    }
    Ok(found)
}

fn read(root: &Path, path: &str) -> Result<String> {
    std::fs::read_to_string(root.join(path)).map_err(|error| format!("cannot read {path}: {error}"))
}

/// The sources under a directory, without those of tests, in order.
fn rust_files(root: &Path, directory: &str, found: &mut Vec<String>) -> Result {
    let entries = std::fs::read_dir(root.join(directory))
        .map_err(|error| format!("cannot read {directory}: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {directory}: {error}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{directory}/{name}");
        if entry.path().is_dir() {
            rust_files(root, &path, found)?;
        } else if name.ends_with(".rs") && name != "tests.rs" {
            found.push(path);
        }
    }
    found.sort();
    Ok(())
}

/// What a message may have in a placeholder that is a word of the code's.
fn printed_values() -> Result<serde_json::Value> {
    Ok(json!({
        "error": run_errors()?,
        "reason": reject_reasons(),
        "status": branch_states()?,
    }))
}

/// Why the server turns a transaction down, as the messages print it.
fn reject_reasons() -> Vec<String> {
    use WireRejectReasonV1 as Reason;
    let reasons = [
        Reason::Forbidden,
        Reason::InvalidOp,
        Reason::NonContiguousClientSeq { expected: 1 },
        Reason::QuotaExceeded,
        Reason::EpochMismatch { server_epoch: 1 },
        Reason::UpgradeRequired,
        Reason::PayloadTooLarge,
    ];
    reasons
        .iter()
        .map(|reason| {
            // No arm is left to the rest: a new variant has to be listed above.
            let field = match reason {
                Reason::Forbidden
                | Reason::InvalidOp
                | Reason::QuotaExceeded
                | Reason::UpgradeRequired
                | Reason::PayloadTooLarge => "",
                Reason::NonContiguousClientSeq { .. } => "{expected}",
                Reason::EpochMismatch { .. } => "{server_epoch}",
            };
            format!("{reason:?}").replace('1', field)
        })
        .collect()
}

/// What a run has for an error that is a single word, too short to be told from
/// code: it is written where the run is cancelled.
fn run_errors() -> Result<Vec<String>> {
    const PATH: &str = "crates/server/src/http/agent.rs";
    const CANCELLED: &str = "cancelled";
    let source = read(&util::root(), PATH)?;
    if source.contains(&format!("\"\", Some(\"{CANCELLED}\"))")) {
        Ok(vec![CANCELLED.to_owned()])
    } else {
        Err(format!("{PATH} does not write {CANCELLED:?} any more"))
    }
}

/// The states of a branch, as the database allows them.
fn branch_states() -> Result<Vec<String>> {
    const PATH: &str = "crates/server/migrations/0004_m4_branches_agent.sql";
    let source = read(&util::root(), PATH)?;
    let states = source
        .split_once("CONSTRAINT branches_status_check CHECK (status IN (")
        .and_then(|(_, rest)| rest.split_once(')'))
        .ok_or_else(|| format!("{PATH} does not list the states of a branch"))?
        .0;
    Ok(states
        .split(',')
        .map(|state| state.trim().trim_matches('\'').to_owned())
        .collect())
}

/// Everything there is to translate, as JSON.
pub(super) fn fixture() -> Result<String> {
    let functions: Vec<_> = FUNCTION_CATALOG
        .iter()
        .map(|f| json!({ "name": f.name, "params": f.params, "summary": f.summary }))
        .collect();
    let fixture = json!({
        "messages": messages()?,
        "values": printed_values()?,
        "functions": functions,
    });
    let mut out = serde_json::to_string_pretty(&fixture)
        .map_err(|error| format!("cannot serialize the messages: {error}"))?;
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{is_english, messages_in, without};

    #[test]
    fn words_are_told_from_code() {
        for text in [
            "Sort",
            "Auto-fill",
            "Insert {count} rows",
            "at top",
            "{pivots} pivot table definition(s)",
        ] {
            assert!(is_english(text), "{text}");
        }
        for text in [
            "",
            "ok",
            "General_",
            "camelCase",
            "SUM",
            "{zip_path}: {e}",
            "reticula_loaded_bytes {bytes}",
        ] {
            assert!(!is_english(text), "{text}");
        }
    }

    #[test]
    fn messages_are_listed_once_and_without_what_only_the_log_gets() {
        let source = r##"
            fn f() -> Result<(), String> {
                tracing::warn!("slow to answer");
                let n = value.expect("a value that is there");
                format!("{n} argument{plural} left");
                Err("not found".into()).or(Err("not found".into()))
            }
            #[cfg(test)]
            mod tests { const A: &str = "only a test"; }
        "##;
        assert_eq!(
            messages_in(source),
            ["{n} argument left", "{n} arguments left", "not found"]
        );
    }

    #[test]
    fn what_is_named_as_no_message_has_to_be_there() {
        let mut messages = vec![
            String::from("You are told at length"),
            String::from("not found"),
        ];
        assert!(without(&["You are*"], "x.rs", &mut messages).is_ok());
        assert_eq!(messages, ["not found"]);
        assert!(without(&["gone"], "x.rs", &mut messages).is_err_and(|e| e.contains("gone")));
    }
}
