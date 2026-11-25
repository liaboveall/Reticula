//! Read-only tables attached to workbooks: their rows, their files, and what
//! their owners may store.
//!
//! A table is declared, uploaded in pieces and then checked. How far the upload
//! has come is the length of its file and nothing else. Once checked, the file
//! never changes; its SHA-256 is its identity.

mod files;
mod gates;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Bytes;
use futures_util::{Stream, StreamExt};
use reticula_sync::MemberRole;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep_until, timeout};
use utoipa::ToSchema;
use uuid::Uuid;

use super::history::validate_label_text;
use super::{DELETED_USER_ID, Store, StoreError, collaboration, integer};
use crate::table_file::MIN_TABLE_FILE_BYTES;
use crate::volume;

use files::FinishError;
use gates::{ClosedWorkbook, TableGates, TableGuard};

/// The most bytes of a table one request may add.
pub const MAX_TABLE_CHUNK_BYTES: u64 = 8 * 1024 * 1024;
/// How long an unfinished upload is kept without anything arriving.
pub const TABLE_UPLOAD_IDLE: Duration = Duration::from_secs(60 * 60);
/// How long files that were in no row are kept.
pub const REMOVED_TABLES_KEPT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const MAX_TABLES_PER_WORKBOOK: i64 = 200;
const CONCURRENT_UPLOADS: usize = 4;
/// How long to wait for whoever is at a table, or for a check.
const LOCK_WAIT: Duration = Duration::from_secs(30);
/// How long a body may go without sending anything.
const CHUNK_IDLE: Duration = Duration::from_secs(15);
/// How long a body may take. What arrived by then is kept.
const CHUNK_DEADLINE: Duration = Duration::from_secs(300);
const WRITE_BUFFER_BYTES: usize = 256 * 1024;
const RETRY_SECONDS: u64 = 5;
const RETRY_UPLOAD_SECONDS: u64 = 30;
const FREE_SPACE_FOR: Duration = Duration::from_secs(5);
/// Serializes the transactions that reserve space.
const RESERVE_LOCK: i64 = 0x7274_626c;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TableStatus {
    /// Declared, and not all there or not yet checked.
    Uploading,
    Ready,
    /// Its file was not in the backup the service was restored from.
    Missing,
}

