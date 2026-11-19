//! Frozen wire schema & persistence compatibility gate (`D-020`, `14-m1b.md` 5.5 & 8.1).
//!
//! `cargo xtask compat --freeze v1` generates deterministic `RTCLSNAP` v1 and
//! `RTCLWAL1` fixtures under `tests/compat/v1/` and records the SHA-256 hashes
//! of `crates/wire/src/v1.rs` and every fixture in `tests/compat/hashes.txt`.
//! `cargo xtask check` verifies those hashes on every commit and CI run so an
//! accidental breaking change to a frozen wire version is caught immediately.

use std::fs;
use std::path::Path;

use reticula_core::determinism::to_hex;
use reticula_core::format::{CurrencySymbol, NumberFormat};
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::{
    CellEdit, CellInput, FormatPatch, FormatTarget, FormulaMode, Op, ScalarValue, Workbook,
};
use reticula_core::sheet::Sheet;
use reticula_core::style::{BorderWeight, ColorPatch, ColorValue, HorizontalAlign, StylePatch};
use reticula_formula::binder::{BoundFormula, templatize_formula};
use reticula_formula::engine::FormulaEngine;
use reticula_wire::snapshot::{decode_snapshot_any, encode_snapshot_v1};
use reticula_wire::v1::{WireOpV1, WireReplicaWatermarkV1, WireWalRecordV1};
use reticula_wire::wal::{encode_wal_complete, replay_wal_records, scan_wal};

use crate::util::{self, Flags, Result};

const HASHES_FILE: &str = "tests/compat/hashes.txt";

const FROZEN_V1_FILES: [&str; 4] = [
    "crates/wire/src/v1.rs",
    "tests/compat/v1/base.rtclsnap",
    "tests/compat/v1/expected_digest.txt",
    "tests/compat/v1/replay.rtclwal",
];

pub fn run(flags: &Flags) -> Result {
    if let Some(ver) = flags.get("freeze") {
        if ver != "v1" {
            return Err(format!("unknown wire version `{ver}` (supported: `v1`)"));
        }
        return freeze_v1();
    }
    check()
}

pub fn check() -> Result {
    util::step("frozen wire compatibility hashes (v1)");
    let root = util::root();
    let hashes_path = root.join(HASHES_FILE);
    let content = fs::read_to_string(&hashes_path).map_err(|e| {
        format!(
            "cannot read {}: {e} (run `cargo xtask compat --freeze v1` first)",
            hashes_path.display()
        )
    })?;

    let mut entries = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((hex, rel_path)) = trimmed.split_once("  ") else {
            return Err(format!(
                "{}:{}: malformed hash entry `{trimmed}`",
                HASHES_FILE,
                idx + 1
            ));
        };
        entries.push((hex.trim().to_owned(), rel_path.trim().to_owned()));
    }

    for required in FROZEN_V1_FILES {
        if !entries.iter().any(|(_, p)| p == required) {
            return Err(format!(
                "{HASHES_FILE} is missing required frozen file `{required}`"
            ));
        }
    }

    for (expected_hex, rel_path) in &entries {
        let full_path = root.join(rel_path);
        let actual_hex = hash_repo_file(&full_path)?;
        if &actual_hex != expected_hex {
            return Err(format!(
                "SHA-256 mismatch on frozen wire file `{rel_path}`:\n  expected: {expected_hex}\n  actual:   {actual_hex}\nFrozen wire modules and fixtures are immutable (D-020); introduce a new wire version instead of editing `{rel_path}`."
            ));
        }
    }

    println!("{} frozen v1 file(s) verified", entries.len());
    Ok(())
}

