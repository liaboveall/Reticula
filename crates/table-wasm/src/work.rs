//! What reads all of a file or writes one, step by step.

use reticula_table::csv;
use reticula_table::parquet;
use reticula_table::reader::{Opened, Reader};
use reticula_table::sample::Sample;
use reticula_table::writer::WriteError;
use sha2::{Digest, Sha256};
use wasm_bindgen::prelude::*;

use crate::check::Check;
use crate::file::{JsFile, TableFile, from_offset, offset};
use crate::json::{Value, json};
use crate::outcome::{TableStep, failed};
use crate::table::open;
use crate::wire::{ImportReport, TableTrouble};
use crate::{csv as csv_parts, parquet as parquet_parts};

/// One thing that takes a while. What it is, its constructor says.
#[wasm_bindgen]
pub struct TableWork(Work);

enum Work {
    Csv {
        file: JsFile,
        import: Box<Result<csv::Import, csv::ImportError>>,
    },
    Parquet {
        source: JsFile,
        len: u64,
        file: JsFile,
        columns: Option<Vec<usize>>,
        stage: Stage,
    },
    Sample {
        file: JsFile,
        rows: u64,
        sample: Result<Sample, WriteError>,
    },
    Check {
        file: JsFile,
        len: u64,
        reader: Option<Box<Reader>>,
        check: Check,
    },
    Digest(Sha256),
}

enum Stage {
    Closed,
    Open(parquet::ParquetFile),
    Importing(Box<parquet::Import>),
}

#[wasm_bindgen]
impl TableWork {
    /// Writes a table from a text that is handed in piece by piece. `head` is the
    /// start of it: as much as `csvHeadBytes`, or all there is. The settings are
    /// `CsvOptions` in JSON.
    #[must_use]
    pub fn csv(file: TableFile, settings: &str, head: &[u8]) -> Self {
        Self(Work::Csv {
            file: JsFile(file),
            import: Box::new(csv::Import::new(&csv_parts::options(settings), head)),
        })
    }

    /// Writes a table from a Parquet file of that length, which has to be opened:
    /// the columns in a list in JSON, or all.
    #[must_use]
    pub fn parquet(source: TableFile, len: f64, file: TableFile, columns: &str) -> Self {
        let chosen = Value::read(columns);
        let chosen = chosen.items().iter().map(|(_, column)| column.number());
        Self(Work::Parquet {
            source: JsFile(source),
            len: from_offset(len),
            file: JsFile(file),
            columns: (!columns.is_empty()).then(|| chosen.collect()),
            stage: Stage::Closed,
        })
    }

    /// Makes up a table of so many rows.
    #[must_use]
    pub fn sample(file: TableFile, rows: f64, seed: u32) -> Self {
        let rows = from_offset(rows);
        Self(Work::Sample {
            file: JsFile(file),
            rows,
            sample: Sample::new(rows, u64::from(seed)),
        })
    }

    /// Checks everything that a table file of that length holds.
    #[must_use]
    pub fn check(file: TableFile, len: f64) -> Self {
        Self(Work::Check {
            file: JsFile(file),
            len: from_offset(len),
            reader: None,
            check: Check::default(),
        })
    }

    /// Works out the SHA-256 of what is handed in piece by piece.
    #[must_use]
    pub fn digest() -> Self {
        Self(Work::Digest(Sha256::default()))
    }