impl TableStatus {
    fn parse(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "uploading" => Ok(Self::Uploading),
            "ready" => Ok(Self::Ready),
            "missing" => Ok(Self::Missing),
            _ => Err(StoreError::Invalid("table status")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TableRecord {
    pub id: Uuid,
    pub name: String,
    pub position: u32,
    /// 0 until the table is ready.
    pub rows: u64,
    /// 0 until the table is ready.
    pub cols: u32,
    pub bytes: u64,
    /// In lower-case hexadecimal digits.
    pub sha256: String,
    pub status: TableStatus,
    /// How many bytes have arrived, while the table is being uploaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_offset: Option<u64>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TableList {
    /// Only to be compared, and to be asked with.
    pub version: String,
    pub changed: bool,
    /// Left out when nothing changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables: Option<Vec<TableRecord>>,
}

/// A table someone is about to upload.
pub struct NewTable {
    pub id: Uuid,
    pub name: String,
    pub bytes: u64,
    pub sha256: [u8; 32],
}

/// The file of a table that is ready.
pub struct TableFile {
    pub file: std::fs::File,
    pub bytes: u64,
}

/// The row of a table that is ready.
pub struct ReadyTable {
    workbook_id: Uuid,
    id: Uuid,
    pub bytes: u64,
    pub sha256: [u8; 32],
}

/// What the uploads of this server share.
pub(super) struct TableWork {
    gates: TableGates,
    uploads: Arc<Semaphore>,
    checks: Arc<Semaphore>,
    free: Mutex<Option<(Instant, Option<u64>)>>,
}

impl Default for TableWork {
    fn default() -> Self {
        Self {
            gates: TableGates::default(),
            uploads: Arc::new(Semaphore::new(CONCURRENT_UPLOADS)),
            checks: Arc::new(Semaphore::new(1)),
            free: Mutex::new(None),
        }
    }
}

#[derive(sqlx::FromRow)]
struct TableRow {
    id: Uuid,
    name: String,
    position: i32,
    rows: i64,
    cols: i32,
    bytes: i64,
    sha256: Vec<u8>,
    status: String,
    created_at: String,
}

impl TableRow {
    fn bytes(&self) -> u64 {
        self.bytes.unsigned_abs()
    }

    fn record(self) -> Result<TableRecord, StoreError> {
        Ok(TableRecord {
            id: self.id,
            name: self.name,
            position: integer(self.position, "table position")?,
            rows: integer(self.rows, "table rows")?,
            cols: integer(self.cols, "table columns")?,
            bytes: integer(self.bytes, "table bytes")?,
            sha256: hex(&self.sha256),
            status: TableStatus::parse(&self.status)?,
            upload_offset: None,
            created_at: self.created_at,
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn busy(retry_after: u64) -> StoreError {
    StoreError::Busy {
        reason: "table uploads are busy; try again shortly".to_owned(),
        retry_after,
    }
}

impl Store {
    /// The tables of a workbook, unless they are those of `since`.
    ///
    /// Tables that are being uploaded are for those who may upload.
    pub async fn list_tables(
        &self,
        doc: Uuid,
        since: Option<&str>,
        with_uploads: bool,
    ) -> Result<TableList, StoreError> {
        // Read before the tables, which are then at least as new. Restoring a
        // backup raises the epoch, so no version from before it comes back.
        let (epoch, version): (i16, i64) =
            sqlx::query_as("SELECT log_epoch, tables_version FROM workbooks WHERE id = $1")
                .bind(doc)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(StoreError::NotFound)?;
        // Who may upload has another list.
        let kind = if with_uploads { "w" } else { "r" };
        let version = format!("{epoch}.{version}.{kind}");
        if since == Some(version.as_str()) {
            return Ok(TableList {
                version,
                changed: false,
                tables: None,
            });
        }
        let rows = table_rows(&self.pool, doc, None, with_uploads).await?;
        let mut tables = Vec::with_capacity(rows.len());
        for row in rows {
            tables.push(self.with_offset(doc, row.record()?).await?);
        }
        Ok(TableList {
            version,
            changed: true,
            tables: Some(tables),
        })
    }

    async fn with_offset(
        &self,
        doc: Uuid,
        mut record: TableRecord,
    ) -> Result<TableRecord, StoreError> {
        if record.status == TableStatus::Uploading {
            record.upload_offset = Some(self.arrived(doc, record.id).await?.0);
        }
        Ok(record)
    }

    /// Declares a table and reserves its bytes. Declaring it again with the
    /// same ID and bytes changes nothing and says how far the upload has come.
    ///
    /// Returns whether it was declared now.
    pub async fn create_table(
        &self,
        doc: Uuid,
        user: Uuid,
        table: NewTable,
    ) -> Result<(TableRecord, bool), StoreError> {
        let name = validate_label_text(&table.name, "table name")?;
        if table.bytes < MIN_TABLE_FILE_BYTES {
            return Err(StoreError::InvalidInput(
                "this is not a table file".to_owned(),
            ));
        }
        if table.bytes > self.limits.table_max_bytes {
            return Err(StoreError::TooLarge(
                "the table file is larger than this server accepts".to_owned(),
            ));
        }
        let guard = self.enter_table(doc, table.id).await?;
        let free = self.free_bytes().await?;

        let mut db = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(RESERVE_LOCK)
            .execute(&mut *db)
            .await?;
        let owner = self.writable_workbook(&mut db, doc, user).await?;
        if let Some(existing) = find_table(&mut *db, doc, table.id).await? {
            db.rollback().await?;
            if existing.sha256 != table.sha256 || existing.bytes() != table.bytes {
                return Err(StoreError::Conflict("another table has this ID".to_owned()));
            }
            let mut record = existing.record()?;
            if record.status == TableStatus::Uploading {
                record.upload_offset = Some(self.arrived(doc, table.id).await?.0);
            }
            return Ok((record, false));
        }
        self.reserve(&mut db, owner, &table, free).await?;
        let (count, position): (i64, i32) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(MAX(position) + 1, 0) FROM tables WHERE workbook_id = $1",
        )
        .bind(doc)
        .fetch_one(&mut *db)
        .await?;
        if count >= MAX_TABLES_PER_WORKBOOK {
            return Err(StoreError::QuotaExceeded(
                "the workbook has as many tables as it can".to_owned(),
            ));
        }
        // What a deletion that was cut short left behind did not arrive for
        // this table.
        let dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || files::remove(&dir, doc, table.id)).await??;
        sqlx::query(
            "INSERT INTO tables (workbook_id, id, name, position, bytes, sha256, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(doc)
        .bind(table.id)
        .bind(&name)
        .bind(position)
        .bind(integer::<_, i64>(table.bytes, "table bytes")?)
        .bind(table.sha256.as_slice())
        .bind(user)
        .execute(&mut *db)
        .await?;
        bump_version(&mut db, doc).await?;
        let row = find_table(&mut *db, doc, table.id)
            .await?
            .ok_or(StoreError::NotFound)?;
        db.commit().await?;
        drop(guard);
        self.refresh_disk_used().await?;
        Ok((self.with_offset(doc, row.record()?).await?, true))
    }

    /// Takes the bytes of a table out of what the owner, all tables and the
    /// volume may still hold.
    async fn reserve(
        &self,
        db: &mut PgConnection,
        owner: Uuid,
        table: &NewTable,
        free: Option<u64>,
    ) -> Result<(), StoreError> {
        let full = || StoreError::DiskFull("server storage capacity reached".to_owned());
        let unfinished: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM tables t JOIN workbooks w ON w.id = t.workbook_id
                WHERE w.owner_id = $1 AND t.status = 'uploading'
             )",
        )
        .bind(owner)
        .fetch_one(&mut *db)
        .await?;
        if unfinished {
            return Err(StoreError::Busy {
                reason: "another table is being uploaded for the owner of this workbook".to_owned(),
                retry_after: RETRY_UPLOAD_SECONDS,
            });
        }
        let held: i64 =
            sqlx::query_scalar("SELECT table_bytes FROM users WHERE id = $1 FOR UPDATE")
                .bind(owner)
                .fetch_one(&mut *db)
                .await?;
        let with = |held: i64| {
            u64::try_from(held.max(0))
                .unwrap_or(0)
                .saturating_add(table.bytes)
        };
        if with(held) > self.limits.table_user_quota_bytes {
            return Err(StoreError::QuotaExceeded(
                "owner table storage quota exceeded".to_owned(),
            ));
        }
        let (tables, everything): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(table_bytes), 0)::bigint,
                    COALESCE(SUM(storage_bytes + table_bytes), 0)::bigint FROM users",
        )
        .fetch_one(&mut *db)
        .await?;
        if with(tables) > self.limits.table_total_bytes
            || with(everything) > self.limits.disk_hard_bytes
            || free
                .is_some_and(|free| free < self.limits.min_free_bytes.saturating_add(table.bytes))
        {
            return Err(full());
        }
        sqlx::query("UPDATE users SET table_bytes = table_bytes + $2 WHERE id = $1")
            .bind(owner)
            .bind(integer::<_, i64>(table.bytes, "table bytes")?)
            .execute(&mut *db)
            .await?;
        Ok(())
    }

    /// Adds what a body brings to an upload. Returns how much is there then.
    ///
    /// The body goes to the disk as it arrives. When it ends early, what has
    /// arrived stays.
    pub async fn receive_table_data<S, E>(
        &self,
        doc: Uuid,
        id: Uuid,
        user: Uuid,
        offset: u64,
        length: Option<u64>,
        body: &mut S,
    ) -> Result<u64, StoreError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
    {
        let too_large =
            || StoreError::TooLarge("the request has too many bytes of the table".to_owned());
        if length.is_some_and(|length| length > MAX_TABLE_CHUNK_BYTES) {
            return Err(too_large());
        }
        let guard = self.enter_table(doc, id).await?;
        let _slot = Arc::clone(&self.tables.uploads)
            .try_acquire_owned()
            .map_err(|_| busy(RETRY_SECONDS))?;
        let row = self.writable_table(doc, id, user).await?;
        if TableStatus::parse(&row.status)? != TableStatus::Uploading {
            return Err(StoreError::Conflict(
                "the table is not being uploaded".to_owned(),
            ));
        }
        let received = self.take_up(doc, id).await?;
        if offset != received {
            return Err(StoreError::UploadOffset(received));
        }
        let room = row
            .bytes()
            .saturating_sub(received)
            .min(MAX_TABLE_CHUNK_BYTES);
        if length.is_some_and(|length| length > room) {
            return Err(too_large());
        }
        if self
            .free_bytes()
            .await?
            .is_some_and(|free| free < self.limits.min_free_bytes)
        {
            return Err(StoreError::DiskFull(
                "server storage capacity reached".to_owned(),
            ));
        }
        let dir = self.data_dir.clone();
        let file = tokio::task::spawn_blocking(move || files::open_part(&dir, doc, id)).await??;
        let mut file = tokio::fs::File::from_std(file);
        file.set_max_buf_size(WRITE_BUFFER_BYTES);
        file.seek(std::io::SeekFrom::Start(received)).await?;
        let end = write_body(&guard, &mut file, body, room).await;
        // Whatever the end was, the file is as long as it is going to be.
        file.flush().await.map_err(disk_error)?;
        match end? {
            BodyEnd::Complete => {}
            BodyEnd::TooLarge => {
                file.set_len(received).await?;
                return Err(too_large());
            }
            BodyEnd::Closing => return Err(StoreError::NotFound),
            BodyEnd::Interrupted => {
                let length = file.metadata().await?.len();
                return Err(StoreError::UploadOffset(length));
            }
        }
        Ok(file.metadata().await?.len())
    }

    /// Checks an upload that is all there and makes the table ready.
    pub async fn complete_table(
        &self,
        doc: Uuid,
        id: Uuid,
        user: Uuid,
    ) -> Result<TableRecord, StoreError> {
        let _guard = self.enter_table(doc, id).await?;
        let row = self.writable_table(doc, id, user).await?;
        match TableStatus::parse(&row.status)? {
            TableStatus::Uploading => {}
            TableStatus::Ready => return row.record(),
            TableStatus::Missing => {
                return Err(StoreError::Conflict(
                    "the table is not being uploaded".to_owned(),
                ));
            }
        }
        let slot = self.check_slot().await?;
        let dir = self.data_dir.clone();
        let (bytes, sha256) = (row.bytes(), row.sha256);
        let shape = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            files::finish(&dir, doc, id, bytes, &sha256)
        })
        .await?
        .map_err(|error| match error {
            FinishError::Io(error) => error.into(),
            FinishError::Short(length) => StoreError::UploadOffset(length),
            FinishError::Wrong(reason) => StoreError::InvalidInput(reason),
        })?;

        let mut db = self.pool.begin().await?;
        bump_version(&mut db, doc).await?;
        sqlx::query(
            "UPDATE tables SET status = 'ready', rows = $3, cols = $4
             WHERE workbook_id = $1 AND id = $2",
        )
        .bind(doc)
        .bind(id)
        .bind(integer::<_, i64>(shape.rows, "table rows")?)
        .bind(integer::<_, i32>(shape.cols, "table columns")?)
        .execute(&mut *db)
        .await?;
        let row = find_table(&mut *db, doc, id)
            .await?
            .ok_or(StoreError::NotFound)?;
        db.commit().await?;
        row.record()
    }

