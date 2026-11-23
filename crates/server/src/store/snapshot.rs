//! Writes immutable RTCLSNAP files before publishing their PostgreSQL metadata.
//!
//! `MAIN_BRANCH` snapshots remain at `snapshots/{doc}/{seq}.rtclsnap` for full
//! backward compatibility; non-main branches store isolated snapshots under
//! `snapshots/{doc}/branches/{branch}/{seq}.rtclsnap`.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use reticula_core::ops::Workbook;
use reticula_formula::binder::BoundFormula;
use reticula_sync::{CommittedPermute, ReplicaWatermark};
use reticula_wire::snapshot::{SnapshotEncoder, SnapshotStep, decode_snapshot_any};
use reticula_wire::v1::WireReplicaWatermarkV1;
use reticula_wire::v3::WireCommittedPermuteV3;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use tempfile::NamedTempFile;
use uuid::Uuid;

use super::{MAIN_BRANCH, StoreError, integer};

const FORMAT_VERSION: i32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct SnapshotFile {
    pub(super) workbook_id: Uuid,
    pub(super) branch_id: Uuid,
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

    pub fn branch_id(&self) -> Uuid {
        self.branch_id
    }

    pub(super) fn storage_key(&self) -> &str {
        &self.storage_key
    }

    /// Opens a verified snapshot and rewinds it for streaming or decoding.
    pub fn open(&self, data_dir: &Path) -> Result<File, StoreError> {
        if self.storage_key != storage_key_branch(self.workbook_id, self.branch_id, self.seq) {
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

    pub(super) fn read_snapshot(
        &self,
        data_dir: &Path,
    ) -> Result<(Workbook<BoundFormula>, Vec<CommittedPermute>), StoreError> {
        let mut bytes = Vec::new();
        self.open(data_dir)?.read_to_end(&mut bytes)?;
        let decoded =
            decode_snapshot_any(&bytes).map_err(|_| StoreError::Invalid("snapshot encoding"))?;
        if i64::from(decoded.format_version) != i64::from(self.format_version) {
            return Err(StoreError::Invalid("snapshot version"));
        }
        let permutes = decoded
            .permutes
            .iter()
            .map(CommittedPermute::from)
            .collect();
        Ok((decoded.engine.workbook().clone(), permutes))
    }

    pub(super) fn write(
        data_dir: &Path,
        doc: Uuid,
        seq: i64,
        workbook: &Workbook<BoundFormula>,
        watermarks: Vec<ReplicaWatermark>,
        permutes: &[CommittedPermute],
    ) -> Result<Self, StoreError> {
        Self::write_branch(
            data_dir,
            doc,
            MAIN_BRANCH,
            seq,
            workbook,
            watermarks,
            permutes,
        )
    }

    pub(super) fn write_branch(
        data_dir: &Path,
        doc: Uuid,
        branch_id: Uuid,
        seq: i64,
        workbook: &Workbook<BoundFormula>,
        watermarks: Vec<ReplicaWatermark>,
        permutes: &[CommittedPermute],
    ) -> Result<Self, StoreError> {
        let storage_key = storage_key_branch(doc, branch_id, seq);
        let parent = if branch_id == MAIN_BRANCH {
            data_dir.join("snapshots").join(doc.to_string())
        } else {
            data_dir
                .join("snapshots")
                .join(doc.to_string())
                .join("branches")
                .join(branch_id.to_string())
        };
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
        let wire_permutes = permutes.iter().map(WireCommittedPermuteV3::from).collect();
        let mut encoder =
            SnapshotEncoder::new_with_permutes(workbook, wire_watermarks, wire_permutes);
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
            branch_id,
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

    pub(super) async fn publish(&self, pool: &PgPool) -> Result<bool, StoreError> {
        let mut db = pool.begin().await?;
        let inserted = self.publish_in_tx(&mut db).await?;
        if inserted {
            sqlx::query(
                "UPDATE users u SET storage_bytes = u.storage_bytes + $2
                 FROM workbooks w WHERE w.id = $1 AND u.id = w.owner_id",
            )
            .bind(self.workbook_id)
            .bind(self.size_bytes)
            .execute(&mut *db)
            .await?;
        }
        db.commit().await?;
        if at_branch(pool, self.workbook_id, self.branch_id, self.seq)
            .await?
            .as_ref()
            != Some(self)
        {
            return Err(StoreError::Invalid("snapshot metadata mismatch"));
        }
        Ok(inserted)
    }

    pub(super) async fn publish_in_tx(&self, db: &mut PgConnection) -> Result<bool, StoreError> {
        let inserted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO snapshots (workbook_id, branch_id, seq, format_version, storage_key, size_bytes, sha256)
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING RETURNING size_bytes",
        )
        .bind(self.workbook_id)
        .bind(self.branch_id)
        .bind(self.seq)
        .bind(self.format_version)
        .bind(&self.storage_key)
        .bind(self.size_bytes)
        .bind(&self.sha256)
        .fetch_optional(&mut *db)
        .await?;
        Ok(inserted.is_some())
    }
}

pub(super) async fn prune_branch(
    db: &mut PgConnection,
    doc: Uuid,
    branch_id: Uuid,
    owner_id: Uuid,
    keep_recent: usize,
) -> Result<(Vec<String>, i64), StoreError> {
    let rows: Vec<(i64, String, i64)> = sqlx::query_as(
        "SELECT seq, storage_key, size_bytes FROM snapshots
         WHERE workbook_id = $1 AND branch_id = $2 ORDER BY seq ASC",
    )
    .bind(doc)
    .bind(branch_id)
    .fetch_all(&mut *db)
    .await?;
    if rows.len() <= keep_recent {
        return Ok((Vec::new(), 0));
    }

    let version_seqs: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT seq FROM versions WHERE workbook_id = $1 AND branch_id = $2 ORDER BY seq ASC",
    )
    .bind(doc)
    .bind(branch_id)
    .fetch_all(&mut *db)
    .await?;

    let mut keep = BTreeSet::new();
    if rows.first().is_some_and(|r| r.0 == 0) {
        keep.insert(0);
    }
    for v_seq in version_seqs {
        if let Some(base) = rows.iter().rfind(|r| r.0 <= v_seq) {
            keep.insert(base.0);
        }
    }
    let mut seen_buckets = BTreeSet::new();
    for row in &rows {
        if row.0 > 0 && seen_buckets.insert(row.0 / 5_000) {
            keep.insert(row.0);
        }
    }
    for row in rows.iter().rev().take(keep_recent) {
        keep.insert(row.0);
    }

    let mut prune_seqs = Vec::new();
    let mut prune_keys = Vec::new();
    let mut reclaimed = 0_i64;
    for (seq, key, size_bytes) in rows {
        if !keep.contains(&seq) {
            prune_seqs.push(seq);
            prune_keys.push(key);
            reclaimed = reclaimed.saturating_add(size_bytes);
        }
    }
    if prune_seqs.is_empty() {
        return Ok((Vec::new(), 0));
    }

    sqlx::query(
        "DELETE FROM snapshots WHERE workbook_id = $1 AND branch_id = $2 AND seq = ANY($3)",
    )
    .bind(doc)
    .bind(branch_id)
    .bind(&prune_seqs)
    .execute(&mut *db)
    .await?;
    sqlx::query("UPDATE users SET storage_bytes = GREATEST(0, storage_bytes - $2) WHERE id = $1")
        .bind(owner_id)
        .bind(reclaimed)
        .execute(&mut *db)
        .await?;
    Ok((prune_keys, reclaimed))
}

/// Recursively sweeps stale `.tmp` files across `data_dir`, `snapshots/{doc}/`,
/// and `snapshots/{doc}/branches/{branch}/`.
pub(super) fn sweep_stale_tmp_files(data_dir: &Path) -> io::Result<usize> {
    let mut removed = 0_usize;
    let mut stack = vec![data_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(".tmp") || name.ends_with(".tmp"))
                && fs::remove_file(entry.path()).is_ok()
            {
                removed = removed.saturating_add(1);
            }
        }
    }
    Ok(removed)
}

