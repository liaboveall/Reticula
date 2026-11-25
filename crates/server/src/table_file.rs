//! What is checked of an uploaded table before anyone is offered it.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use reticula_table::reader::{Opened, ReadError, Reader};
use reticula_table::source::{Source, SourceError};
use thiserror::Error;

const MAGIC: [u8; 4] = *b"RTBL";
const END_MAGIC: [u8; 8] = *b"RTBLEND1";
const HEADER_BYTES: u64 = 16;
const TRAILER_BYTES: u64 = 64;
/// The shortest file worth looking at: a header and a trailer.
pub const MIN_TABLE_FILE_BYTES: u64 = HEADER_BYTES + TRAILER_BYTES;
/// The longest directory the server reads into memory.
const MAX_DIRECTORY_BYTES: u64 = 16 * 1024 * 1024;

/// What a table file holds, by its own account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableShape {
    pub rows: u64,
    pub cols: u32,
    /// The length of the file.
    pub bytes: u64,
}

#[derive(Debug, Error)]
pub enum TableFileError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("this is not a table file")]
    NotATable,
    #[error("the table file is not as long as it says it is")]
    Length,
    #[error("the table has too many columns or groups of rows for this server")]
    Directory,
}

/// Opens a table file and reads what it says about itself.
///
/// The bytes are whatever was uploaded. They are as many as were declared and
/// have the declared SHA-256, which says nothing about what is in them.
pub fn verify_table_file(path: &Path) -> Result<TableShape, TableFileError> {
    let mut file = File::open(path)?;
    let bytes = file.metadata()?.len();
    if bytes < MIN_TABLE_FILE_BYTES {
        return Err(TableFileError::NotATable);
    }
    let mut header = [0; HEADER_BYTES as usize];
    file.read_exact(&mut header)?;
    let mut trailer = [0; TRAILER_BYTES as usize];
    file.seek(SeekFrom::Start(bytes - TRAILER_BYTES))?;
    file.read_exact(&mut trailer)?;
    if header[..4] != MAGIC || trailer[56..] != END_MAGIC {
        return Err(TableFileError::NotATable);
    }
    let directory_at = u64_at(&trailer, 0);
    let directory_len = u64_at(&trailer, 8);
    let claimed = u64_at(&trailer, 48);
    let spanned = directory_at
        .checked_add(directory_len)
        .and_then(|end| end.checked_add(TRAILER_BYTES));
    if claimed != bytes || spanned != Some(bytes) || directory_at < HEADER_BYTES {
        return Err(TableFileError::Length);
    }
    if directory_len > MAX_DIRECTORY_BYTES {
        return Err(TableFileError::Directory);
    }
    let (rows, cols) = directory_shape(&mut file, bytes)?;
    Ok(TableShape { rows, cols, bytes })
}

/// The rows and columns the directory lists, once everything in it was found
/// to be where it can be. The blocks themselves are not read: those who open
/// the table check each page as they come to it.
fn directory_shape(file: &mut File, bytes: u64) -> Result<(u64, u32), TableFileError> {
    match Reader::open(&mut FileSource(file), bytes) {
        Ok(Opened::Ready(table)) => Ok((
            u64::from(table.rows()),
            u32::try_from(table.columns().len()).unwrap_or(u32::MAX),
        )),
        // A file that is all here needs nothing.
        Ok(Opened::Need(_)) | Err(ReadError::Corrupt(_) | ReadError::Missing) => {
            Err(TableFileError::NotATable)
        }
        Err(ReadError::OutOfMemory) => Err(TableFileError::Directory),
        Err(ReadError::Failed) => Err(TableFileError::Io(io::Error::other(
            "the table file could not be read",
        ))),
    }
}

struct FileSource<'a>(&'a mut File);

impl Source for FileSource<'_> {
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
        self.0
            .seek(SeekFrom::Start(at))
            .and_then(|_| self.0.read_exact(into))
            .map_err(|_| SourceError::Failed)
    }
}

fn u64_at(bytes: &[u8; TRAILER_BYTES as usize], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

#[cfg(test)]
#[path = "../tests/support/table_fixture.rs"]
mod table_fixture;

#[cfg(test)]
mod tests {
    use super::*;

    use super::table_fixture::table_file;

    /// Both ends of a table, and nothing between them that makes sense.
    fn husk(payload: usize, directory: usize) -> Vec<u8> {
        let mut bytes = b"RTBL\x01\0\0\0\0\0\0\0\0\0\0\0".to_vec();
        bytes.resize(16 + payload, 7);
        let directory_at = bytes.len() as u64;
        bytes.resize(bytes.len() + directory, 9);
        let len = bytes.len() as u64 + TRAILER_BYTES;
        bytes.extend_from_slice(&directory_at.to_le_bytes());
        bytes.extend_from_slice(&(directory as u64).to_le_bytes());
        bytes.extend_from_slice(&[0; 32]);
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&END_MAGIC);
        bytes
    }

    fn verify(bytes: &[u8]) -> Result<TableShape, TableFileError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("table.part");
        std::fs::write(&path, bytes).unwrap();
        verify_table_file(&path)
    }

    #[test]
    fn a_table_is_as_long_as_it_says_and_has_a_shape() {
        for payload in [0, 1, 4_096, 900_000] {
            let bytes = table_file(payload, 1);
            let shape = verify(&bytes).unwrap();
            assert_eq!(shape.bytes, bytes.len() as u64);
            assert_eq!(shape.rows, payload.div_ceil(8) as u64);
            assert_eq!(shape.cols, 1);
        }
    }

    #[test]
    fn other_files_are_not_tables() {
        let mut magic = table_file(10, 1);
        magic[0] = b'X';
        let mut version = table_file(10, 1);
        version[4] = 9;
        let mut end = table_file(10, 1);
        *end.last_mut().unwrap() = b'2';
        let mut directory = table_file(10, 1);
        let at = directory.len() - 70;
        directory[at] ^= 1;
        let short = husk(0, 0);
        for bytes in [
            &[][..],
            b"RTBL",
            &short[1..],
            &short,
            &husk(5_000, 900),
            &magic,
            &version,
            &end,
            &directory,
        ] {
            assert!(matches!(verify(bytes), Err(TableFileError::NotATable)));
        }
    }

    #[test]
    fn a_length_that_is_not_the_files_is_refused() {
        // Cut in the middle, with both ends intact.
        let whole = husk(1_000, 100);
        let mut cut = whole[..500].to_vec();
        cut.extend_from_slice(&whole[600..]);
        assert!(matches!(verify(&cut), Err(TableFileError::Length)));

        for (at, value) in [(0, u64::MAX), (8, u64::MAX), (0, 0), (48, 1)] {
            let mut bytes = table_file(1_000, 1);
            let trailer = bytes.len() - 64;
            bytes[trailer + at..trailer + at + 8].copy_from_slice(&value.to_le_bytes());
            assert!(matches!(verify(&bytes), Err(TableFileError::Length)));
        }
    }

    #[test]
    fn a_directory_larger_than_the_server_reads_is_refused() {
        let bytes = husk(0, MAX_DIRECTORY_BYTES as usize + 1);
        assert!(matches!(verify(&bytes), Err(TableFileError::Directory)));
    }

    #[test]
    fn a_file_that_is_not_there_is_an_error_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            verify_table_file(&dir.path().join("none")),
            Err(TableFileError::Io(_))
        ));
    }
}