    async fn check_slot(&self) -> Result<OwnedSemaphorePermit, StoreError> {
        timeout(LOCK_WAIT, Arc::clone(&self.tables.checks).acquire_owned())
            .await
            .map_err(|_| busy(RETRY_SECONDS))?
            .map_err(|_| busy(RETRY_SECONDS))
    }

    /// Renames a table, or moves it to another place in the list.
    pub async fn update_table(
        &self,
        doc: Uuid,
        id: Uuid,
        user: Uuid,
        name: Option<&str>,
        position: Option<u32>,
    ) -> Result<TableRecord, StoreError> {
        let name = name
            .map(|name| validate_label_text(name, "table name"))
            .transpose()?;
        let mut db = self.pool.begin().await?;
        self.writable_workbook(&mut db, doc, user).await?;
        if find_table(&mut *db, doc, id).await?.is_none() {
            return Err(StoreError::NotFound);
        }
        if let Some(name) = name {
            sqlx::query("UPDATE tables SET name = $3 WHERE workbook_id = $1 AND id = $2")
                .bind(doc)
                .bind(id)
                .bind(name)
                .execute(&mut *db)
                .await?;
        }
        if let Some(position) = position {
            let mut order: Vec<Uuid> = sqlx::query_scalar(
                "SELECT id FROM tables WHERE workbook_id = $1 ORDER BY position, created_at, id",
            )
            .bind(doc)
            .fetch_all(&mut *db)
            .await?;
            order.retain(|other| *other != id);
            let at = usize::try_from(position)
                .unwrap_or(usize::MAX)
                .min(order.len());
            order.insert(at, id);
            sqlx::query(
                "UPDATE tables t SET position = o.place - 1
                 FROM UNNEST($2::uuid[]) WITH ORDINALITY AS o(id, place)
                 WHERE t.workbook_id = $1 AND t.id = o.id",
            )
            .bind(doc)
            .bind(&order)
            .execute(&mut *db)
            .await?;
        }
        bump_version(&mut db, doc).await?;
        let row = find_table(&mut *db, doc, id)
            .await?
            .ok_or(StoreError::NotFound)?;
        db.commit().await?;
        self.with_offset(doc, row.record()?).await
    }

