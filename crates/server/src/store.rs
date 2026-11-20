//! PostgreSQL transaction log and replica high-water marks.

mod collaboration;
mod snapshot;

use std::path::{Path, PathBuf};

use reticula_core::id::unpack_replica_id;
use reticula_core::ops::Workbook;
use reticula_formula::binder::BoundFormula;
use reticula_sync::{ReplicaWatermark, ServerDocMachine, ServerLimits, StagedTxn};
use reticula_wire::v2::{WireCommittedTxnV2, WireTxnV2, WireWalRecordV2};
use reticula_wire::wal::{
    MAX_WAL_FRAME_PAYLOAD, WAL_HEADER_LEN, encode_wal_complete_v2, scan_wal_any,
};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use thiserror::Error;
use uuid::Uuid;

pub use collaboration::{SessionIdentity, User, WorkbookAccess};
pub use snapshot::SnapshotFile;

pub const MAIN_BRANCH: Uuid = Uuid::nil();

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

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
    #[error("the actor's replica counter or log head no longer matches persistent state")]
    StaleActor,
    #[error("invalid persistent value: {0}")]
    Invalid(&'static str),
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
}

pub(crate) struct SnapshotCapture {
    doc: Uuid,
    seq: u64,
    workbook: Workbook<BoundFormula>,
    watermarks: Vec<ReplicaWatermark>,
}

impl From<&ServerDocMachine> for SnapshotCapture {
    fn from(machine: &ServerDocMachine) -> Self {
        Self {
            doc: Uuid::from_bytes(machine.doc_id()),
            seq: machine.head_seq(),
            workbook: machine.workbook().clone(),
            watermarks: machine.watermarks(),
        }
    }
}

