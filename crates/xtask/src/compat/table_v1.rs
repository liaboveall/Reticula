//! Samples of table files, version 1.
//!
//! `cargo xtask compat --samples table-v1` writes them to `target/compat/` to be
//! looked at. `--freeze table-v1` writes them to `tests/compat/table-v1/` and
//! records their hashes, and those of the modules that define the format. From
//! then on `cargo xtask check` makes sure that all of them are what they were,
//! and that the samples still hold what they held.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use reticula_table::determinism::{content_digest, to_hex};
use reticula_table::layout::{Directory, Trailer};
use reticula_table::reader::{Opened, Reader};
use reticula_table::sample::Sample;
use reticula_table::types::ColumnType;
use reticula_table::writer::{ColumnSpec, TableWriter};

use super::{HASHES_FILE, hash_repo_file};
use crate::util::{self, Result};

const DIRECTORY: &str = "tests/compat/table-v1";

/// What is frozen: the modules that say what the bytes of a file mean, and files.
pub(super) const FROZEN_TABLE_V1_FILES: [&str; 9] = [
    "crates/table/src/bits.rs",
    "crates/table/src/hash.rs",
    "crates/table/src/layout.rs",
    "crates/table/src/types.rs",
    "tests/compat/table-v1/empty.rtbl",
    "tests/compat/table-v1/expected_digests.txt",
    "tests/compat/table-v1/kinds.rtbl",
    "tests/compat/table-v1/mixed.rtbl",
    "tests/compat/table-v1/orders.rtbl",
];

/// Whether the hashes are recorded.
pub(super) fn is_frozen(entries: &[(String, String)]) -> bool {
    entries.iter().any(|(_, path)| path.starts_with(DIRECTORY))
}

/// The samples by name.
fn samples() -> Result<Vec<(&'static str, Vec<u8>)>> {
    let orders = {
        let mut file = Vec::new();
        let mut sample = Sample::new(2_000, 2026).map_err(|e| format!("{e:?}"))?;
        while sample.step(&mut file).map_err(|e| format!("{e:?}"))? {}
        file
    };
    Ok(vec![
        ("empty.rtbl", write_rows(&kinds_of_columns(), 0, 256)?),
        ("kinds.rtbl", write_rows(&kinds_of_columns(), 1_000, 256)?),
        ("mixed.rtbl", mixed()?),
        ("orders.rtbl", orders),
    ])
}