    /// Deletes a table, or abandons its upload.
    pub async fn delete_table(&self, doc: Uuid, id: Uuid, user: Uuid) -> Result<(), StoreError> {
        let _closed = timeout(LOCK_WAIT, self.tables.gates.close_table(doc, id))
            .await
            .map_err(|_| busy(RETRY_SECONDS))?;
        let mut db = self.pool.begin().await?;
        let owner = self.writable_workbook(&mut db, doc, user).await?;
        if !release_table(&mut db, doc, id, owner, None).await? {
            return Err(StoreError::NotFound);
        }
        db.commit().await?;
        self.remove_table_files(doc, id).await;
        self.refresh_disk_used().await?;
        Ok(())
    }

    /// A table whose bytes can be read.
    pub async fn ready_table(&self, doc: Uuid, id: Uuid) -> Result<Option<ReadyTable>, StoreError> {
        let row: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT bytes, sha256 FROM tables
             WHERE workbook_id = $1 AND id = $2 AND status = 'ready'",
        )
        .bind(doc)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(bytes, sha256)| {
            Ok(ReadyTable {
                workbook_id: doc,
                id,
                bytes: integer(bytes, "table bytes")?,
                sha256: sha256
                    .try_into()
                    .map_err(|_| StoreError::Invalid("table checksum"))?,
            })
        })
        .transpose()
    }

    /// Opens the file of a table. It was checked when it was completed and is
    /// not hashed again.
    pub async fn open_table(&self, table: &ReadyTable) -> Result<TableFile, StoreError> {
        let path = files::ready_path(&self.data_dir, table.workbook_id, table.id);
        let bytes = table.bytes;
        let opened = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(path)?;
            let length = file.metadata()?.len();
            Ok::<_, std::io::Error>((file, length))
        })
        .await?;
        match opened {
            Ok((file, length)) if length == bytes => Ok(TableFile { file, bytes }),
            Ok(_) => Err(StoreError::Invalid("length of a table file")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::error!(
                    workbook_id = %table.workbook_id,
                    table_id = %table.id,
                    "the file of a table is missing"
                );
                Err(StoreError::NotFound)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Drops the uploads nothing has arrived for in that time, and gives the
    /// bytes reserved for them back. Returns how many.
    pub async fn drop_idle_tables(&self, idle: Duration) -> Result<usize, StoreError> {
        let candidates: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT workbook_id, id FROM tables
             WHERE status = 'uploading' AND created_at <= now() - $1 * interval '1 second'",
        )
        .bind(idle.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        let mut dropped = 0;
        for (doc, id) in candidates {
            match self.drop_if_idle(doc, id, idle).await {
                Ok(true) => dropped += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(workbook_id = %doc, table_id = %id, %error, "could not drop an idle table upload");
                }
            }
        }
        if dropped > 0 {
            self.refresh_disk_used().await?;
            tracing::info!(dropped, "dropped idle table uploads");
        }
        Ok(dropped)
    }

    /// The table was idle a moment ago. By now it may be ready, or another one
    /// that was declared under the same ID.
    async fn drop_if_idle(&self, doc: Uuid, id: Uuid, idle: Duration) -> Result<bool, StoreError> {
        // Someone is at it, so it is not idle.
        let Some(_guard) = self.tables.gates.try_enter(doc, id) else {
            return Ok(false);
        };
        let (_, last) = self.arrived(doc, id).await?;
        if last.is_some_and(|last| is_within(last, idle)) {
            return Ok(false);
        }
        let mut db = self.pool.begin().await?;
        let owner: Option<Uuid> =
            sqlx::query_scalar("SELECT owner_id FROM workbooks WHERE id = $1 FOR UPDATE")
                .bind(doc)
                .fetch_optional(&mut *db)
                .await?;
        let Some(owner) = owner else {
            return Ok(false);
        };
        if !release_table(&mut db, doc, id, owner, Some(idle)).await? {
            return Ok(false);
        }
        db.commit().await?;
        self.remove_table_files(doc, id).await;
        Ok(true)
    }

    /// Puts the files of tables aside that are in no row, and those back that
    /// are in one again. Returns how many were put aside.
    pub(super) async fn sweep_table_files(&self) -> Result<usize, StoreError> {
        // Before the rows: a table declared meanwhile has its row first.
        let dir = self.data_dir.clone();
        let mut found = tokio::task::spawn_blocking(move || files::list(&dir)).await??;
        let rows: Vec<(Uuid, Uuid, bool)> =
            sqlx::query_as("SELECT workbook_id, id, status <> 'uploading' FROM tables")
                .fetch_all(&self.pool)
                .await?;
        let tables = found.len();
        let mut without_file = HashSet::new();
        for (doc, id, was_completed) in rows {
            // An upload without a file has not begun, whatever was put aside.
            if found.remove(&(doc, id)).is_none() && was_completed {
                without_file.insert((doc, id));
            }
        }
        let dir = self.data_dir.clone();
        let back =
            tokio::task::spawn_blocking(move || files::put_back(&dir, &without_file)).await??;
        if back > 0 {
            tracing::info!(back, "put files of tables back that are in a row again");
        }
        if found.is_empty() {
            return Ok(0);
        }
        let aside = found.len();
        if aside * 2 > tables {
            tracing::warn!(
                aside,
                tables,
                "left the files of tables alone that are in no row: they are most of them, and the database may be the wrong one"
            );
            return Ok(0);
        }
        let dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || files::put_aside(&dir, &found)).await??;
        tracing::info!(aside, "put files of tables aside that are in no row");
        Ok(aside)
    }

    /// Removes for good what was put aside longer ago than that.
    pub async fn purge_removed_tables(&self, kept: Duration) -> Result<usize, StoreError> {
        let dir = self.data_dir.clone();
        Ok(tokio::task::spawn_blocking(move || files::purge(&dir, kept)).await??)
    }

    /// Keeps the tables of a workbook, or of several, from being written until
    /// what is returned is dropped.
    pub(super) async fn close_workbooks(&self, mut docs: Vec<Uuid>) -> Vec<ClosedWorkbook> {
        // Always in the same order.
        docs.sort_unstable();
        docs.dedup();
        let mut closed = Vec::with_capacity(docs.len());
        for doc in docs {
            closed.push(self.tables.gates.close_workbook(doc).await);
        }
        closed
    }

    /// Removes the files of all tables of a workbook, which has been deleted.
    pub(super) async fn remove_workbook_tables(&self, doc: Uuid) {
        let dir = files::directory(&self.data_dir, doc);
        match tokio::fs::remove_dir_all(dir).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(workbook_id = %doc, %error, "could not remove the tables of a workbook");
            }
            _ => {}
        }
    }

    async fn remove_table_files(&self, doc: Uuid, id: Uuid) {
        let dir = self.data_dir.clone();
        let removed = tokio::task::spawn_blocking(move || files::remove(&dir, doc, id)).await;
        if !matches!(removed, Ok(Ok(()))) {
            // The next start finds them without a row.
            tracing::warn!(workbook_id = %doc, table_id = %id, "could not remove the files of a table");
        }
    }

    async fn enter_table(&self, doc: Uuid, id: Uuid) -> Result<TableGuard, StoreError> {
        let guard = timeout(LOCK_WAIT, self.tables.gates.enter(doc, id))
            .await
            .map_err(|_| busy(RETRY_SECONDS))?;
        if guard.is_closing() {
            return Err(StoreError::NotFound);
        }
        Ok(guard)
    }

    /// How much of an upload is there, and when anything last arrived.
    async fn arrived(&self, doc: Uuid, id: Uuid) -> Result<(u64, Option<SystemTime>), StoreError> {
        let dir = self.data_dir.clone();
        Ok(tokio::task::spawn_blocking(move || files::arrived(&dir, doc, id)).await??)
    }

    /// The same of one that is about to be added to, whose lock is held.
    async fn take_up(&self, doc: Uuid, id: Uuid) -> Result<u64, StoreError> {
        let dir = self.data_dir.clone();
        Ok(tokio::task::spawn_blocking(move || files::take_up(&dir, doc, id)).await??)
    }

    /// The space left on the volume, as of a few seconds ago at most.
    async fn free_bytes(&self) -> Result<Option<u64>, StoreError> {
        let free = || {
            self.tables
                .free
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        if let Some((read, bytes)) = *free()
            && read.elapsed() < FREE_SPACE_FOR
        {
            return Ok(bytes);
        }
        let dir = self.data_dir.clone();
        let bytes =
            tokio::task::spawn_blocking(move || volume::available_bytes(Path::new(&dir))).await?;
        *free() = Some((Instant::now(), bytes));
        Ok(bytes)
    }

    /// Locks a workbook for someone who may write it. Returns its owner.
    async fn writable_workbook(
        &self,
        db: &mut PgConnection,
        doc: Uuid,
        user: Uuid,
    ) -> Result<Uuid, StoreError> {
        let owner: Uuid =
            sqlx::query_scalar("SELECT owner_id FROM workbooks WHERE id = $1 FOR UPDATE")
                .bind(doc)
                .fetch_optional(&mut *db)
                .await?
                .ok_or(StoreError::NotFound)?;
        let role: Option<String> =
            sqlx::query_scalar("SELECT role FROM members WHERE workbook_id = $1 AND user_id = $2")
                .bind(doc)
                .bind(user)
                .fetch_optional(&mut *db)
                .await?;
        if !may_write(role.as_deref())? {
            return Err(StoreError::Forbidden);
        }
        Ok(owner)
    }

    /// The row of a table for someone who may write the workbook: whoever
    /// waited for the table may have lost that right meanwhile.
    async fn writable_table(
        &self,
        doc: Uuid,
        id: Uuid,
        user: Uuid,
    ) -> Result<TableRow, StoreError> {
        let role: Option<String> =
            sqlx::query_scalar("SELECT role FROM members WHERE workbook_id = $1 AND user_id = $2")
                .bind(doc)
                .bind(user)
                .fetch_optional(&self.pool)
                .await?;
        if !may_write(role.as_deref())? {
            return Err(StoreError::Forbidden);
        }
        find_table(&self.pool, doc, id)
            .await?
            .ok_or(StoreError::NotFound)
    }
}

