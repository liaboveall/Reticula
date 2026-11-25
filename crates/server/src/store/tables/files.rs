//! The files of tables: `tables/{workbook}/{table}.part` while one is being
//! uploaded, and `.rtbl` once it has been checked.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use uuid::Uuid;

use super::super::snapshot::{hash_file, sync_directory};
use crate::table_file::{TableFileError, TableShape, verify_table_file};

const PART: &str = "part";
const READY: &str = "rtbl";

pub(super) fn directory(data_dir: &Path, workbook: Uuid) -> PathBuf {
    data_dir.join("tables").join(workbook.to_string())
}

pub(super) fn part_path(data_dir: &Path, workbook: Uuid, table: Uuid) -> PathBuf {
    directory(data_dir, workbook).join(format!("{table}.{PART}"))
}

pub(super) fn ready_path(data_dir: &Path, workbook: Uuid, table: Uuid) -> PathBuf {
    directory(data_dir, workbook).join(format!("{table}.{READY}"))
}

/// How much of an upload is there, and when anything last arrived. Moves nothing.
pub(super) fn arrived(
    data_dir: &Path,
    workbook: Uuid,
    table: Uuid,
) -> io::Result<(u64, Option<SystemTime>)> {
    // The second when the server stopped between checking the file and saying so.
    for path in [
        part_path(data_dir, workbook, table),
        ready_path(data_dir, workbook, table),
    ] {
        match fs::metadata(path) {
            Ok(found) => return Ok((found.len(), found.modified().ok())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok((0, None))
}

/// Gets the file of a table that is being uploaded, as its row says, ready to be
/// added to or checked. Returns its length.
pub(super) fn take_up(data_dir: &Path, workbook: Uuid, table: Uuid) -> io::Result<u64> {
    let part = part_path(data_dir, workbook, table);
    if !part.try_exists()? {
        match fs::rename(ready_path(data_dir, workbook, table), &part) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(arrived(data_dir, workbook, table)?.0)
}

/// Opens an unfinished upload to add to it.
pub(super) fn open_part(data_dir: &Path, workbook: Uuid, table: Uuid) -> io::Result<File> {
    fs::create_dir_all(directory(data_dir, workbook))?;
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(part_path(data_dir, workbook, table))
}

pub(super) fn remove(data_dir: &Path, workbook: Uuid, table: Uuid) -> io::Result<()> {
    for path in [
        part_path(data_dir, workbook, table),
        ready_path(data_dir, workbook, table),
    ] {
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    // The directory stays: another table of the workbook may be on its way there.
    Ok(())
}

pub(super) enum FinishError {
    Io(io::Error),
    /// Not all bytes have arrived, only these many.
    Short(u64),
    /// The bytes are not those of the table, and have been thrown away.
    Wrong(String),
}

impl From<io::Error> for FinishError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Checks an upload and makes it the file of the table.
pub(super) fn finish(
    data_dir: &Path,
    workbook: Uuid,
    table: Uuid,
    bytes: u64,
    sha256: &[u8],
) -> Result<TableShape, FinishError> {
    let length = take_up(data_dir, workbook, table)?;
    if length < bytes {
        return Err(FinishError::Short(length));
    }
    let part = part_path(data_dir, workbook, table);
    let mut file = OpenOptions::new().read(true).write(true).open(&part)?;
    let checked = if length == bytes && hash_file(&mut file)?.as_slice() == sha256 {
        verify_table_file(&part).and_then(|shape| {
            if shape.bytes == bytes {
                Ok(shape)
            } else {
                Err(TableFileError::Length)
            }
        })
    } else {
        file.set_len(0)?;
        return Err(FinishError::Wrong(
            "the table file does not have the declared checksum".to_owned(),
        ));
    };
    let shape = match checked {
        Ok(shape) => shape,
        Err(TableFileError::Io(error)) => return Err(error.into()),
        Err(wrong) => {
            file.set_len(0)?;
            return Err(FinishError::Wrong(wrong.to_string()));
        }
    };
    file.sync_all()?;
    drop(file);
    fs::rename(&part, ready_path(data_dir, workbook, table))?;
    sync_directory(&directory(data_dir, workbook))?;
    Ok(shape)
}

/// The files of tables in a data directory, by workbook and table. Names that
/// are not of the server's making are left out.
pub(super) fn list(data_dir: &Path) -> io::Result<Files> {
    let mut found = Files::new();
    list_into(&data_dir.join("tables"), &mut found)?;
    Ok(found)
}

/// Files by workbook and table.
pub(super) type Files = HashMap<(Uuid, Uuid), Vec<PathBuf>>;

fn list_into(tables: &Path, found: &mut Files) -> io::Result<()> {
    for entry in entries(tables)? {
        let entry = entry?;
        let Some(workbook) = named(&entry).and_then(|name| name.parse::<Uuid>().ok()) else {
            continue;
        };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(entry.path())? {
            let file = file?;
            let table = named(&file)
                .as_deref()
                .and_then(|name| name.split_once('.'))
                .filter(|(_, kind)| [PART, READY].contains(kind))
                .and_then(|(table, _)| table.parse::<Uuid>().ok());
            if let Some(table) = table {
                found
                    .entry((workbook, table))
                    .or_default()
                    .push(file.path());
            }
        }
    }
    Ok(())
}

/// The entries of a directory, which need not be there.
fn entries(directory: &Path) -> io::Result<impl Iterator<Item = io::Result<fs::DirEntry>>> {
    let entries = match fs::read_dir(directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        other => Some(other?),
    };
    Ok(entries.into_iter().flatten())
}

fn named(entry: &fs::DirEntry) -> Option<String> {
    entry.file_name().into_string().ok()
}

/// Where files without a row are kept for a while, in a directory for each time
/// some were found, by the second.
fn removed_directory(data_dir: &Path) -> PathBuf {
    data_dir.join("tables-removed")
}

fn removals(data_dir: &Path) -> io::Result<Vec<(SystemTime, PathBuf)>> {
    let mut found = Vec::new();
    for entry in entries(&removed_directory(data_dir))? {
        let entry = entry?;
        if let Some(seconds) = named(&entry).and_then(|name| name.parse::<u64>().ok())
            && let Some(at) = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
        {
            found.push((at, entry.path()));
        }
    }
    found.sort_unstable();
    Ok(found)
}

/// Puts files aside that are in no row.
pub(super) fn put_aside(data_dir: &Path, files: &Files) -> io::Result<()> {
    let seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    for ((workbook, _), paths) in files {
        let aside = removed_directory(data_dir)
            .join(seconds.to_string())
            .join(workbook.to_string());
        fs::create_dir_all(&aside)?;
        for path in paths {
            if let Some(name) = path.file_name() {
                fs::rename(path, aside.join(name))?;
            }
        }
        // Only goes when it is empty.
        let _ = fs::remove_dir(directory(data_dir, *workbook));
    }
    Ok(())
}

/// Puts the files of these tables back, the ones put aside last. Returns how many.
pub(super) fn put_back(data_dir: &Path, tables: &HashSet<(Uuid, Uuid)>) -> io::Result<usize> {
    let mut back = 0;
    let mut wanted = tables.clone();
    for (_, removal) in removals(data_dir)?.into_iter().rev() {
        let mut files = Files::new();
        list_into(&removal, &mut files)?;
        for (table, paths) in files {
            if !wanted.remove(&table) {
                continue;
            }
            let place = directory(data_dir, table.0);
            fs::create_dir_all(&place)?;
            for path in paths {
                if let Some(name) = path.file_name() {
                    fs::rename(&path, place.join(name))?;
                }
            }
            back += 1;
            // These only go when they are empty.
            let _ = fs::remove_dir(removal.join(table.0.to_string()));
            let _ = fs::remove_dir(&removal);
        }
    }
    Ok(back)
}

/// Removes what was put aside longer ago than that. Returns how often it had been.
pub(super) fn purge(data_dir: &Path, kept: Duration) -> io::Result<usize> {
    let mut purged = 0;
    for (at, removal) in removals(data_dir)? {
        let is_old = SystemTime::now()
            .duration_since(at)
            .is_ok_and(|since| since >= kept);
        if is_old {
            fs::remove_dir_all(removal)?;
            purged += 1;
        }
    }
    Ok(purged)
}