type Cells = fn(u32) -> Cell;
/// A column: its name, its type, whether it has a dictionary, and its cells.
type OfKind = (&'static str, Option<ColumnType>, bool, Cells);

enum Cell {
    Blank,
    Int(i64),
    Float(f64),
    Text(String),
}

/// Every type, and shapes that lead to every way a block can be held.
fn kinds_of_columns() -> Vec<OfKind> {
    let decimal = |scale, fixed| Some(ColumnType::Decimal { scale, fixed });
    vec![
        ("row", Some(ColumnType::Integer), false, |row| {
            Cell::Int(i64::from(row))
        }),
        ("same", Some(ColumnType::Integer), false, |_| {
            Cell::Int(i64::MIN)
        }),
        ("wide", Some(ColumnType::Integer), false, |row| {
            Cell::Int(mix(row).cast_signed())
        }),
        ("hundreds", Some(ColumnType::Integer), false, |row| {
            Cell::Int(i64::from(row % 70) * 100 - 900)
        }),
        ("money", decimal(2, true), false, |row| {
            Cell::Int(i64::from(row) * 5 - 250)
        }),
        ("tiny", decimal(18, false), false, |row| match row % 3 {
            0 => Cell::Blank,
            _ => Cell::Int(i64::from(row)),
        }),
        ("float", Some(ColumnType::Float), false, |row| {
            match row % 7 {
                0 => Cell::Blank,
                1 => Cell::Float(-0.0),
                2 => Cell::Float(5e-324),
                3 => Cell::Float(f64::MAX),
                _ => Cell::Float(f64::from(row) / 7.0 - 50.0),
            }
        }),
        ("day", Some(ColumnType::Date), false, |row| match row {
            0 => Cell::Int(-719_162),
            1 => Cell::Int(2_932_896),
            _ => Cell::Int(19_000 + i64::from(row / 9)),
        }),
        ("at", Some(ColumnType::DateTime), false, |row| {
            Cell::Int(1_700_000_000_000_000 + i64::from(row) * 1_500_000)
        }),
        ("truth", Some(ColumnType::Boolean), false, |row| {
            if row % 11 == 0 {
                Cell::Blank
            } else {
                Cell::Int(i64::from(row & 1))
            }
        }),
        ("few", Some(ColumnType::Text), true, |row| {
            let texts = ["North", "north", "Süd", "東", "", "West"];
            Cell::Text(texts[row as usize % texts.len()].to_owned())
        }),
        ("many \u{1f980}", Some(ColumnType::Text), true, |row| {
            Cell::Text(format!("{:x}", mix(row) >> 52))
        }),
        ("plain", Some(ColumnType::Text), false, |row| {
            match row % 13 {
                0 => Cell::Blank,
                1 => Cell::Text("long ".repeat(60)),
                _ => Cell::Text(format!("{:016x}", mix(row))),
            }
        }),
        ("late", None, true, |row| {
            if row < 600 {
                Cell::Blank
            } else {
                Cell::Text(format!("{}", row % 5))
            }
        }),
        ("nothing", None, false, |_| Cell::Blank),
    ]
}

/// Numbers that look like any.
fn mix(row: u32) -> u64 {
    reticula_core::rng::mix64(u64::from(row) + 1)
}

fn write_rows(columns: &[OfKind], rows: u32, group_rows: u32) -> Result<Vec<u8>> {
    let failed = |e| format!("cannot write a sample: {e:?}");
    let specs = columns
        .iter()
        .map(|(name, kind, dictionary, _)| ColumnSpec {
            name: (*name).to_owned(),
            kind: *kind,
            dictionary: *dictionary,
        })
        .collect();
    let mut writer = TableWriter::new(specs, group_rows).map_err(failed)?;
    let mut file = Vec::new();
    for row in 0..rows {
        for (builder, (_, _, _, cell)) in writer.columns().iter_mut().zip(columns) {
            match cell(row) {
                Cell::Blank => builder.push_blank(),
                Cell::Int(value) => builder.push_int(value),
                Cell::Float(value) => builder.push_float(value),
                Cell::Text(text) => {
                    builder.set_kind(ColumnType::Text);
                    builder.push_text(&text);
                }
            }
        }
        if writer.check_group().map_err(failed)? {
            writer.seal_group().map_err(failed)?;
            while writer.step(&mut file).map_err(failed)? {}
        }
    }
    writer.seal_group().map_err(failed)?;
    while writer.step(&mut file).map_err(failed)? {}
    writer.finish();
    while writer.step(&mut file).map_err(failed)? {}
    Ok(file)
}

/// A column whose first group is in a dictionary and whose others are not, as
/// the writer makes it only of columns that have millions of texts: of two
/// columns with the same texts, the blocks of the one and those of the other.
fn mixed() -> Result<Vec<u8>> {
    let cells: Cells = |row| match row % 9 {
        0 => Cell::Blank,
        _ => Cell::Text(format!("Text {}", mix(row) >> 56)),
    };
    let kind = Some(ColumnType::Text);
    let pair = write_rows(
        &[("mixed", kind, true, cells), ("mixed", kind, false, cells)],
        700,
        256,
    )?;
    let mut source = pair.as_slice();
    let Ok(Opened::Ready(reader)) = Reader::open(&mut source, pair.len() as u64) else {
        return Err("a sample cannot be opened".to_owned());
    };
    let old = reader.directory();
    let mut directory = Directory {
        blocks: (0..old.groups())
            .filter_map(|group| old.block(usize::from(group > 0), group).copied())
            .collect(),
        ..old.clone()
    };
    directory.columns.truncate(1);
    let data_len = usize::try_from(old.data_len).map_err(|e| e.to_string())?;
    // The data, and so the checksums of its pages, are the same.
    let mut file = pair[..data_len].to_vec();
    let mut encoded = Vec::new();
    directory.encode(&mut encoded);
    let trailer = Trailer {
        directory_at: old.data_len,
        directory_len: encoded.len() as u64,
        directory_sha256: sha256(&encoded),
    };
    trailer.encode(&mut encoded);
    file.extend_from_slice(&encoded);
    Ok(file)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let hex = super::sha256_hex(bytes);
    std::array::from_fn(|at| u8::from_str_radix(&hex[at * 2..at * 2 + 2], 16).unwrap_or(0))
}

/// The digests of what the samples hold, one on each line.
fn digests(files: &[(&'static str, Vec<u8>)]) -> Result<String> {
    let mut out = String::new();
    for (name, file) in files {
        let digest =
            content_digest(file).ok_or_else(|| format!("{name} cannot be read as a table"))?;
        let hex = to_hex(digest);
        let _ = writeln!(out, "{}  {name}", String::from_utf8_lossy(&hex));
    }
    Ok(out)
}

fn write_to(directory: &Path) -> Result {
    let files = samples()?;
    let digests = digests(&files)?;
    let failed = |path: &Path, e: std::io::Error| format!("cannot write {}: {e}", path.display());
    fs::create_dir_all(directory).map_err(|e| failed(directory, e))?;
    for (name, file) in &files {
        let path = directory.join(name);
        fs::write(&path, file).map_err(|e| failed(&path, e))?;
        println!("{} bytes  {}", file.len(), path.display());
    }
    let path = directory.join("expected_digests.txt");
    fs::write(&path, &digests).map_err(|e| failed(&path, e))?;
    print!("{digests}");
    Ok(())
}

/// Writes the samples to where nobody minds.
pub(super) fn preview() -> Result {
    util::step("samples of table files");
    write_to(&util::root().join("target/compat/table-v1"))
}

/// Writes the samples and records the hashes after those that are there.
pub(super) fn freeze() -> Result {
    util::step("generating tests/compat/table-v1 and updating hashes.txt");
    freeze_at(&util::root())
}

fn freeze_at(root: &Path) -> Result {
    let hashes_path = root.join(HASHES_FILE);
    let before = fs::read_to_string(&hashes_path)
        .map_err(|e| format!("cannot read {}: {e}", hashes_path.display()))?;
    if before.lines().any(|line| line.contains(DIRECTORY)) {
        return Err(format!(
            "table files of version 1 are frozen: {HASHES_FILE} has them. A change of the format is a new version"
        ));
    }
    let directory = root.join(DIRECTORY);
    write_to(&directory)?;
    let attributes = directory.join(".gitattributes");
    fs::write(&attributes, "*.rtbl binary\n")
        .map_err(|e| format!("cannot write {}: {e}", attributes.display()))?;
    let mut after = before;
    after.push_str("# Frozen SHA-256 manifest for table files, version 1 (RTBL v1).\n");
    for path in FROZEN_TABLE_V1_FILES {
        let hash = hash_repo_file(&root.join(path))?;
        let _ = writeln!(after, "{hash}  {path}");
    }
    fs::write(&hashes_path, after).map_err(|e| format!("write {HASHES_FILE}: {e}"))
}

/// Makes sure that the samples hold what they held.
pub(super) fn check() -> Result {
    check_at(&util::root())
}

fn check_at(root: &Path) -> Result {
    let directory = root.join(DIRECTORY);
    let expected = fs::read_to_string(directory.join("expected_digests.txt"))
        .map_err(|e| format!("cannot read the digests of {DIRECTORY}: {e}"))?;
    for line in expected.lines() {
        let (digest, name) = line
            .split_once("  ")
            .ok_or_else(|| format!("{DIRECTORY}: malformed digest `{line}`"))?;
        let file = fs::read(directory.join(name))
            .map_err(|e| format!("cannot read {DIRECTORY}/{name}: {e}"))?;
        let found = content_digest(&file).map(to_hex);
        if found.as_ref().map(<[u8; 16]>::as_slice) != Some(digest.as_bytes()) {
            return Err(format!(
                "{DIRECTORY}/{name} does not hold what it held: tables that were made before would not either"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use reticula_table::layout::Encoding;

    use super::*;

    fn directory(file: &[u8]) -> Directory {
        let mut source = file;
        match Reader::open(&mut source, file.len() as u64) {
            Ok(Opened::Ready(reader)) => reader.directory().clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_samples_have_blocks_of_every_kind() {
        let files = samples().expect("they can be made");
        assert_eq!(files, samples().expect("they can be made again"));
        let of = |name: &str| {
            let file = files.iter().find(|(of, _)| *of == name);
            directory(&file.expect(name).1)
        };
        let kinds = of("kinds.rtbl");
        for encoding in [
            Encoding::Blank,
            Encoding::Packed,
            Encoding::Raw,
            Encoding::Text,
        ] {
            assert!(kinds.blocks.iter().any(|block| block.encoding == encoding));
        }
        assert!(kinds.blocks.iter().any(|block| block.unit > 0));
        assert!(kinds.blocks.iter().any(|block| block.width == 64));
        assert!(
            kinds
                .blocks
                .iter()
                .any(|block| block.width == 0 && block.len == 0)
        );
        assert!(kinds.blocks.iter().any(|block| block.nulls_at > 0));
        assert_eq!(kinds.groups(), 4);
        assert_eq!(of("empty.rtbl").rows, 0);
        let mixed = of("mixed.rtbl");
        let encodings: Vec<Encoding> = mixed.blocks.iter().map(|block| block.encoding).collect();
        assert_eq!(
            encodings,
            [Encoding::Packed, Encoding::Text, Encoding::Text]
        );
        assert_eq!(
            digests(&files).expect("they can be read").lines().count(),
            4
        );
    }

    #[test]
    fn what_was_frozen_is_not_frozen_again_and_is_noticed_to_change() {
        let root = util::target_dir().join(format!("compat-test-{}", std::process::id()));
        for path in FROZEN_TABLE_V1_FILES
            .iter()
            .filter(|path| path.ends_with(".rs"))
        {
            let copy = root.join(path);
            fs::create_dir_all(copy.parent().expect("a directory")).expect("it can be made");
            fs::copy(util::root().join(path), copy).expect("it can be copied");
        }
        fs::create_dir_all(root.join("tests/compat")).expect("it can be made");
        let before = fs::read_to_string(util::root().join(HASHES_FILE)).expect("the hashes");
        let before: String = before
            .lines()
            .filter(|line| !line.contains("table"))
            .fold(String::new(), |all, line| all + line + "\n");
        fs::write(root.join(HASHES_FILE), &before).expect("it can be written");

        freeze_at(&root).expect("it can be frozen");
        let after = fs::read_to_string(root.join(HASHES_FILE)).expect("the hashes");
        let added = after.strip_prefix(&before).expect("what was there stays");
        assert_eq!(added.lines().count(), 1 + FROZEN_TABLE_V1_FILES.len());
        for path in FROZEN_TABLE_V1_FILES {
            let hash = hash_repo_file(&root.join(path)).expect("it is there");
            assert!(added.contains(&format!("{hash}  {path}\n")), "{path}");
        }
        assert_eq!(check_at(&root), Ok(()));
        assert!(freeze_at(&root).is_err());
        let sample = root.join(DIRECTORY).join("orders.rtbl");
        fs::copy(root.join(DIRECTORY).join("kinds.rtbl"), sample).expect("it can be copied");
        assert!(check_at(&root).is_err());
        fs::remove_dir_all(&root).expect("it can be removed");
    }

    #[test]
    fn what_is_to_be_frozen_is_what_is_written() {
        let names: Vec<&str> = samples()
            .expect("they can be made")
            .iter()
            .map(|(n, _)| *n)
            .collect();
        for path in FROZEN_TABLE_V1_FILES {
            match path.strip_prefix("tests/compat/table-v1/") {
                Some("expected_digests.txt") => {}
                Some(name) => assert!(names.contains(&name), "{path}"),
                None => assert!(util::root().join(path).is_file(), "{path}"),
            }
        }
        assert!(!is_frozen(&[(
            String::new(),
            "tests/compat/v1/x".to_owned()
        )]));
        assert!(is_frozen(&[(String::new(), format!("{DIRECTORY}/x"))]));
    }
}