    /// Takes the next piece, or that there is none when it is empty. Whether it
    /// did: not much can wait.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        match &mut self.0 {
            Work::Csv { import, .. } => match import.as_mut() {
                Ok(import) if bytes.is_empty() => {
                    import.end_of_input();
                    true
                }
                Ok(import) => import.feed(bytes).is_ok(),
                // Its step says what is wrong.
                Err(_) => true,
            },
            Work::Digest(digest) => {
                digest.update(bytes);
                true
            }
            _ => false,
        }
    }

    /// Finds out what is in a Parquet file.
    pub fn open(&mut self) -> TableStep {
        if let Work::Parquet {
            source, len, stage, ..
        } = &mut self.0
        {
            match parquet::open(source, *len) {
                Ok(parquet::Opened::Ready(file)) => *stage = Stage::Open(file),
                Ok(parquet::Opened::Need(_)) => return failed(TableTrouble::Io),
                Err(error) => return failed(error),
            }
        }
        TableStep::Done
    }

    /// Does some more.
    pub fn step(&mut self) -> TableStep {
        match &mut self.0 {
            Work::Csv { file, import } => match import.as_mut() {
                Ok(import) => match import.step(file) {
                    Ok(csv::Step::NeedInput) => TableStep::NeedInput,
                    Ok(csv::Step::Working) => TableStep::Working,
                    Ok(csv::Step::Restart) => TableStep::Restart,
                    Ok(csv::Step::Done) => TableStep::Done,
                    Err(error) => failed(error),
                },
                Err(error) => failed(*error),
            },
            Work::Parquet {
                source,
                file,
                columns,
                stage,
                ..
            } => match stage.advance(source, file, columns) {
                Ok(parquet::Step::Working) => TableStep::Working,
                Ok(parquet::Step::Restart) => TableStep::Restart,
                Ok(parquet::Step::Done) => TableStep::Done,
                Ok(parquet::Step::Need(_)) => failed(TableTrouble::Io),
                Err(error) => failed(error),
            },
            Work::Sample { file, sample, .. } => {
                let more = match sample {
                    Ok(sample) => sample.step(file),
                    Err(error) => Err(*error),
                };
                more.map_or_else(failed, TableStep::of)
            }
            Work::Check {
                file,
                len,
                reader,
                check,
            } => match reader {
                None => match open(file, *len) {
                    Ok(Opened::Ready(opened)) => {
                        *reader = Some(opened);
                        TableStep::Working
                    }
                    Ok(Opened::Need(_)) => failed(TableTrouble::Io),
                    Err(error) => failed(error),
                },
                Some(reader) => check.step(reader, file).map_or_else(failed, TableStep::of),
            },
            Work::Digest(_) => TableStep::Done,
        }
    }

    /// How much is done, from 0 to 1, rows that were read or made, and which time
    /// the file is being read, from 1 on. How much of a text is not known.
    #[must_use]
    pub fn progress(&self) -> Vec<f64> {
        let (rows, of, pass) = self.done();
        let share = match &self.0 {
            Work::Check {
                reader: Some(reader),
                check,
                ..
            } => check.share(reader),
            _ => offset(rows) / offset(of.max(1)),
        };
        vec![share, offset(rows), f64::from(pass)]
    }

    /// What became of it, once it is done: an `ImportReport`, or a digest in
    /// hexadecimal digits. Of a Parquet file that is open, what is in it.
    #[must_use]
    #[wasm_bindgen(unchecked_return_type = "Json<ImportReport | ParquetInfo>")]
    pub fn report(&mut self) -> String {
        match &mut self.0 {
            Work::Csv { import, .. } => {
                let report = import.as_ref().as_ref().ok().and_then(csv_parts::report);
                json(&report.unwrap_or_default())
            }
            Work::Parquet {
                stage: Stage::Importing(import),
                ..
            } => json(&parquet_parts::report(import)),
            Work::Parquet {
                stage: Stage::Open(file),
                ..
            } => json(&parquet_parts::info(file)),
            Work::Digest(digest) => hex(&digest.finalize_reset()),
            _ => json(&ImportReport::default()),
        }
    }
}

impl TableWork {
    /// Rows that are done and those there are, and which time it is.
    fn done(&self) -> (u64, u64, u32) {
        match &self.0 {
            Work::Csv { import, .. } => import.as_ref().as_ref().map_or((0, 0, 1), |import| {
                let progress = import.progress();
                (progress.rows, 0, progress.pass)
            }),
            Work::Parquet {
                stage: Stage::Importing(import),
                ..
            } => {
                let progress = import.progress();
                (
                    progress.rows,
                    progress.of,
                    progress.restarts.saturating_add(1),
                )
            }
            Work::Sample {
                rows,
                sample: Ok(sample),
                ..
            } => (sample.made(), *rows, 1),
            _ => (0, 0, 1),
        }
    }
}

impl Stage {
    fn advance(
        &mut self,
        source: &mut JsFile,
        sink: &mut JsFile,
        columns: &mut Option<Vec<usize>>,
    ) -> Result<parquet::Step, parquet::ParquetError> {
        match std::mem::replace(self, Self::Closed) {
            Self::Closed => return Err(parquet::ParquetError::Source),
            Self::Open(file) => {
                let options = parquet::Options {
                    columns: columns.take(),
                };
                *self = Self::Importing(Box::new(parquet::Import::new(file, &options)?));
            }
            importing @ Self::Importing(_) => *self = importing,
        }
        match self {
            Self::Importing(import) => import.step(source, sink),
            Self::Closed | Self::Open(_) => Ok(parquet::Step::Working),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 15])
        .map(|digit| char::from(if digit < 10 { b'0' } else { b'a' - 10 } + digit))
        .collect()
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    #[test]
    fn a_digest_is_the_same_however_the_file_is_cut() {
        // Of FIPS 180-2, appendix B.2.
        let file = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        let expected = "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1";
        for cut in 0..file.len() {
            let mut digest = Sha256::default();
            digest.update(&file[..cut]);
            digest.update(&file[cut..]);
            assert_eq!(hex(&digest.finalize()), expected);
        }
    }

    /// With files as JavaScript has them.
    #[cfg(target_arch = "wasm32")]
    mod with_files {
        use wasm_bindgen_test::wasm_bindgen_test as test;

