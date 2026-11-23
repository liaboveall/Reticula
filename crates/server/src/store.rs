//! PostgreSQL transaction log, snapshots, quotas, and replica high-water marks.

mod agent;
mod collaboration;
mod history;
mod snapshot;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use reticula_core::id::unpack_replica_id;
use reticula_core::ops::Workbook;
use reticula_formula::binder::BoundFormula;
use reticula_sync::{
    CommittedPermute, ReplicaWatermark, ServerDocMachine, ServerLimits, StagedTxn,
};
use reticula_wire::snapshot::decode_snapshot_for_collab;
use reticula_wire::v3::{WireCommittedTxnV3, WireTxnV3, WireWalRecordV3};
use reticula_wire::wal::{
    MAX_WAL_FRAME_PAYLOAD, WAL_HEADER_LEN, encode_wal_complete_v3, scan_wal_any_v3,
};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use thiserror::Error;
use uuid::Uuid;

pub use agent::{
    AgentRunDetail, AgentRunRecord, BranchRecord, DailyQuotaUsage, InterruptedAgentRun,
};
pub use collaboration::{
    AccountSummary, AnonymousShareAccess, DeleteAccountOutcome, MemberEntry, SessionIdentity,
    ShareLinkRecord, User, WorkbookAccess, WorkbookListItem, WorkbookLists, WorkbookVisibility,
};
pub use history::{HistoryEntry, HistoryPage, VersionEntry};
pub use snapshot::SnapshotFile;

pub const MAIN_BRANCH: Uuid = Uuid::nil();
pub const DELETED_USER_ID: Uuid = Uuid::nil();
pub const DEFAULT_SNAPSHOT_KEEP_RECENT: usize = 5;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

#[derive(Clone, Copy, Debug)]
pub struct StorageLimits {
    pub max_doc_cells: usize,
    pub max_doc_bytes: usize,
    pub max_import_bytes: usize,
    pub user_quota_bytes: u64,
    pub disk_soft_bytes: u64,
    pub disk_hard_bytes: u64,
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self {
            max_doc_cells: 2_000_000,
            max_doc_bytes: 64 * 1024 * 1024,
            max_import_bytes: 32 * 1024 * 1024,
            user_quota_bytes: 268_435_456,
            disk_soft_bytes: 16 * 1024 * 1024 * 1024,
            disk_hard_bytes: 20 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("blocking snapshot task failed: {0}")]
    SnapshotTask(#[from] tokio::task::JoinError),
    #[error("workbook does not exist")]
    NotFound,
    #[error("access denied")]
    Forbidden,
    #[error("the actor's replica counter or log head no longer matches persistent state")]
    StaleActor,
    #[error("invalid persistent value: {0}")]
    Invalid(&'static str),
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    QuotaExceeded(String),
    #[error("{0}")]
    DiskFull(String),
}

/// A validated change and the last server revision its author had observed.
pub struct BatchEntry<'a> {
    pub staged: &'a StagedTxn,
    pub base_seq: u64,
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    data_dir: PathBuf,
    limits: StorageLimits,
    disk_used_bytes: Arc<AtomicU64>,
}

pub(crate) struct SnapshotCapture {
    doc: Uuid,
    branch_id: Uuid,
    seq: u64,
    workbook: Workbook<BoundFormula>,
    watermarks: Vec<ReplicaWatermark>,
    permutes: Vec<CommittedPermute>,
}

impl From<&ServerDocMachine> for SnapshotCapture {
    fn from(machine: &ServerDocMachine) -> Self {
        Self {
            doc: Uuid::from_bytes(machine.doc_id()),
            branch_id: Uuid::from_bytes(machine.branch_id()),
            seq: machine.head_seq(),
            workbook: machine.workbook().clone(),
            watermarks: machine.watermarks(),
            permutes: machine.committed_permutes().to_vec(),
        }
    }
}

impl Store {
    pub async fn connect(database_url: &str, data_dir: PathBuf) -> Result<Self, StoreError> {
        Self::connect_with_limits(database_url, data_dir, StorageLimits::default()).await
    }

