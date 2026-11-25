//! Checking a file that comes from somewhere else: that all of it can be read,
//! and is as its directory says.

use reticula_table::layout::Encoding;
use reticula_table::reader::{BlockBuffer, Dictionary, ReadError, Reader};
use reticula_table::source::{AnySource, Source};

/// Texts of a dictionary that are checked in one step.
const TEXTS: usize = 16_384;

#[derive(Default)]
enum Stage {
    #[default]
    Dictionary,
    Texts(usize),
    Blocks(u32),
}

/// Goes through a table, column by column.
#[derive(Default)]
pub struct Check {
    column: usize,
    stage: Stage,
    buffer: BlockBuffer,
}

impl Check {
    /// Checks some more. Gives whether something is left.
    pub fn step(
        &mut self,
        reader: &mut Reader,
        source: &mut dyn Source,
    ) -> Result<bool, ReadError> {
        // As the session reads, so that they share the code for it.
        let source = &mut AnySource(source);
        if self.column >= reader.columns().len() {
            return Ok(false);
        }
        match self.stage {
            Stage::Dictionary => {
                if reader.load_dictionary(source, self.column)? {
                    self.stage = Stage::Texts(0);
                }
            }
            Stage::Texts(from) => {
                let texts = reader.dictionary(self.column);
                let len = texts.map_or(0, Dictionary::len);
                let to = from.saturating_add(TEXTS).min(len);
                if let Some(texts) = texts {
                    for code in from..to {
                        text(texts.bytes_of(code))?;
                    }
                }
                self.stage = if to < len {
                    Stage::Texts(to)
                } else {
                    Stage::Blocks(0)
                };
            }
            Stage::Blocks(group) => {
                if group < reader.directory().groups() {
                    reader.block(source, self.column, group, &mut self.buffer)?;
                    let codes = reader
                        .columns()
                        .get(self.column)
                        .and_then(|column| column.dictionary)
                        .map(|dictionary| dictionary.entries as usize);
                    block(&self.buffer, codes)?;
                    self.stage = Stage::Blocks(group.saturating_add(1));
                } else {
                    reader.drop_dictionary(self.column);
                    self.column = self.column.saturating_add(1);
                    self.stage = Stage::Dictionary;
                }
            }
        }
        Ok(true)
    }

    /// How much was checked, from 0 to 1.
    pub fn share(&self, reader: &Reader) -> f64 {
        let groups = f64::from(reader.directory().groups().max(1));
        let columns = u32::try_from(reader.columns().len().max(1)).unwrap_or(1);
        let column = u32::try_from(self.column).unwrap_or(0);
        let within = match self.stage {
            Stage::Blocks(group) => f64::from(group) / groups,
            Stage::Dictionary | Stage::Texts(_) => 0.0,
        };
        ((f64::from(column) + within) / f64::from(columns)).min(1.0)
    }
}

fn text(bytes: &[u8]) -> Result<(), ReadError> {
    match std::str::from_utf8(bytes) {
        Ok(_) => Ok(()),
        Err(_) => Err(ReadError::Corrupt("text that is not UTF-8")),
    }
}

fn block(buffer: &BlockBuffer, codes: Option<usize>) -> Result<(), ReadError> {
    let block = buffer.block();
    let blanks: u32 = (0..buffer.rows())
        .map(|row| u32::from(buffer.is_blank(row)))
        .sum();
    if blanks != block.blanks {
        return Err(ReadError::Corrupt("number of blank cells"));
    }
    let mut rows = (0..buffer.rows()).filter(|row| !buffer.is_blank(*row));
    let fits = match block.encoding {
        Encoding::Blank => true,
        Encoding::Packed => {
            let range = block.low.cast_signed()..=block.high.cast_signed();
            rows.all(|row| {
                range.contains(&buffer.int(row))
                    && codes.is_none_or(|entries| buffer.code(row) < entries)
            })
        }
        Encoding::Raw => {
            let range = f64::from_bits(block.low)..=f64::from_bits(block.high);
            rows.all(|row| range.contains(&buffer.float(row)))
        }
        Encoding::Text => {
            return rows.try_for_each(|row| text(buffer.text(row)));
        }
    };
    if fits {
        Ok(())
    } else {
        Err(ReadError::Corrupt("a value outside of its block's range"))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use reticula_core::rng::Rng;
    use reticula_table::reader::Opened;
    use reticula_table::sample::Sample;

    use super::*;

    fn check(file: &[u8]) -> Result<(), ReadError> {
        let mut source = file;
        let Opened::Ready(mut reader) = Reader::open(&mut source, file.len() as u64)? else {
            panic!("all of it is here");
        };
        let mut check = Check::default();
        let mut share = 0.0;
        while check.step(&mut reader, &mut source)? {
            assert!(check.share(&reader) >= share);
            share = check.share(&reader);
        }
        assert!((share - 1.0).abs() < f64::EPSILON);
        Ok(())
    }

    fn file() -> Vec<u8> {
        let mut sample = Sample::new(3_000, 7).unwrap();
        let mut file = Vec::new();
        while sample.step(&mut file).unwrap() {}
        file
    }

    #[test]
    fn a_file_that_is_whole_passes() {
        assert_eq!(check(&file()), Ok(()));
    }

    #[test]
    fn damage_to_what_is_read_is_noticed_wherever_it_is() {
        let whole = file();
        let mut rng = Rng::from_seed(11);
        let mut noticed = 0;
        for _ in 0..200 {
            let mut damaged = whole.clone();
            let at = usize::try_from(rng.next_below(whole.len() as u64)).unwrap();
            damaged[at] ^= 0x10;
            noticed += usize::from(check(&damaged).is_err());
        }
        // All but what lies in pages that nothing is in but zeros at the end.
        assert!(noticed >= 198, "{noticed}");
    }

    #[test]
    fn a_directory_that_tells_of_other_values_is_noticed() {
        use reticula_table::layout::{Directory, TRAILER_LEN, Trailer};
        use sha2::{Digest, Sha256};

        let whole = file();
        let at = whole.len() - usize::try_from(TRAILER_LEN).unwrap();
        let trailer = Trailer::decode(&whole[at..], whole.len() as u64).unwrap();
        let data = usize::try_from(trailer.directory_at).unwrap();
        let decoded = Directory::decode(&whole[data..at], trailer.directory_at).unwrap();
        let rebuilt = |change: &dyn Fn(&mut Directory)| {
            let mut directory = decoded.clone();
            change(&mut directory);
            let mut file = whole[..data].to_vec();
            let mut bytes = Vec::new();
            directory.encode(&mut bytes);
            Trailer {
                directory_sha256: Sha256::digest(&bytes).into(),
                ..trailer
            }
            .encode(&mut bytes);
            file.extend(bytes);
            check(&file)
        };
        assert_eq!(rebuilt(&|_| {}), Ok(()));
        assert!(rebuilt(&|directory| directory.blocks[0].high -= 1).is_err());
        assert!(rebuilt(&|directory| directory.blocks[0].low += 1).is_err());
    }
}
