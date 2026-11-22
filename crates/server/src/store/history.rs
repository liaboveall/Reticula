//! Revision history, transaction replay chunks, and named versions.

use reticula_wire::v3::{WireCommittedTxnV3, WireTxnV3};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{MAIN_BRANCH, Store, StoreError, decode_log, integer};

const MAX_TXNS_FETCH_LIMIT: i64 = 500;
const MAX_HISTORY_LIMIT: i64 = 200;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub seq: String,
    pub replica: u32,
    pub user_id: Uuid,
    pub author_name: String,
    pub label: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub head_seq: String,
    pub snapshot_seqs: Vec<String>,
    pub entries: Vec<HistoryEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VersionEntry {
    pub id: Uuid,
    pub seq: String,
    pub name: String,
    pub created_by: Uuid,
    pub created_by_name: String,
    pub created_at: String,
}

impl Store {
    pub async fn list_history(
        &self,
        doc: Uuid,
        before_seq: Option<u64>,
        limit: u32,
    ) -> Result<HistoryPage, StoreError> {
        let clamped_limit = i64::from(limit.max(1)).min(MAX_HISTORY_LIMIT);
        let before = before_seq
            .map(|seq| integer::<_, i64>(seq, "before_seq"))
            .transpose()?;
        let mut db = self.pool.begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *db)
            .await?;