    pub async fn connect_with_limits(
        database_url: &str,
        data_dir: PathBuf,
        limits: StorageLimits,
    ) -> Result<Self, StoreError> {
        let options = database_url
            .parse::<PgConnectOptions>()?
            .options([("synchronous_commit", "on")]);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        let store = Self {
            pool,
            data_dir,
            limits,
            disk_used_bytes: Arc::new(AtomicU64::new(0)),
        };
        store.ready().await?;
        let dir = store.data_dir.clone();
        let _swept =
            tokio::task::spawn_blocking(move || snapshot::sweep_stale_tmp_files(&dir)).await??;
        store.refresh_disk_used().await?;
        Ok(store)
    }

    pub fn limits(&self) -> StorageLimits {
        self.limits
    }

    pub fn disk_used_bytes(&self) -> u64 {
        self.disk_used_bytes.load(Ordering::Relaxed)
    }

    pub fn disk_over_soft_limit(&self) -> bool {
        self.disk_used_bytes() >= self.limits.disk_soft_bytes
    }

    pub fn disk_over_hard_limit(&self) -> bool {
        self.disk_used_bytes() >= self.limits.disk_hard_bytes
    }

    pub async fn refresh_disk_used(&self) -> Result<u64, StoreError> {
        let total = sqlx::query_scalar!(
            "SELECT COALESCE(SUM(storage_bytes), 0)::bigint AS \"total!\" FROM users"
        )
        .fetch_one(&self.pool)
        .await?;
        let bytes = u64::try_from(total.max(0)).unwrap_or(0);
        self.disk_used_bytes.store(bytes, Ordering::Relaxed);
        Ok(bytes)
    }