fn freeze_v1() -> Result {
    util::step("generating tests/compat/v1 fixtures and hashes.txt");
    let root = util::root();
    let dir = root.join("tests/compat/v1");
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    // 1. Construct deterministic 2-sheet base workbook and encode `base.rtclsnap`.
    let base_sheet = Sheet::with_axes(SheetId(0), String::from("Sheet1"), 64, 12);
    let mut eng = FormulaEngine::from_workbook_clean(Workbook::from_sheet(base_sheet, None));
    let s0 = SheetId(0);
    let s1 = eng.alloc_sheet_id();
    eng.apply_op(Op::AddSheet {
        sheet: s1,
        name: String::from("Rates"),
        after: Some(s0),
    })
    .map_err(|e| format!("AddSheet: {e:?}"))?;

    for (r, raw) in ["10", "20", "30", "40", "50"].iter().enumerate() {
        eng.edit_cell_raw(s0, RowId(r as u64), ColId(0), raw)
            .map_err(|e| format!("seed A: {e:?}"))?;
    }
    eng.edit_cell_raw(s0, RowId(0), ColId(1), "=SUM(A1:A5)")
        .map_err(|e| format!("seed B1: {e:?}"))?;

    for (r, (sku, rate)) in [("SKU-A", "15.5"), ("SKU-B", "28.0"), ("SKU-C", "42.25")]
        .iter()
        .enumerate()
    {
        eng.edit_cell_raw(s1, RowId(r as u64), ColId(0), &format!("'{sku}"))
            .map_err(|e| format!("Rates sku: {e:?}"))?;
        eng.edit_cell_raw(s1, RowId(r as u64), ColId(1), rate)
            .map_err(|e| format!("Rates val: {e:?}"))?;
    }
    eng.edit_cell_raw(
        s0,
        RowId(1),
        ColId(1),
        "=VLOOKUP(\"SKU-B\",Rates!A1:B3,2,FALSE)",
    )
    .map_err(|e| format!("seed B2: {e:?}"))?;
    eng.apply_op(Op::SetFreeze {
        sheet: s0,
        frozen_rows: 1,
        frozen_cols: 1,
    })
    .map_err(|e| format!("SetFreeze: {e:?}"))?;
    eng.recalc_all();

    let watermarks = vec![WireReplicaWatermarkV1 {
        replica: 1,
        last_client_seq: 10,
        max_id_counter: 50,
    }];
    let snap_bytes = encode_snapshot_v1(eng.workbook(), &watermarks)
        .map_err(|e| format!("encode_snapshot_v1: {e:?}"))?;

    // 2. Build 6 WAL transactions exercising InsertRows, DeleteRows, MoveRows
    //    (E25 anchor handoff on `=SUM(A1:A5)`!), FillFormula, SetFormat/Resize,
    //    and cross-sheet edit.
    let mut replay_eng = decode_snapshot_any(&snap_bytes)
        .map_err(|e| format!("decode base: {e:?}"))?
        .engine;

    let mut wal_records = Vec::new();
    let ins_run = replay_eng.allocator_mut().alloc_run(2).unwrap();
    let txns: Vec<(&str, Vec<Op<BoundFormula>>)> = {
        let op1 = vec![
            Op::InsertRows {
                sheet: s0,
                after: Some(RowId(1)),
                ids: ins_run,
            },
            Op::SetCells {
                sheet: s0,
                edits: vec![CellEdit {
                    row: RowId(ins_run.first_id),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(99.0)),
                    format: FormatPatch::Keep,
                }],
            },
        ];
        let op2 = vec![Op::DeleteRows {
            sheet: s0,
            ids: vec![IdRun::new(2, 1).unwrap()],
        }];
        // Move top anchor `RowId(0)` downward into the interior of `A1:A5` -> E25 handoff!
        let op3 = vec![Op::MoveRows {
            sheet: s0,
            ids: vec![IdRun::new(0, 1).unwrap()],
            after: Some(RowId(3)),
        }];
        let tpl_base = BoundFormula::parse_and_bind(
            "=A1*10+$B$2",
            FormulaMode::Dynamic,
            replay_eng.workbook(),
            s0,
        )
        .unwrap();
        let tpl = templatize_formula(&tpl_base, RowId(0), ColId(2));
        let op4 = vec![Op::FillFormula {
            sheet: s0,
            rows: vec![IdRun::new(5, 8).unwrap()],
            cols: vec![IdRun::new(2, 1).unwrap()],
            template: tpl,
        }];
        let op5 = vec![
            Op::SetFormat {
                sheet: s0,
                target: FormatTarget::Cols(vec![IdRun::new(2, 1).unwrap()]),
                number_format: FormatPatch::Set(NumberFormat::Currency {
                    symbol: CurrencySymbol::Usd,
                    decimals: 2,
                }),
                style: StylePatch {
                    bold: Some(true),
                    align: Some(HorizontalAlign::Right),
                    fill_color: ColorPatch::Set(ColorValue::Palette(3)),
                    border_bottom: Some(BorderWeight::Thin),
                    ..StylePatch::default()
                },
            },
            Op::ResizeRows {
                sheet: s0,
                sizes: vec![(IdRun::new(0, 1).unwrap(), Some(32))],
            },
        ];
        let cross_f = BoundFormula::parse_and_bind(
            "=B1+Rates!B3",
            FormulaMode::Dynamic,
            replay_eng.workbook(),
            s0,
        )
        .unwrap();
        let op6 = vec![Op::SetCells {
            sheet: s0,
            edits: vec![CellEdit {
                row: RowId(2),
                col: ColId(1),
                input: CellInput::Formula {
                    formula: cross_f,
                    cached: None,
                },
                format: FormatPatch::Keep,
            }],
        }];
        vec![
            ("Insert 2 rows", op1),
            ("Delete row", op2),
            ("Move row with E25 handoff", op3),
            ("Fill formula C6:C13", op4),
            ("Format column C & resize row", op5),
            ("Add cross-sheet formula", op6),
        ]
    };

    for (idx, (label, ops)) in txns.into_iter().enumerate() {
        let seq = 11 + (idx as u64);
        let wire_ops: Vec<WireOpV1> = ops.iter().map(WireOpV1::from).collect();
        wal_records.push(WireWalRecordV1 {
            seq,
            timestamp_ms: 1_763_500_000_000 + seq,
            label: String::from(label),
            ops: wire_ops,
        });
    }

    let wal_bytes =
        encode_wal_complete(10, &wal_records).map_err(|e| format!("encode_wal_complete: {e:?}"))?;

    // Verify cold decode + WAL scan + replay produces deterministic digest.
    let mut verify_eng = decode_snapshot_any(&snap_bytes)
        .map_err(|e| format!("verify decode: {e:?}"))?
        .engine;
    let scan = scan_wal(&wal_bytes, 10, 16).map_err(|e| format!("verify scan_wal: {e:?}"))?;
    replay_wal_records(&mut verify_eng, &scan.records, 0, true)
        .map_err(|e| format!("verify replay: {e:?}"))?;
    let digest_hex_bytes = to_hex(verify_eng.workbook().logical_digest());
    let digest_hex = String::from_utf8_lossy(&digest_hex_bytes).into_owned();

    fs::write(dir.join("base.rtclsnap"), &snap_bytes)
        .map_err(|e| format!("write base.rtclsnap: {e}"))?;
    fs::write(dir.join("replay.rtclwal"), &wal_bytes)
        .map_err(|e| format!("write replay.rtclwal: {e}"))?;
    fs::write(dir.join("expected_digest.txt"), format!("{digest_hex}\n"))
        .map_err(|e| format!("write expected_digest.txt: {e}"))?;

    let mut hash_lines = String::from(
        "# Frozen SHA-256 manifest for wire v1 schema and compatibility fixtures (D-020).\n",
    );
    for rel in FROZEN_V1_FILES {
        let h = hash_repo_file(&root.join(rel))?;
        hash_lines.push_str(&format!("{h}  {rel}\n"));
    }
    fs::write(root.join(HASHES_FILE), hash_lines)
        .map_err(|e| format!("write {HASHES_FILE}: {e}"))?;

    println!(
        "wrote tests/compat/v1/ (base={}B, wal={}B, expected_digest={digest_hex})",
        snap_bytes.len(),
        wal_bytes.len()
    );
    Ok(())
}

fn hash_repo_file(path: &Path) -> Result<String> {
    let raw = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let is_text = path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| matches!(ext, "rs" | "txt"));
    if is_text {
        let normalised: Vec<u8> = raw.into_iter().filter(|&b| b != b'\r').collect();
        Ok(sha256_hex(&normalised))
    } else {
        Ok(sha256_hex(&raw))
    }
}

// ------------------------------------------------------------ FIPS 180-4 SHA-256

const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64) * 8;
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0x00);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0_u32; 64];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K256[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{word:08x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    #[test]
    fn fips_180_4_sha256_test_vectors_hold() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