fn may_write(role: Option<&str>) -> Result<bool, StoreError> {
    Ok(match role {
        Some(role) => matches!(
            collaboration::member_role(role)?,
            MemberRole::Owner | MemberRole::Editor
        ),
        None => false,
    })
}

/// The tables of a workbook in their order, or the one that is asked for.
async fn table_rows(
    db: impl sqlx::PgExecutor<'_>,
    doc: Uuid,
    id: Option<Uuid>,
    with_uploads: bool,
) -> Result<Vec<TableRow>, StoreError> {
    Ok(sqlx::query_as(
        "SELECT id, name, position, rows, cols, bytes, sha256, status,
                to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at
         FROM tables
         WHERE workbook_id = $1 AND ($2::uuid IS NULL OR id = $2) AND ($3 OR status <> 'uploading')
         ORDER BY position, created_at, id",
    )
    .bind(doc)
    .bind(id)
    .bind(with_uploads)
    .fetch_all(db)
    .await?)
}

async fn find_table(
    db: impl sqlx::PgExecutor<'_>,
    doc: Uuid,
    id: Uuid,
) -> Result<Option<TableRow>, StoreError> {
    Ok(table_rows(db, doc, Some(id), true).await?.pop())
}

async fn bump_version(db: &mut PgConnection, doc: Uuid) -> Result<(), StoreError> {
    sqlx::query("UPDATE workbooks SET tables_version = tables_version + 1 WHERE id = $1")
        .bind(doc)
        .execute(db)
        .await?;
    Ok(())
}