pub(super) async fn at_branch(
    pool: &PgPool,
    doc: Uuid,
    branch_id: Uuid,
    seq: i64,
) -> Result<Option<SnapshotFile>, StoreError> {
    Ok(sqlx::query_as::<_, SnapshotFile>(
        "SELECT workbook_id, branch_id, seq, format_version, storage_key, size_bytes, sha256
         FROM snapshots
         WHERE workbook_id = $1 AND branch_id = $2 AND seq = $3",
    )
    .bind(doc)
    .bind(branch_id)
    .bind(seq)
    .fetch_optional(pool)
    .await?)
}

pub(super) async fn list_desc_branch(
    db: &mut PgConnection,
    doc: Uuid,
    branch_id: Uuid,
) -> Result<Vec<SnapshotFile>, StoreError> {
    Ok(sqlx::query_as::<_, SnapshotFile>(
        "SELECT workbook_id, branch_id, seq, format_version, storage_key, size_bytes, sha256
         FROM snapshots
         WHERE workbook_id = $1 AND branch_id = $2 ORDER BY seq DESC",
    )
    .bind(doc)
    .bind(branch_id)
    .fetch_all(db)
    .await?)
}

fn storage_key_branch(doc: Uuid, branch_id: Uuid, seq: i64) -> String {
    if branch_id == MAIN_BRANCH {
        format!("snapshots/{doc}/{seq}.rtclsnap")
    } else {
        format!("snapshots/{doc}/branches/{branch_id}/{seq}.rtclsnap")
    }
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
        for ancestor in path.ancestors().take(4) {
            File::open(ancestor)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
