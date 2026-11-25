//! Tables for tests.

use reticula_table::types::ColumnType;
use reticula_table::writer::{ColumnSpec, TableWriter};

/// A table file with so many bytes of cells, different for each seed: one
/// column of numbers that cannot be packed any tighter.
pub fn table_file(payload: usize, seed: u8) -> Vec<u8> {
    write(payload.div_ceil(8), seed, &[])
}

/// A table file of exactly that length, which has to be 5,000 at least.
#[allow(dead_code, reason = "not every test that has tables needs it")]
pub fn table_file_of(bytes: usize, seed: u8) -> Vec<u8> {
    /// What a column without cells takes besides its name.
    const COLUMN: usize = 58;
    const NAME: usize = 1_000;
    let mut rows = bytes / 8;
    loop {
        // Whole pages, until the rest can be made up of columns without cells.
        rows -= rows.min(512);
        let mut rest = bytes.saturating_sub(write(rows, seed, &[]).len());
        let mut names = Vec::new();
        while rest >= COLUMN {
            let name = (rest - COLUMN).min(NAME);
            names.push(name);
            rest -= COLUMN + name;
        }
        if rest == 0 && !names.is_empty() {
            let file = write(rows, seed, &names);
            assert_eq!(file.len(), bytes);
            return file;
        }
        assert!(rows > 0, "{bytes} bytes are not enough for a table");
    }
}

fn write(rows: usize, seed: u8, names: &[usize]) -> Vec<u8> {
    let mut columns = vec![ColumnSpec {
        name: String::from("n"),
        kind: Some(ColumnType::Integer),
        dictionary: false,
    }];
    columns.extend(names.iter().map(|len| ColumnSpec {
        name: "x".repeat(*len),
        kind: None,
        dictionary: false,
    }));
    let mut writer = TableWriter::new(columns, 65_536).expect("there are few columns");
    let mut file = Vec::new();
    let mut state = u64::from(seed) + 1;
    let mut flush = |writer: &mut TableWriter| {
        while writer.step(&mut file).expect("memory takes everything") {}
    };
    for _ in 0..rows {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        for (index, column) in writer.columns().iter_mut().enumerate() {
            if index == 0 {
                column.push_int(state.cast_signed());
            } else {
                column.push_blank();
            }
        }
        if writer.check_group().expect("there is no text") {
            writer.seal_group().expect("the group is full");
            flush(&mut writer);
        }
    }
    writer
        .seal_group()
        .expect("the columns are as long as each other");
    flush(&mut writer);
    writer.finish();
    flush(&mut writer);
    file
}