impl Store {
    pub async fn connect(database_url: &str, data_dir: PathBuf) -> Result<Self, StoreError> {
        let options = database_url
            .parse::<PgConnectOptions>()?
            .options([("synchronous_commit", "on")]);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        let store = Self { pool, data_dir };
        store.ready().await?;
        Ok(store)
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
    pub async fn create_workbook(&self, owner: Uuid, title: &str) -> Result<Uuid, StoreError> {
        let id = Uuid::new_v4();
        let mut db = self.pool.begin().await?;
        sqlx::query!(
            "INSERT INTO workbooks (id, owner_id, title) VALUES ($1, $2, $3)",
            id,
            owner,
            title
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "INSERT INTO branches (id, workbook_id, created_by) VALUES ($1, $2, $3)",
            MAIN_BRANCH,
            id,
            owner
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "INSERT INTO members (workbook_id, user_id, role) VALUES ($1, $2, 'owner')",
            id,
            owner
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "INSERT INTO replicas (workbook_id, replica, user_id) VALUES ($1, 1, $2)",
            id,
            owner
        )
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        Ok(id)
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

    /// Makes the whole staged batch durable, including all replica watermarks.
    pub async fn commit_batch(
        &self,
        doc: Uuid,
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
        let locked = sqlx::query!(
            "SELECT id FROM workbooks WHERE id = $1 AND log_epoch = $2 FOR UPDATE",
            doc,
            i16::from(epoch)
        )
        .fetch_optional(&mut *db)
        .await?;
        let head = sqlx::query_scalar!(
            "SELECT COALESCE(MAX(seq), 0) AS \"head!\" FROM txns WHERE workbook_id = $1 AND branch_id = $2",
            doc,
            MAIN_BRANCH,
        )
        .fetch_one(&mut *db)
        .await?;
        if locked.is_none() || head != previous {
            return Err(StoreError::StaleActor);
        }
        sqlx::query!(
            "INSERT INTO txns (workbook_id, branch_id, seq, replica, client_seq, base_seq, user_id, created_at, payload)
             SELECT $1, $2, b.seq, b.replica, b.client_seq, b.base_seq, b.user_id,
                    to_timestamp(b.timestamp_ms::float8 / 1000), b.payload
             FROM UNNEST($3::bigint[], $4::integer[], $5::bigint[], $6::bigint[],
                         $7::uuid[], $8::bigint[], $9::bytea[])
                  AS b(seq, replica, client_seq, base_seq, user_id, timestamp_ms, payload)",
            doc, MAIN_BRANCH, &seqs, &replicas, &client_seqs, &bases, &users, &timestamps, &payloads,
        )
        .execute(&mut *db)
        .await?;
        let updated = sqlx::query!(
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
            doc,
            &replicas,
            &users,
            &client_seqs,
            &counters,
        )
        .execute(&mut *db)
        .await?;
        let distinct = replicas
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        if updated.rows_affected() != distinct as u64 {
            return Err(StoreError::StaleActor);
        }
        sqlx::query!("UPDATE workbooks SET log_bytes = log_bytes + $2, est_bytes = $3, updated_at = now() WHERE id = $1", doc, log_bytes, est_bytes)
            .execute(&mut *db)
            .await?;
        db.commit().await?;
        Ok(())
    }

    /// Loads one consistent database revision, then replays its durable tail.
    pub async fn load_machine(&self, doc: Uuid) -> Result<ServerDocMachine, StoreError> {
        let mut db = self.pool.begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *db)
            .await?;
        let book = sqlx::query!(
            "SELECT log_epoch, next_per_epoch_replica FROM workbooks WHERE id = $1",
            doc
        )
        .fetch_optional(&mut *db)
        .await?
        .ok_or(StoreError::NotFound)?;
        let snapshot = snapshot::latest(&mut db, doc).await?;
        let snapshot_seq = snapshot.as_ref().map_or(0, |file| file.seq);
        let tail = sqlx::query!(
            "SELECT seq, replica, client_seq, user_id, payload FROM txns
             WHERE workbook_id = $1 AND branch_id = $2 AND seq > $3 ORDER BY seq",
            doc,
            MAIN_BRANCH,
            snapshot_seq,
        )
        .fetch_all(&mut *db)
        .await?;
        let replicas = sqlx::query!("SELECT replica, user_id, last_client_seq, max_id_counter FROM replicas WHERE workbook_id = $1", doc)
            .fetch_all(&mut *db)
            .await?;
        db.commit().await?;

        let watermarks = replicas
            .into_iter()
            .map(|row| {
                Ok(ReplicaWatermark {
                    replica: integer(row.replica, "replica")?,
                    user_id: row.user_id.into_bytes(),
                    last_client_seq: integer(row.last_client_seq, "client seq")?,
                    max_id_counter: integer(row.max_id_counter, "max id counter")?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let epoch = integer(book.log_epoch, "epoch")?;
        let next = integer(book.next_per_epoch_replica, "next replica")?;
        let dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || {
            let workbook = match snapshot {
                Some(file) => file.read_workbook(&dir)?,
                None => Workbook::new(),
            };
            let commits = tail
                .into_iter()
                .map(|row| {
                    let seq = integer(row.seq, "transaction seq")?;
                    let record = decode_log(&row.payload, seq)?;
                    Ok(WireCommittedTxnV2 {
                        seq,
                        replica: integer(row.replica, "replica")?,
                        client_seq: integer(row.client_seq, "client seq")?,
                        user_id: row.user_id.into_bytes(),
                        timestamp_ms: record.timestamp_ms,
                        txn: WireTxnV2 {
                            label: record.label,
                            ops: record.ops,
                        },
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            ServerDocMachine::from_snapshot_and_log(
                doc.into_bytes(),
                MAIN_BRANCH.into_bytes(),
                format!("/api/workbooks/{doc}"),
                epoch,
                integer(snapshot_seq, "snapshot seq")?,
                workbook,
                next,
                watermarks,
                commits,
                ServerLimits::default(),
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
        if let Some(file) = self.snapshot(capture.doc, capture.seq).await? {
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
            SnapshotFile::write(
                &dir,
                capture.doc,
                seq,
                &capture.workbook,
                capture.watermarks,
            )
        })
        .await??;
        file.publish(&self.pool).await?;
        Ok(file)
    }

    pub(crate) async fn receipt(
        &self,
        doc: Uuid,
        replica: u32,
        client_seq: u64,
    ) -> Result<(u64, u64), StoreError> {
        let replica = integer::<_, i32>(replica, "replica")?;
        let client_seq = integer::<_, i64>(client_seq, "client seq")?;
        let row = sqlx::query!(
            "SELECT seq, payload FROM txns WHERE workbook_id = $1 AND branch_id = $2 AND replica = $3 AND client_seq = $4",
            doc, MAIN_BRANCH, replica, client_seq,
        ).fetch_optional(&self.pool).await?.ok_or(StoreError::Invalid("missing durable receipt"))?;
        let seq = integer(row.seq, "receipt seq")?;
        Ok((seq, decode_log(&row.payload, seq)?.timestamp_ms))
    }

    pub async fn snapshot(&self, doc: Uuid, seq: u64) -> Result<Option<SnapshotFile>, StoreError> {
        snapshot::at(&self.pool, doc, integer(seq, "snapshot seq")?).await
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

fn encode_log(commit: &WireCommittedTxnV2) -> Result<Vec<u8>, StoreError> {
    let record = WireWalRecordV2 {
        seq: commit.seq,
        timestamp_ms: commit.timestamp_ms,
        label: commit.txn.label.clone(),
        ops: commit.txn.ops.clone(),
    };
    let bytes = encode_wal_complete_v2(commit.seq - 1, &[record])
        .map_err(|_| StoreError::Invalid("transaction encoding"))?;
    Ok(lz4_flex::compress_prepend_size(&bytes))
}

fn decode_log(payload: &[u8], seq: u64) -> Result<WireWalRecordV2, StoreError> {
    let invalid = || StoreError::Invalid("transaction encoding");
    let (size, _) = lz4_flex::block::uncompressed_size(payload).map_err(|_| invalid())?;
    let base_seq = seq.checked_sub(1).ok_or_else(invalid)?;
    if size > MAX_WAL_FRAME_PAYLOAD + WAL_HEADER_LEN + 32 {
        return Err(invalid());
    }
    let bytes = lz4_flex::decompress_size_prepended(payload).map_err(|_| invalid())?;
    let mut wal = scan_wal_any(&bytes, base_seq, seq).map_err(|_| invalid())?;
    if wal.records.len() != 1 || wal.valid_bytes != bytes.len() {
        return Err(invalid());
    }
    wal.records.pop().ok_or_else(invalid)
}

fn integer<T, U: TryFrom<T>>(value: T, name: &'static str) -> Result<U, StoreError> {
    U::try_from(value).map_err(|_| StoreError::Invalid(name))
}