/// Deletes the row of a table, or of one that has been an upload for that long,
/// and gives its bytes back to the owner. The workbook is locked. Returns whether
/// there was one.
async fn release_table(
    db: &mut PgConnection,
    doc: Uuid,
    id: Uuid,
    owner: Uuid,
    idle: Option<Duration>,
) -> Result<bool, StoreError> {
    let bytes: Option<i64> = sqlx::query_scalar(
        "DELETE FROM tables
         WHERE workbook_id = $1 AND id = $2
               AND ($3::float8 IS NULL
                    OR (status = 'uploading' AND created_at <= now() - $3 * interval '1 second'))
         RETURNING bytes",
    )
    .bind(doc)
    .bind(id)
    .bind(idle.map(|idle| idle.as_secs_f64()))
    .fetch_optional(&mut *db)
    .await?;
    let Some(bytes) = bytes else {
        return Ok(false);
    };
    release_bytes(db, owner, bytes).await?;
    bump_version(db, doc).await?;
    Ok(true)
}

pub(super) async fn release_bytes(
    db: &mut PgConnection,
    owner: Uuid,
    bytes: i64,
) -> Result<(), StoreError> {
    sqlx::query("UPDATE users SET table_bytes = GREATEST(0, table_bytes - $2) WHERE id = $1")
        .bind(owner)
        .bind(bytes)
        .execute(db)
        .await?;
    Ok(())
}