        use reticula_table::source::Source;

        use super::*;
        use crate::outcome::last_trouble;
        use crate::table::Table;

        #[wasm_bindgen]
        extern "C" {
            #[wasm_bindgen(js_name = eval)]
            fn file(code: &str) -> TableFile;
            #[wasm_bindgen(method, getter)]
            fn size(this: &TableFile) -> f64;
            #[wasm_bindgen(method, setter)]
            fn set_bytes(this: &TableFile, bytes: &[u8]);
        }

        fn empty() -> TableFile {
            limited(u32::MAX)
        }

        /// A file with room for so many blocks.
        fn limited(blocks: u32) -> TableFile {
            file(&format!(
                "({{
                    bytes: new Uint8Array(),
                    left: {blocks},
                    get size() {{ return this.bytes.length; }},
                    append(more) {{
                        if (this.left-- <= 0) return false;
                        const grown = new Uint8Array(this.size + more.length);
                        grown.set(this.bytes);
                        grown.set(more, this.size);
                        this.bytes = grown;
                        return true;
                    }},
                    readAt(at, into) {{
                        into.set(this.bytes.slice(at, at + into.length));
                        return at + into.length <= this.size ? 0 : 2;
                    }},
                }})"
            ))
        }

        /// Imports a text that is handed in piece by piece.
        fn import(text: &str, piece: usize) -> (TableFile, TableWork, TableStep) {
            let written = empty();
            let same = TableFile::from(JsValue::from(&written));
            let mut work = TableWork::csv(same, "{}", text.as_bytes());
            assert_eq!(work.open(), TableStep::Done);
            let mut pieces = text.as_bytes().chunks(piece);
            loop {
                match work.step() {
                    TableStep::NeedInput => assert!(work.feed(pieces.next().unwrap_or(&[]))),
                    TableStep::Working => {}
                    step => return (written, work, step),
                }
            }
        }

        #[test]
        fn a_text_becomes_a_table() {
            for piece in [1, 5, 1_000] {
                let (written, mut work, step) = import("n,t\n1,x\n2,\"y z\"\n3,\n", piece);
                assert_eq!(step, TableStep::Done, "{}", last_trouble());
                assert_eq!(work.progress()[1..], [3.0, 1.0]);
                assert!(
                    work.report()
                        .contains(r#""name":"t","type":{"kind":"text"},"blanks":1"#)
                );
                let len = written.size();
                let mut table = Table::new(Some(written), len);
                assert_eq!(table.open(), TableStep::Done);
                assert_eq!(table.to_top(), 3);
                assert_eq!(table.text(1, 1).as_deref(), Some("y z"));
                assert_eq!(table.text(2, 0).as_deref(), Some("3"));
            }
        }

        #[test]
        fn what_is_wrong_with_a_text_is_said_at_the_end() {
            let (_, _, step) = import("a\n\"open\n", 3);
            assert_eq!(step, TableStep::Failed);
            assert_eq!(last_trouble(), r#"{"reason":"unclosedQuote","row":2}"#);
            let (_, _, step) = import("", 3);
            assert_eq!(step, TableStep::Failed);
            assert_eq!(last_trouble(), r#"{"reason":"empty"}"#);
        }

        #[test]
        fn a_table_that_is_made_up_passes_the_check() {
            let written = made_up(300.0);
            let mut check = TableWork::check(same(&written), written.size());
            let mut step = check.step();
            while step == TableStep::Working {
                step = check.step();
            }
            assert_eq!(step, TableStep::Done);
            let mut digest = TableWork::digest();
            assert!(digest.feed(b"abc"));
            assert!(digest.report().starts_with("ba7816bf8f01cfea"));
        }

        fn same(file: &TableFile) -> TableFile {
            TableFile::from(JsValue::from(file))
        }

        fn made_up(rows: f64) -> TableFile {
            let written = empty();
            let mut sample = TableWork::sample(same(&written), rows, 7);
            while sample.step() == TableStep::Working {}
            written
        }

        /// Answers the requests that a table has, with so many at once. Whether
        /// there were any.
        fn answer(table: &mut Table, file: &TableFile, requests: usize) -> bool {
            let plan = table.plan(requests);
            for request in plan.as_chunks::<3>().0 {
                let [block, from, until] = *request;
                let mut bytes = vec![0; from_offset(until - from) as usize];
                JsFile(same(file))
                    .read(from_offset(from), &mut bytes)
                    .unwrap();
                assert_ne!(table.take(from, &bytes), TableStep::Failed);
                table.ended(block as u32);
            }
            !plan.is_empty()
        }

        fn opened(table: &mut Table, file: &TableFile) {
            while table.open() != TableStep::Done {
                assert!(table.is_waiting() && answer(table, file, 0));
            }
        }

        /// Has pages come, in turn with what they need. How many did.
        fn pages(table: &mut Table, file: &TableFile) -> usize {
            let mut pages = 0;
            loop {
                while let Some(page) = table.next_page() {
                    assert!(!page.is_empty());
                    pages += 1;
                }
                if !answer(table, file, 0) {
                    return pages;
                }
            }
        }

        /// A screen of rows from a row on, and as many below.
        fn look(table: &mut Table, row: u32) {
            table.subscribe(1, 2, true, &[0, row, 128, 0, 16, row, row + 64, 0, 16]);
        }

        #[test]
        fn pages_that_wait_are_had_from_the_server_once_there_is_no_log() {
            // However much of what they need was in the log.
            let written = made_up(60_000.0);
            let len = written.size();
            for room in 4..10 {
                let log = limited(room);
                let mut table = Table::new(None, len);
                table.fetch(Some(same(&log)), 0.0);
                opened(&mut table, &written);
                look(&mut table, 30_016);
                assert_eq!(pages(&mut table, &written), 2, "room for {room}");
            }
            // The same when it is taken away in between.
            for step in 0..4 {
                let mut table = Table::new(None, len);
                table.fetch(Some(empty()), 0.0);
                opened(&mut table, &written);
                look(&mut table, 30_016);
                let mut sent = 0;
                for at in 0..=step {
                    while table.next_page().is_some() {
                        sent += 1;
                    }
                    if at < step {
                        answer(&mut table, &written, 0);
                    }
                }
                table.leave_log();
                assert_eq!(sent + pages(&mut table, &written), 2, "after {step}");
            }
        }

        #[test]
        fn a_file_that_is_gone_comes_from_the_server_and_from_the_log_of_it() {
            let written = made_up(60_000.0);
            let len = written.size();
            let log = empty();
            let mut seeded = Table::new(None, len);
            seeded.fetch(Some(same(&log)), 0.0);
            while answer(&mut seeded, &written, 4) {}
            for has_log in [false, true] {
                let gone = made_up(60_000.0);
                let mut table = Table::new(Some(same(&gone)), len);
                table.fetch(None, 0.0);
                assert_eq!(table.open(), TableStep::Done);
                look(&mut table, 0);
                assert_eq!(pages(&mut table, &written), 2);
                assert_eq!(table.have().to_bits(), len.to_bits());
                gone.set_bytes(&[]);
                look(&mut table, 30_016);
                // What is waited for is not forgotten when the log is there.
                assert_eq!(table.next_page(), None);
                let end = table.fetch(has_log.then(|| same(&log)), log.size());
                assert_eq!(end > 0.0, has_log);
                assert_eq!(table.have() == len, has_log);
                assert_eq!(answer(&mut table, &written, 0), !has_log);
                assert_eq!(pages(&mut table, &written), 2);
            }
        }

        #[test]
        fn too_much_to_copy_is_said_whatever_was_missing_before() {
            let row = format!("{}\n", "x".repeat(1_000_000));
            let (written, _, step) = import(&format!("t\n{}", row.repeat(70)), 1 << 20);
            assert_eq!(step, TableStep::Done, "{}", last_trouble());
            let mut table = Table::new(None, written.size());
            table.fetch(None, 0.0);
            opened(&mut table, &written);
            table.begin_copy(0, 70, 0, 1);
            let mut waited = 0;
            let step = loop {
                match table.step_copy() {
                    TableStep::Need => {
                        waited += 1;
                        assert!(table.is_waiting() && answer(&mut table, &written, 0));
                    }
                    TableStep::Working => {}
                    step => break step,
                }
            };
            assert!(waited > 0);
            assert_eq!(step, TableStep::Failed);
            assert!(!table.is_waiting());
            assert!(last_trouble().contains("limit"), "{}", last_trouble());
        }

        #[test]
        fn a_table_comes_from_a_server_block_by_block() {
            let written = made_up(60_000.0);
            let len = written.size();
            let mut table = Table::new(None, len);
            let log = empty();
            table.fetch(Some(same(&log)), 0.0);
            opened(&mut table, &written);
            assert!(table.have() < len);
            // Pages come once what they need is here.
            look(&mut table, 0);
            assert_eq!(pages(&mut table, &written), 2);
            while answer(&mut table, &written, 4) {}
            assert_eq!(table.have().to_bits(), len.to_bits());
            // What was kept is here the next time.
            let mut again = Table::new(None, len);
            let log_len = log.size();
            assert_eq!(again.fetch(Some(log), log_len).to_bits(), log_len.to_bits());
            assert_eq!(again.have().to_bits(), len.to_bits());
            assert_eq!(again.open(), TableStep::Done);
            assert_eq!(again.text(5, 0), table.text(5, 0));
        }
    }
}