        let head = sqlx::query_scalar!(
            "SELECT COALESCE(MAX(seq), 0) AS \"head!\" FROM txns
             WHERE workbook_id = $1 AND branch_id = $2",
            doc,
            MAIN_BRANCH,
        )
        .fetch_one(&mut *db)
        .await?;
        let snapshots = sqlx::query_scalar!(
            "SELECT seq FROM snapshots WHERE workbook_id = $1 AND branch_id = $2 ORDER BY seq ASC",
            doc,
            MAIN_BRANCH,
        )
        .fetch_all(&mut *db)
        .await?;
        let rows = sqlx::query!(
            "SELECT t.seq, t.replica, t.user_id, u.name AS author_name, t.payload,
                    to_char(t.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS \"created_at!\"
             FROM txns t
             JOIN users u ON u.id = t.user_id
             WHERE t.workbook_id = $1 AND t.branch_id = $2 AND ($3::bigint IS NULL OR t.seq < $3)
             ORDER BY t.seq DESC
             LIMIT $4",
            doc,
            MAIN_BRANCH,
            before,
            clamped_limit,
        )
        .fetch_all(&mut *db)
        .await?;
        db.commit().await?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let seq = integer::<_, u64>(row.seq, "transaction seq")?;
            let label = decode_log(&row.payload, seq)?.label;
            entries.push(HistoryEntry {
                seq: seq.to_string(),
                replica: integer(row.replica, "replica")?,
                user_id: row.user_id,
                author_name: row.author_name,
                label,
                created_at: row.created_at,
            });
        }
        let snapshot_seqs = snapshots
            .into_iter()
            .map(|s| integer::<_, u64>(s, "snapshot seq").map(|v| v.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(HistoryPage {
            head_seq: integer::<_, u64>(head, "head seq")?.to_string(),
            snapshot_seqs,
            entries,
        })
    }

    pub async fn fetch_txns_after(
        &self,
        doc: Uuid,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<u8>, StoreError> {
        let after = integer::<_, i64>(after_seq, "after_seq")?;
        let clamped_limit = i64::from(limit.max(1)).min(MAX_TXNS_FETCH_LIMIT);
        let rows = sqlx::query!(
            "SELECT seq, replica, client_seq, user_id, payload
             FROM txns
             WHERE workbook_id = $1 AND branch_id = $2 AND seq > $3
             ORDER BY seq ASC
             LIMIT $4",
            doc,
            MAIN_BRANCH,
            after,
            clamped_limit,
        )
        .fetch_all(&self.pool)
        .await?;

        let txns = tokio::task::spawn_blocking(move || {
            let mut commits = Vec::with_capacity(rows.len());
            for row in rows {
                let seq = integer(row.seq, "transaction seq")?;
                let record = decode_log(&row.payload, seq)?;
                commits.push(WireCommittedTxnV3 {
                    seq,
                    replica: integer(row.replica, "replica")?,
                    client_seq: integer(row.client_seq, "client seq")?,
                    user_id: row.user_id.into_bytes(),
                    timestamp_ms: record.timestamp_ms,
                    txn: WireTxnV3 {
                        label: record.label,
                        ops: record.ops,
                    },
                });
            }
            postcard::to_allocvec(&commits).map_err(|_| StoreError::Invalid("history encoding"))
        })
        .await??;
        Ok(txns)
    }

    pub async fn list_versions(&self, doc: Uuid) -> Result<Vec<VersionEntry>, StoreError> {
        let rows = sqlx::query!(
            "SELECT v.id, v.seq, v.name, v.created_by, u.name AS created_by_name,
                    to_char(v.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS \"created_at!\"
             FROM versions v
             JOIN users u ON u.id = v.created_by
             WHERE v.workbook_id = $1 AND v.branch_id = $2
             ORDER BY v.seq DESC, v.created_at DESC",
            doc,
            MAIN_BRANCH,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(VersionEntry {
                    id: row.id,
                    seq: integer::<_, u64>(row.seq, "version seq")?.to_string(),
                    name: row.name,
                    created_by: row.created_by,
                    created_by_name: row.created_by_name,
                    created_at: row.created_at,
                })
            })
            .collect()
    }

    pub async fn create_version(
        &self,
        doc: Uuid,
        author: Uuid,
        seq: u64,
        raw_name: &str,
    ) -> Result<VersionEntry, StoreError> {
        let name = validate_label_text(raw_name, "version name")?;
        let seq_i64 = integer::<_, i64>(seq, "version seq")?;
        let id = Uuid::new_v4();
        let mut db = self.pool.begin().await?;
        let head = sqlx::query_scalar!(
            "SELECT COALESCE(MAX(seq), 0) AS \"head!\" FROM txns
             WHERE workbook_id = $1 AND branch_id = $2",
            doc,
            MAIN_BRANCH,
        )
        .fetch_one(&mut *db)
        .await?;
        if seq_i64 > head {
            return Err(StoreError::InvalidInput(
                "version sequence exceeds workbook head".to_owned(),
            ));
        }
        let inserted = sqlx::query!(
            "INSERT INTO versions (id, workbook_id, branch_id, seq, name, created_by)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS \"created_at!\"",
            id,
            doc,
            MAIN_BRANCH,
            seq_i64,
            name,
            author,
        )
        .fetch_one(&mut *db)
        .await;
        let created_at = match inserted {
            Ok(row) => row.created_at,
            Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("23505") => {
                return Err(StoreError::Conflict(
                    "a named version with this title already exists".to_owned(),
                ));
            }
            Err(err) => return Err(err.into()),
        };
        let created_by_name = sqlx::query_scalar!("SELECT name FROM users WHERE id = $1", author)
            .fetch_one(&mut *db)
            .await?;
        db.commit().await?;
        Ok(VersionEntry {
            id,
            seq: seq.to_string(),
            name,
            created_by: author,
            created_by_name,
            created_at,
        })
    }

    pub async fn delete_version(&self, doc: Uuid, version_id: Uuid) -> Result<bool, StoreError> {
        let deleted = sqlx::query!(
            "DELETE FROM versions WHERE workbook_id = $1 AND branch_id = $2 AND id = $3",
            doc,
            MAIN_BRANCH,
            version_id,
        )
        .execute(&self.pool)
        .await?;
        Ok(deleted.rows_affected() == 1)
    }
}

pub(super) fn validate_label_text(raw: &str, field: &str) -> Result<String, StoreError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 120 {
        return Err(StoreError::InvalidInput(format!(
            "{field} must be between 1 and 120 characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(StoreError::InvalidInput(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(trimmed.to_owned())
}