    pub async fn ready(&self) -> Result<(), StoreError> {
        sqlx::query!("SELECT 1 AS \"ready!\"")
            .fetch_one(&self.pool)
            .await?;
        let dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            tempfile::NamedTempFile::new_in(dir)?.close()
        })
        .await??;
        Ok(())
    }

    /// Creates the main branch, owner membership and owner's first replica atomically.
    pub async fn create_workbook(&self, owner: Uuid, raw_title: &str) -> Result<Uuid, StoreError> {
        if owner == DELETED_USER_ID {
            return Err(StoreError::Forbidden);
        }
        let title = history::validate_label_text(raw_title, "workbook title")?;
        if self.disk_over_hard_limit() {
            return Err(StoreError::DiskFull(
                "server storage capacity reached".to_owned(),
            ));
        }

        let id = Uuid::new_v4();
        let mut db = self.pool.begin().await?;
        let user_bytes = sqlx::query_scalar!(
            "SELECT storage_bytes FROM users WHERE id = $1 FOR UPDATE",
            owner,
        )
        .fetch_optional(&mut *db)
        .await?
        .ok_or(StoreError::NotFound)?;
        let used = u64::try_from(user_bytes.max(0)).unwrap_or(0);
        if used >= self.limits.user_quota_bytes {
            return Err(StoreError::QuotaExceeded(
                "user storage quota exceeded".to_owned(),
            ));
        }
        let total = sqlx::query_scalar!(
            "SELECT COALESCE(SUM(storage_bytes), 0)::bigint AS \"total!\" FROM users"
        )
        .fetch_one(&mut *db)
        .await?;
        if u64::try_from(total.max(0)).unwrap_or(0) >= self.limits.disk_hard_bytes {
            return Err(StoreError::DiskFull(
                "server storage capacity reached".to_owned(),
            ));
        }

        sqlx::query!(
            "INSERT INTO workbooks (id, owner_id, title) VALUES ($1, $2, $3)",
            id,
            owner,
            title,
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "INSERT INTO branches (id, workbook_id, created_by) VALUES ($1, $2, $3)",
            MAIN_BRANCH,
            id,
            owner,
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "INSERT INTO members (workbook_id, user_id, role) VALUES ($1, $2, 'owner')",
            id,
            owner,
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "INSERT INTO replicas (workbook_id, replica, user_id) VALUES ($1, 1, $2)",
            id,
            owner,
        )
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        Ok(id)
    }

    /// Validates, normalizes, and imports an `RTCLSNAP` snapshot as a new collaborative workbook.
    pub async fn import_workbook(
        &self,
        owner: Uuid,
        raw_title: &str,
        bytes: Vec<u8>,
    ) -> Result<Uuid, StoreError> {
        if owner == DELETED_USER_ID {
            return Err(StoreError::Forbidden);
        }
        let title = history::validate_label_text(raw_title, "workbook title")?;
        if bytes.is_empty() || bytes.len() > self.limits.max_import_bytes {
            return Err(StoreError::InvalidInput(
                "snapshot payload exceeds import byte limit".to_owned(),
            ));
        }
        if self.disk_over_hard_limit() {
            return Err(StoreError::DiskFull(
                "server storage capacity reached".to_owned(),
            ));
        }

        let max_cells = self.limits.max_doc_cells;
        let max_doc_bytes = self.limits.max_doc_bytes;
        let id = Uuid::new_v4();
        let dir = self.data_dir.clone();
        let (file, est_bytes) = tokio::task::spawn_blocking(move || {
            let decoded =
                decode_snapshot_for_collab(&bytes, max_cells, max_doc_bytes).map_err(|_| {
                    StoreError::InvalidInput("invalid or oversized snapshot".to_owned())
                })?;
            let workbook = decoded.engine.workbook();
            let est_bytes = workbook.estimated_bytes();
            let file = SnapshotFile::write(&dir, id, 0, workbook, Vec::new(), &[])?;
            Ok::<_, StoreError>((file, est_bytes))
        })
        .await??;

        let snap_size = file.size();
        let snap_size_i64 = integer::<_, i64>(snap_size, "snapshot size")?;
        let est_i64 = integer::<_, i64>(est_bytes, "estimated size")?;

        let result: Result<(), StoreError> = async {
            let mut db = self.pool.begin().await?;
            let user_bytes = sqlx::query_scalar!(
                "SELECT storage_bytes FROM users WHERE id = $1 FOR UPDATE",
                owner,
            )
            .fetch_optional(&mut *db)
            .await?
            .ok_or(StoreError::NotFound)?;
            let used = u64::try_from(user_bytes.max(0)).unwrap_or(0);
            if used.saturating_add(snap_size) > self.limits.user_quota_bytes {
                return Err(StoreError::QuotaExceeded(
                    "user storage quota exceeded".to_owned(),
                ));
            }
            let total = sqlx::query_scalar!(
                "SELECT COALESCE(SUM(storage_bytes), 0)::bigint AS \"total!\" FROM users"
            )
            .fetch_one(&mut *db)
            .await?;
            if u64::try_from(total.max(0))
                .unwrap_or(0)
                .saturating_add(snap_size)
                > self.limits.disk_hard_bytes
            {
                return Err(StoreError::DiskFull(
                    "server storage capacity reached".to_owned(),
                ));
            }

            sqlx::query!(
                "INSERT INTO workbooks (id, owner_id, title, est_bytes) VALUES ($1, $2, $3, $4)",
                id,
                owner,
                title,
                est_i64,
            )
            .execute(&mut *db)
            .await?;
            sqlx::query!(
                "INSERT INTO branches (id, workbook_id, created_by) VALUES ($1, $2, $3)",
                MAIN_BRANCH,
                id,
                owner,
            )
            .execute(&mut *db)
            .await?;
            sqlx::query!(
                "INSERT INTO members (workbook_id, user_id, role) VALUES ($1, $2, 'owner')",
                id,
                owner,
            )
            .execute(&mut *db)
            .await?;
            sqlx::query!(
                "INSERT INTO replicas (workbook_id, replica, user_id) VALUES ($1, 1, $2)",
                id,
                owner,
            )
            .execute(&mut *db)
            .await?;
            file.publish_in_tx(&mut db).await?;
            sqlx::query!(
                "UPDATE users SET storage_bytes = storage_bytes + $2 WHERE id = $1",
                owner,
                snap_size_i64,
            )
            .execute(&mut *db)
            .await?;
            db.commit().await?;
            Ok(())
        }
        .await;

        if let Err(err) = result {
            let dir = self.data_dir.join("snapshots").join(id.to_string());
            let _ = tokio::fs::remove_dir_all(dir).await;
            return Err(err);
        }
        self.refresh_disk_used().await?;
        Ok(id)
    }

    pub async fn update_workbook(
        &self,
        doc: Uuid,
        title: Option<String>,
        visibility: Option<WorkbookVisibility>,
    ) -> Result<(String, WorkbookVisibility), StoreError> {
        let valid_title = title
            .as_deref()
            .map(|t| history::validate_label_text(t, "workbook title"))
            .transpose()?;
        let mut db = self.pool.begin().await?;
        let existing = sqlx::query!(
            "SELECT title, visibility FROM workbooks WHERE id = $1 FOR UPDATE",
            doc,
        )
        .fetch_optional(&mut *db)
        .await?
        .ok_or(StoreError::NotFound)?;

        if visibility == Some(WorkbookVisibility::LinkView) {
            let has_viewer_link = sqlx::query_scalar!(
                "SELECT EXISTS(
                    SELECT 1 FROM share_links
                    WHERE workbook_id = $1 AND role = 'viewer' AND revoked_at IS NULL
                      AND (expires_at IS NULL OR expires_at > now())
                      AND (NOT single_use OR consumed_by IS NULL)
                ) AS \"exists!\"",
                doc,
            )
            .fetch_one(&mut *db)
            .await?;
            if !has_viewer_link {
                return Err(StoreError::InvalidInput(
                    "create an active viewer share link before enabling link view".to_owned(),
                ));
            }
        }

        let next_title = valid_title.unwrap_or(existing.title);
        let next_vis = visibility
            .map(|v| v.as_str().to_owned())
            .unwrap_or(existing.visibility);
        sqlx::query!(
            "UPDATE workbooks SET title = $2, visibility = $3, updated_at = now() WHERE id = $1",
            doc,
            next_title,
            next_vis,
        )
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        Ok((next_title, WorkbookVisibility::parse(&next_vis)?))
    }

    pub async fn delete_workbook(&self, doc: Uuid) -> Result<(), StoreError> {
        let mut db = self.pool.begin().await?;
        let wb = sqlx::query!(
            "SELECT owner_id, log_bytes FROM workbooks WHERE id = $1 FOR UPDATE",
            doc,
        )
        .fetch_optional(&mut *db)
        .await?
        .ok_or(StoreError::NotFound)?;
        let snap_bytes = sqlx::query_scalar!(
            "SELECT COALESCE(SUM(size_bytes), 0)::bigint AS \"total!\"
             FROM snapshots WHERE workbook_id = $1",
            doc,
        )
        .fetch_one(&mut *db)
        .await?;
        let reclaimed = snap_bytes.saturating_add(wb.log_bytes);
        sqlx::query!("DELETE FROM workbooks WHERE id = $1", doc)
            .execute(&mut *db)
            .await?;
        sqlx::query!(
            "UPDATE users SET storage_bytes = GREATEST(0, storage_bytes - $2) WHERE id = $1",
            wb.owner_id,
            reclaimed,
        )
        .execute(&mut *db)
        .await?;
        db.commit().await?;

        let dir = self.data_dir.join("snapshots").join(doc.to_string());
        let _ = tokio::fs::remove_dir_all(dir).await;
        self.refresh_disk_used().await?;
        Ok(())
    }

    /// Persists a new replica before the actor exposes it in `Welcome`.
    pub async fn register_replica(
        &self,
        doc: Uuid,
        user: Uuid,
        replica: u32,
    ) -> Result<(), StoreError> {
        let number = integer::<_, i32>(replica, "replica")?;
        let (epoch, counter) = unpack_replica_id(replica);
        let mut db = self.pool.begin().await?;
        let changed = sqlx::query!(
            "UPDATE workbooks SET next_per_epoch_replica = next_per_epoch_replica + 1
             WHERE id = $1 AND log_epoch = $2 AND next_per_epoch_replica = $3",
            doc,
            i16::from(epoch),
            i32::from(counter),
        )
        .execute(&mut *db)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(StoreError::StaleActor);
        }
        sqlx::query!(
            "INSERT INTO replicas (workbook_id, replica, user_id) VALUES ($1, $2, $3)",
            doc,
            number,
            user
        )
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        Ok(())
    }

    /// Makes the whole staged batch durable on `MAIN_BRANCH`, including all replica watermarks.
    pub async fn commit_batch(
        &self,
        doc: Uuid,
        epoch: u8,
        entries: &[BatchEntry<'_>],
        est_bytes: usize,
    ) -> Result<(), StoreError> {
        self.commit_batch_branch(doc, MAIN_BRANCH, epoch, entries, est_bytes)
            .await
    }

    /// Makes the whole staged batch durable on `(doc, branch_id)`.
    ///
    /// Preserves `workbooks.est_bytes` isolation: non-main branches update
    /// `branches.est_bytes` while continuing to account log bytes against
    /// `workbooks.log_bytes` and `users.storage_bytes`.
    pub async fn commit_batch_branch(
        &self,
        doc: Uuid,
        branch_id: Uuid,
        epoch: u8,
        entries: &[BatchEntry<'_>],
        est_bytes: usize,
    ) -> Result<(), StoreError> {
        let Some(first) = entries.first() else {
            return Ok(());
        };
        let previous = first
            .staged
            .committed
            .seq
            .checked_sub(1)
            .ok_or(StoreError::Invalid("transaction seq"))?;
        let mut seqs = Vec::with_capacity(entries.len());
        let mut replicas = Vec::with_capacity(entries.len());
        let mut client_seqs = Vec::with_capacity(entries.len());
        let mut bases = Vec::with_capacity(entries.len());
        let mut counters = Vec::with_capacity(entries.len());
        let mut users = Vec::with_capacity(entries.len());
        let mut timestamps = Vec::with_capacity(entries.len());
        let mut payloads = Vec::with_capacity(entries.len());
        let mut log_bytes = 0_i64;
        let mut expected = previous;
        for entry in entries {
            let commit = &entry.staged.committed;
            expected += 1;
            if commit.seq != expected || unpack_replica_id(commit.replica).0 != epoch {
                return Err(StoreError::Invalid("batch ordering or epoch"));
            }
            let payload = encode_log(commit)?;
            log_bytes += integer::<_, i64>(payload.len(), "payload length")?;
            seqs.push(integer::<_, i64>(commit.seq, "transaction seq")?);
            replicas.push(integer::<_, i32>(commit.replica, "replica")?);
            client_seqs.push(integer::<_, i64>(commit.client_seq, "client seq")?);
            bases.push(integer::<_, i64>(entry.base_seq, "base seq")?);
            counters.push(i64::from(entry.staged.max_id_counter));
            users.push(Uuid::from_bytes(commit.user_id));
            timestamps.push(integer::<_, i64>(commit.timestamp_ms, "timestamp")?);
            payloads.push(payload);
        }
        let previous = integer::<_, i64>(previous, "previous seq")?;
        let est_bytes = integer::<_, i64>(est_bytes, "estimated size")?;
        let mut db = self.pool.begin().await?;
        let owner_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT owner_id FROM workbooks WHERE id = $1 AND log_epoch = $2 FOR UPDATE",
        )
        .bind(doc)
        .bind(i16::from(epoch))
        .fetch_optional(&mut *db)
        .await?;
        let head: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) FROM txns WHERE workbook_id = $1 AND branch_id = $2",
        )
        .bind(doc)
        .bind(branch_id)
        .fetch_one(&mut *db)
        .await?;
        let Some(owner_id) = owner_id else {
            return Err(StoreError::StaleActor);
        };
        if head != previous {
            return Err(StoreError::StaleActor);
        }
        sqlx::query(
            "INSERT INTO txns (workbook_id, branch_id, seq, replica, client_seq, base_seq, user_id, created_at, payload)
             SELECT $1, $2, b.seq, b.replica, b.client_seq, b.base_seq, b.user_id,
                    to_timestamp(b.timestamp_ms::float8 / 1000), b.payload
             FROM UNNEST($3::bigint[], $4::integer[], $5::bigint[], $6::bigint[],
                         $7::uuid[], $8::bigint[], $9::bytea[])
                  AS b(seq, replica, client_seq, base_seq, user_id, timestamp_ms, payload)",
        )
        .bind(doc)
        .bind(branch_id)
        .bind(&seqs)
        .bind(&replicas)
        .bind(&client_seqs)
        .bind(&bases)
        .bind(&users)
        .bind(&timestamps)
        .bind(&payloads)
        .execute(&mut *db)
        .await?;
        let updated = sqlx::query(
            "UPDATE replicas AS r
             SET last_client_seq = b.last_client_seq, max_id_counter = b.max_id_counter
             FROM (
                 SELECT replica, user_id, MIN(client_seq) AS first_client_seq,
                        MAX(client_seq) AS last_client_seq, MAX(max_id_counter) AS max_id_counter
                 FROM UNNEST($2::integer[], $3::uuid[], $4::bigint[], $5::bigint[])
                      AS e(replica, user_id, client_seq, max_id_counter)
                 GROUP BY replica, user_id
                 HAVING MAX(client_seq) - MIN(client_seq) + 1 = COUNT(*)
             ) AS b
             WHERE r.workbook_id = $1 AND r.replica = b.replica AND r.user_id = b.user_id
                   AND r.last_client_seq = b.first_client_seq - 1
                   AND r.max_id_counter <= b.max_id_counter",
        )
        .bind(doc)
        .bind(&replicas)
        .bind(&users)
        .bind(&client_seqs)
        .bind(&counters)
        .execute(&mut *db)
        .await?;
        let distinct = replicas
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        if updated.rows_affected() != distinct as u64 {
            return Err(StoreError::StaleActor);
        }
        if branch_id == MAIN_BRANCH {
            sqlx::query(
                "UPDATE workbooks SET log_bytes = log_bytes + $2, est_bytes = $3, updated_at = now() WHERE id = $1",
            )
            .bind(doc)
            .bind(log_bytes)
            .bind(est_bytes)
            .execute(&mut *db)
            .await?;
        } else {
            sqlx::query(
                "UPDATE workbooks SET log_bytes = log_bytes + $2, updated_at = now() WHERE id = $1",
            )
            .bind(doc)
            .bind(log_bytes)
            .execute(&mut *db)
            .await?;
            sqlx::query(
                "UPDATE branches SET est_bytes = $3, updated_at = now() WHERE workbook_id = $1 AND id = $2",
            )
            .bind(doc)
            .bind(branch_id)
            .bind(est_bytes)
            .execute(&mut *db)
            .await?;
        }
        sqlx::query("UPDATE users SET storage_bytes = storage_bytes + $2 WHERE id = $1")
            .bind(owner_id)
            .bind(log_bytes)
            .execute(&mut *db)
            .await?;
        db.commit().await?;
        self.disk_used_bytes.fetch_add(
            u64::try_from(log_bytes.max(0)).unwrap_or(0),
            Ordering::Relaxed,
        );
        Ok(())
    }

    /// Loads `MAIN_BRANCH` at one consistent database revision, then replays its durable tail.
    pub async fn load_machine(&self, doc: Uuid) -> Result<ServerDocMachine, StoreError> {
        self.load_machine_branch(doc, MAIN_BRANCH).await
    }

    /// Loads `(doc, branch_id)` at one consistent database revision, injecting the
    /// appropriate branch snapshot URL prefix (`MAIN_BRANCH` vs `/branches/{bid}`).
    pub async fn load_machine_branch(
        &self,
        doc: Uuid,
        branch_id: Uuid,
    ) -> Result<ServerDocMachine, StoreError> {
        let mut db = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *db)
            .await?;
        let book: (i16, i32) =
            sqlx::query_as("SELECT log_epoch, next_per_epoch_replica FROM workbooks WHERE id = $1")
                .bind(doc)
                .fetch_optional(&mut *db)
                .await?
                .ok_or(StoreError::NotFound)?;

        if branch_id != MAIN_BRANCH {
            let exists: Option<Uuid> =
                sqlx::query_scalar("SELECT id FROM branches WHERE workbook_id = $1 AND id = $2")
                    .bind(doc)
                    .bind(branch_id)
                    .fetch_optional(&mut *db)
                    .await?;
            if exists.is_none() {
                return Err(StoreError::NotFound);
            }
        }

        let snapshots = snapshot::list_desc_branch(&mut db, doc, branch_id).await?;
        let mut snapshot = None;
        for candidate in snapshots {
            if tokio::fs::try_exists(self.data_dir.join(candidate.storage_key())).await? {
                snapshot = Some(candidate);
                break;
            }
            if candidate.seq == 0 {
                return Err(StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "base snapshot seq=0 missing on disk",
                )));
            }
            tracing::warn!(
                workbook_id = %doc,
                branch_id = %branch_id,
                seq = candidate.seq,
                storage_key = candidate.storage_key(),
                "snapshot file missing on disk; falling back to earlier snapshot and replaying txns"
            );
        }
        let snapshot_seq = snapshot.as_ref().map_or(0, |file| file.seq);
        let tail: Vec<(i64, i32, i64, Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT seq, replica, client_seq, user_id, payload FROM txns
             WHERE workbook_id = $1 AND branch_id = $2 AND seq > $3 ORDER BY seq",
        )
        .bind(doc)
        .bind(branch_id)
        .bind(snapshot_seq)
        .fetch_all(&mut *db)
        .await?;
        let replicas: Vec<(i32, Uuid, i64, i64)> = sqlx::query_as(
            "SELECT replica, user_id, last_client_seq, max_id_counter FROM replicas WHERE workbook_id = $1",
        )
        .bind(doc)
        .fetch_all(&mut *db)
        .await?;
        db.commit().await?;

        let watermarks = replicas
            .into_iter()
            .map(|(replica, user_id, last_client_seq, max_id_counter)| {
                Ok(ReplicaWatermark {
                    replica: integer(replica, "replica")?,
                    user_id: user_id.into_bytes(),
                    last_client_seq: integer(last_client_seq, "client seq")?,
                    max_id_counter: integer(max_id_counter, "max id counter")?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let epoch = integer(book.0, "epoch")?;
        let next = integer(book.1, "next replica")?;
        let dir = self.data_dir.clone();
        let server_limits = ServerLimits {
            max_populated_cells: self.limits.max_doc_cells as u64,
            max_doc_bytes: self.limits.max_doc_bytes,
            ..ServerLimits::default()
        };
        let snapshot_prefix = if branch_id == MAIN_BRANCH {
            format!("/api/workbooks/{doc}")
        } else {
            format!("/api/workbooks/{doc}/branches/{branch_id}")
        };
        tokio::task::spawn_blocking(move || {
            let (workbook, snapshot_permutes) = match snapshot {
                Some(file) => file.read_snapshot(&dir)?,
                None => (Workbook::new(), Vec::new()),
            };
            let commits = tail
                .into_iter()
                .map(|(seq_i64, rep_i32, cseq_i64, uid, payload)| {
                    let seq = integer(seq_i64, "transaction seq")?;
                    let record = decode_log(&payload, seq)?;
                    Ok(WireCommittedTxnV3 {
                        seq,
                        replica: integer(rep_i32, "replica")?,
                        client_seq: integer(cseq_i64, "client seq")?,
                        user_id: uid.into_bytes(),
                        timestamp_ms: record.timestamp_ms,
                        txn: WireTxnV3 {
                            label: record.label,
                            ops: record.ops,
                        },
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            ServerDocMachine::from_snapshot_and_log(
                doc.into_bytes(),
                branch_id.into_bytes(),
                snapshot_prefix,
                epoch,
                integer(snapshot_seq, "snapshot seq")?,
                workbook,
                next,
                watermarks,
                snapshot_permutes,
                commits,
                server_limits,
            )
            .map_err(|_| StoreError::Invalid("transaction log replay"))
        })
        .await?
    }

    /// Captures the current workbook through CoW blocks and persists it off the async runtime.
    pub async fn write_snapshot(
        &self,
        machine: &ServerDocMachine,
    ) -> Result<SnapshotFile, StoreError> {
        self.persist_snapshot(machine.into()).await
    }

    pub(crate) async fn persist_snapshot(
        &self,
        capture: SnapshotCapture,
    ) -> Result<SnapshotFile, StoreError> {
        if let Some(file) = self
            .snapshot_branch(capture.doc, capture.branch_id, capture.seq)
            .await?
        {
            let dir = self.data_dir.clone();
            return tokio::task::spawn_blocking(move || {
                file.open(&dir)?;
                Ok(file)
            })
            .await?;
        }
        let dir = self.data_dir.clone();
        let seq = integer(capture.seq, "snapshot seq")?;
        let file = tokio::task::spawn_blocking(move || {
            SnapshotFile::write_branch(
                &dir,
                capture.doc,
                capture.branch_id,
                seq,
                &capture.workbook,
                capture.watermarks,
                &capture.permutes,
            )
        })
        .await??;
        if file.publish(&self.pool).await? {
            self.disk_used_bytes
                .fetch_add(file.size(), Ordering::Relaxed);
        }
        Ok(file)
    }

    pub async fn prune_snapshots(&self, doc: Uuid, keep_recent: usize) -> Result<u64, StoreError> {
        self.prune_snapshots_branch(doc, MAIN_BRANCH, keep_recent)
            .await
    }

    pub async fn prune_snapshots_branch(
        &self,
        doc: Uuid,
        branch_id: Uuid,
        keep_recent: usize,
    ) -> Result<u64, StoreError> {
        let mut db = self.pool.begin().await?;
        let Some(owner_id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT owner_id FROM workbooks WHERE id = $1 FOR UPDATE",
        )
        .bind(doc)
        .fetch_optional(&mut *db)
        .await?
        else {
            return Ok(0);
        };
        let (pruned_keys, reclaimed) =
            snapshot::prune_branch(&mut db, doc, branch_id, owner_id, keep_recent).await?;
        db.commit().await?;
        if pruned_keys.is_empty() {
            return Ok(0);
        }
        let count = pruned_keys.len() as u64;
        for key in pruned_keys {
            let _ = tokio::fs::remove_file(self.data_dir.join(key)).await;
        }
        let _ = reclaimed;
        self.refresh_disk_used().await?;
        Ok(count)
    }

    pub async fn workbook_est_bytes(&self, doc: Uuid) -> Result<u64, StoreError> {
        self.branch_est_bytes(doc, MAIN_BRANCH).await
    }

    pub async fn branch_est_bytes(&self, doc: Uuid, branch_id: Uuid) -> Result<u64, StoreError> {
        let est: i64 = if branch_id == MAIN_BRANCH {
            sqlx::query_scalar("SELECT est_bytes FROM workbooks WHERE id = $1")
                .bind(doc)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(StoreError::NotFound)?
        } else {
            sqlx::query_scalar("SELECT est_bytes FROM branches WHERE workbook_id = $1 AND id = $2")
                .bind(doc)
                .bind(branch_id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(StoreError::NotFound)?
        };
        Ok(u64::try_from(est.max(0)).unwrap_or(0))
    }

    pub(crate) async fn receipt_branch(
        &self,
        doc: Uuid,
        branch_id: Uuid,
        replica: u32,
        client_seq: u64,
    ) -> Result<(u64, u64), StoreError> {
        let replica = integer::<_, i32>(replica, "replica")?;
        let client_seq = integer::<_, i64>(client_seq, "client seq")?;
        let row: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT seq, payload FROM txns WHERE workbook_id = $1 AND branch_id = $2 AND replica = $3 AND client_seq = $4",
        )
        .bind(doc)
        .bind(branch_id)
        .bind(replica)
        .bind(client_seq)
        .fetch_optional(&self.pool)
        .await?;
        let (seq_i64, payload) = row.ok_or(StoreError::Invalid("missing durable receipt"))?;
        let seq = integer(seq_i64, "receipt seq")?;
        Ok((seq, decode_log(&payload, seq)?.timestamp_ms))
    }

    pub async fn snapshot(&self, doc: Uuid, seq: u64) -> Result<Option<SnapshotFile>, StoreError> {
        self.snapshot_branch(doc, MAIN_BRANCH, seq).await
    }

    pub async fn snapshot_branch(
        &self,
        doc: Uuid,
        branch_id: Uuid,
        seq: u64,
    ) -> Result<Option<SnapshotFile>, StoreError> {
        snapshot::at_branch(&self.pool, doc, branch_id, integer(seq, "snapshot seq")?).await
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

fn encode_log(commit: &WireCommittedTxnV3) -> Result<Vec<u8>, StoreError> {
    let record = WireWalRecordV3 {
        seq: commit.seq,
        timestamp_ms: commit.timestamp_ms,
        label: commit.txn.label.clone(),
        ops: commit.txn.ops.clone(),
    };
    let bytes = encode_wal_complete_v3(commit.seq - 1, &[record])
        .map_err(|_| StoreError::Invalid("transaction encoding"))?;
    Ok(lz4_flex::compress_prepend_size(&bytes))
}

fn decode_log(payload: &[u8], seq: u64) -> Result<WireWalRecordV3, StoreError> {
    let invalid = || StoreError::Invalid("transaction encoding");
    let (size, _) = lz4_flex::block::uncompressed_size(payload).map_err(|_| invalid())?;
    let base_seq = seq.checked_sub(1).ok_or_else(invalid)?;
    if size > MAX_WAL_FRAME_PAYLOAD + WAL_HEADER_LEN + 32 {
        return Err(invalid());
    }
    let bytes = lz4_flex::decompress_size_prepended(payload).map_err(|_| invalid())?;
    let mut wal = scan_wal_any_v3(&bytes, base_seq, seq).map_err(|_| invalid())?;
    if wal.records.len() != 1 || wal.valid_bytes != bytes.len() {
        return Err(invalid());
    }
    wal.records.pop().ok_or_else(invalid)
}

fn integer<T, U: TryFrom<T>>(value: T, name: &'static str) -> Result<U, StoreError> {
    U::try_from(value).map_err(|_| StoreError::Invalid(name))
}
