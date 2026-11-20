//! Writes immutable RTCLSNAP files before publishing their PostgreSQL metadata.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use reticula_core::ops::Workbook;
use reticula_formula::binder::BoundFormula;
use reticula_sync::ReplicaWatermark;
use reticula_wire::snapshot::{SnapshotEncoder, SnapshotStep, decode_snapshot_any};
use reticula_wire::v1::WireReplicaWatermarkV1;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use tempfile::NamedTempFile;
use uuid::Uuid;

use super::{MAIN_BRANCH, StoreError, integer};

const FORMAT_VERSION: i32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct SnapshotFile {
    pub(super) workbook_id: Uuid,
    pub(super) seq: i64,
    format_version: i32,
    storage_key: String,
    size_bytes: i64,
    sha256: Vec<u8>,
}

impl SnapshotFile {
    pub fn seq(&self) -> u64 {
        self.seq.unsigned_abs()
    }

    pub fn size(&self) -> u64 {
        self.size_bytes.unsigned_abs()
    }

    /// Opens a verified snapshot and rewinds it for streaming or decoding.
    pub fn open(&self, data_dir: &Path) -> Result<File, StoreError> {
        if self.storage_key != storage_key(self.workbook_id, self.seq) {
            return Err(StoreError::Invalid("snapshot path"));
        }
        let mut file = File::open(data_dir.join(&self.storage_key))?;
        if file.metadata()?.len() != self.size() || hash_file(&mut file)?.as_slice() != self.sha256
        {
            return Err(StoreError::Invalid("snapshot checksum or length"));
        }
        file.rewind()?;
        Ok(file)
    }

    pub(super) fn read_workbook(
        &self,
        data_dir: &Path,
    ) -> Result<Workbook<BoundFormula>, StoreError> {
        let mut bytes = Vec::new();
        self.open(data_dir)?.read_to_end(&mut bytes)?;
        let decoded =
            decode_snapshot_any(&bytes).map_err(|_| StoreError::Invalid("snapshot encoding"))?;
        if i64::from(decoded.format_version) != i64::from(self.format_version) {
            return Err(StoreError::Invalid("snapshot version"));
        }
        Ok(decoded.engine.workbook().clone())
    }

    pub(super) fn write(
        data_dir: &Path,
        doc: Uuid,
        seq: i64,
        workbook: &Workbook<BoundFormula>,
        watermarks: Vec<ReplicaWatermark>,
    ) -> Result<Self, StoreError> {
        let storage_key = storage_key(doc, seq);
        let parent = data_dir.join("snapshots").join(doc.to_string());
        fs::create_dir_all(&parent)?;
        let mut temp = NamedTempFile::new_in(&parent)?;
        let wire_watermarks = watermarks
            .into_iter()
            .map(|wm| WireReplicaWatermarkV1 {
                replica: wm.replica,
                last_client_seq: wm.last_client_seq,
                max_id_counter: wm.max_id_counter,
            })
            .collect();
        let mut encoder = SnapshotEncoder::new(workbook, wire_watermarks);
        let length = loop {
            match encoder
                .encode_step(workbook, 8)
                .map_err(|_| StoreError::Invalid("snapshot encoding"))?
            {
                SnapshotStep::Progress => {}
                SnapshotStep::Chunk { offset, bytes } => {
                    temp.seek(SeekFrom::Start(offset))?;
                    temp.write_all(&bytes)?;
                }
                SnapshotStep::Finished {
                    header_bytes,
                    total_len,
                } => {
                    temp.rewind()?;
                    temp.write_all(&header_bytes)?;
                    temp.as_file().set_len(total_len)?;
                    break total_len;
                }
            }
        };
        temp.as_file().sync_all()?;
        temp.rewind()?;
        let record = Self {
            workbook_id: doc,
            seq,
            format_version: FORMAT_VERSION,
            storage_key,
            size_bytes: integer(length, "snapshot length")?,
            sha256: hash_file(temp.as_file_mut())?.to_vec(),
        };
        match temp.persist_noclobber(data_dir.join(&record.storage_key)) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                // An earlier writer may have finished the file before recording its metadata.
                record.open(data_dir)?;
            }
            Err(error) => return Err(error.error.into()),
        }
        sync_directory(&parent)?;
        Ok(record)
    }

    pub(super) async fn publish(&self, pool: &PgPool) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO snapshots (workbook_id, branch_id, seq, format_version, storage_key, size_bytes, sha256)
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
            self.workbook_id, MAIN_BRANCH, self.seq, self.format_version,
            self.storage_key, self.size_bytes, &self.sha256,
        ).execute(pool).await?;
        if at(pool, self.workbook_id, self.seq).await?.as_ref() != Some(self) {
            return Err(StoreError::Invalid("snapshot metadata mismatch"));
        }
        Ok(())
    }
}

pub(super) async fn at(
    pool: &PgPool,
    doc: Uuid,
    seq: i64,
) -> Result<Option<SnapshotFile>, StoreError> {
    Ok(sqlx::query_as!(
        SnapshotFile,
        "SELECT workbook_id, seq, format_version, storage_key, size_bytes, sha256 FROM snapshots
         WHERE workbook_id = $1 AND branch_id = $2 AND seq = $3",
        doc,
        MAIN_BRANCH,
        seq,
    )
    .fetch_optional(pool)
    .await?)
}

pub(super) async fn latest(
    db: &mut PgConnection,
    doc: Uuid,
) -> Result<Option<SnapshotFile>, StoreError> {
    Ok(sqlx::query_as!(
        SnapshotFile,
        "SELECT workbook_id, seq, format_version, storage_key, size_bytes, sha256 FROM snapshots
         WHERE workbook_id = $1 AND branch_id = $2 ORDER BY seq DESC LIMIT 1",
        doc,
        MAIN_BRANCH,
    )
    .fetch_optional(db)
    .await?)
}

fn storage_key(doc: Uuid, seq: i64) -> String {
    format!("snapshots/{doc}/{seq}.rtclsnap")
}

fn hash_file(file: &mut File) -> io::Result<[u8; 32]> {
    let mut sha = Sha256::new();
    let mut buffer = [0; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        sha.update(&buffer[..read]);
    }
    Ok(sha.finalize().into())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        // Include the new workbook and snapshots directory entries on a fresh volume.
        for ancestor in path.ancestors().take(3) {
            File::open(ancestor)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