/// The bytes reserved for all tables of a workbook.
pub(super) async fn workbook_table_bytes(
    db: &mut PgConnection,
    doc: Uuid,
) -> Result<i64, StoreError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(SUM(bytes), 0)::bigint FROM tables WHERE workbook_id = $1",
    )
    .bind(doc)
    .fetch_one(db)
    .await?)
}

/// Hands the tables someone uploaded over to the deleted user.
pub(super) async fn disown_tables(db: &mut PgConnection, user: Uuid) -> Result<(), StoreError> {
    sqlx::query("UPDATE tables SET created_by = $2 WHERE created_by = $1")
        .bind(user)
        .bind(DELETED_USER_ID)
        .execute(db)
        .await?;
    Ok(())
}

/// Whether a moment is that close to now. One ahead of it is of a clock that was
/// set back since.
fn is_within(moment: SystemTime, distance: Duration) -> bool {
    let now = SystemTime::now();
    now.duration_since(moment)
        .or_else(|_| moment.duration_since(now))
        .is_ok_and(|apart| apart < distance)
}

enum BodyEnd {
    Complete,
    /// It has more bytes than there is room for.
    TooLarge,
    /// It broke off, or went on for too long.
    Interrupted,
    /// The table is being deleted.
    Closing,
}

async fn write_body<S, E>(
    guard: &TableGuard,
    file: &mut tokio::fs::File,
    body: &mut S,
    room: u64,
) -> Result<BodyEnd, StoreError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    let deadline = tokio::time::Instant::now() + CHUNK_DEADLINE;
    let mut written = 0_u64;
    loop {
        let next = tokio::select! {
            biased;
            () = guard.closing() => return Ok(BodyEnd::Closing),
            () = sleep_until(deadline) => return Ok(BodyEnd::Interrupted),
            next = timeout(CHUNK_IDLE, body.next()) => next,
        };
        let bytes = match next {
            Ok(Some(Ok(bytes))) => bytes,
            Ok(None) => return Ok(BodyEnd::Complete),
            Ok(Some(Err(_))) | Err(_) => return Ok(BodyEnd::Interrupted),
        };
        written = written.saturating_add(bytes.len() as u64);
        if written > room {
            return Ok(BodyEnd::TooLarge);
        }
        file.write_all(&bytes).await.map_err(disk_error)?;
    }
}

fn disk_error(error: std::io::Error) -> StoreError {
    match error.kind() {
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => {
            StoreError::DiskFull("server storage capacity reached".to_owned())
        }
        _ => error.into(),
    }
}
