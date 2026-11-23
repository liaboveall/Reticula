//! Branch lifecycle, atomic cross-branch replica allocation, durable AI agent
//! runs/steps, and per-user daily LLM quota persistence (`18-m4.md` §7.1).

use reticula_agent::runner::{AgentStepKind, PersistedStep};
use reticula_core::id::pack_replica_id;
use reticula_core::ops::Workbook;
use reticula_formula::binder::BoundFormula;
use reticula_sync::{CommittedPermute, ReplicaWatermark};
use reticula_wire::convert_v3::ops_from_wire_txn_v3;
use reticula_wire::v3::{WireCommittedTxnV3, WireTxnV3};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{MAIN_BRANCH, SnapshotFile, Store, StoreError, decode_log, integer, snapshot};

/// Branch metadata row exposed via `GET /api/workbooks/{id}/branches`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BranchRecord {
    pub id: Uuid,
    pub workbook_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub fork_seq: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub created_by: Uuid,
    pub est_bytes: u64,
    pub agent_replica: Option<u32>,
    pub merge_seq: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub merged_at: Option<String>,
}

/// Summary of an AI agent run stored in `agent_runs`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunRecord {
    pub id: Uuid,
    pub workbook_id: Uuid,
    pub branch_id: Uuid,
    pub user_id: Uuid,
    pub prompt: String,
    pub status: String,
    pub model: String,
    pub provider: String,
    pub step_count: i32,
    pub resume_count: i32,
    pub prompt_tokens: u64,
    pub cached_prompt_tokens: u64,
    pub completion_tokens: u64,
    pub summary: String,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

/// Full agent run detail including all persisted steps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunDetail {
    pub run: AgentRunRecord,
    pub steps: Vec<PersistedStep>,
}

/// Daily AI usage row from `quotas` for the current UTC/DB date.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DailyQuotaUsage {
    pub llm_tokens: u64,
    pub agent_runs: u32,
    pub txn_count: u32,
}

/// Interrupted run discovered on server startup for crash-recovery evaluation.
#[derive(Clone, Debug)]
pub struct InterruptedAgentRun {
    pub run: AgentRunRecord,
    pub branch_status: String,
    pub agent_replica: Option<u32>,
    pub workbook_epoch: u8,
}

#[derive(sqlx::FromRow)]
struct BranchRow {
    id: Uuid,
    workbook_id: Uuid,
    parent_id: Option<Uuid>,
    fork_seq: i64,
    kind: String,
    status: String,
    title: String,
    created_by: Uuid,
    est_bytes: i64,
    agent_replica: Option<i32>,
    merge_seq: Option<i64>,
    created_at: String,
    updated_at: String,
    merged_at: Option<String>,
}

impl From<BranchRow> for BranchRecord {
    fn from(r: BranchRow) -> Self {
        Self {
            id: r.id,
            workbook_id: r.workbook_id,
            parent_id: r.parent_id,
            fork_seq: r.fork_seq.max(0).to_string(),
            kind: r.kind,
            status: r.status,
            title: r.title,
            created_by: r.created_by,
            est_bytes: u64::try_from(r.est_bytes.max(0)).unwrap_or(0),
            agent_replica: r.agent_replica.and_then(|v| u32::try_from(v).ok()),
            merge_seq: r.merge_seq.map(|v| v.max(0).to_string()),
            created_at: r.created_at,
            updated_at: r.updated_at,
            merged_at: r.merged_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct AgentRunRow {
    id: Uuid,
    workbook_id: Uuid,
    branch_id: Uuid,
    user_id: Uuid,
    prompt: String,
    status: String,
    model: String,
    provider: String,
    step_count: i32,
    resume_count: i32,
    prompt_tokens: i64,
    cached_prompt_tokens: i64,
    completion_tokens: i64,
    summary: String,
    error_message: Option<String>,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
}

impl From<AgentRunRow> for AgentRunRecord {
    fn from(r: AgentRunRow) -> Self {
        Self {
            id: r.id,
            workbook_id: r.workbook_id,
            branch_id: r.branch_id,
            user_id: r.user_id,
            prompt: r.prompt,
            status: r.status,
            model: r.model,
            provider: r.provider,
            step_count: r.step_count,
            resume_count: r.resume_count,
            prompt_tokens: u64::try_from(r.prompt_tokens.max(0)).unwrap_or(0),
            cached_prompt_tokens: u64::try_from(r.cached_prompt_tokens.max(0)).unwrap_or(0),
            completion_tokens: u64::try_from(r.completion_tokens.max(0)).unwrap_or(0),
            summary: r.summary,
            error_message: r.error_message,
            created_at: r.created_at,
            updated_at: r.updated_at,
            finished_at: r.finished_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct AgentStepRow {
    idx: i32,
    kind: String,
    payload: sqlx::types::Json<JsonValue>,
    prompt_tokens: i32,
    cached_prompt_tokens: i32,
    completion_tokens: i32,
    latency_ms: i32,
}

impl Store {
    /// Atomically allocates the next replica ID for `(doc, epoch)` in PostgreSQL
    /// and inserts its row in `replicas`, eliminating cross-branch `StaleActor` races.
    pub async fn allocate_replica(
        &self,
        doc: Uuid,
        epoch: u8,
        user_id: Uuid,
    ) -> Result<u32, StoreError> {
        let mut db = self.pool.begin().await?;
        let seq_opt: Option<i32> = sqlx::query_scalar(
            "UPDATE workbooks
             SET next_per_epoch_replica = next_per_epoch_replica + 1
             WHERE id = $1 AND log_epoch = $2 AND next_per_epoch_replica <= 65534
             RETURNING next_per_epoch_replica - 1",
        )
        .bind(doc)
        .bind(i16::from(epoch))
        .fetch_optional(&mut *db)
        .await?;
        let seq_i32 = seq_opt.ok_or(StoreError::StaleActor)?;
        let seq_u16 = u16::try_from(seq_i32).map_err(|_| StoreError::Invalid("replica seq"))?;
        let replica = pack_replica_id(epoch, seq_u16).ok_or(StoreError::QuotaExceeded(
            "replica ID space exhausted".to_owned(),
        ))?;
        let replica_i32 = integer::<_, i32>(replica, "replica")?;
        sqlx::query("INSERT INTO replicas (workbook_id, replica, user_id) VALUES ($1, $2, $3)")
            .bind(doc)
            .bind(replica_i32)
            .bind(user_id)
            .execute(&mut *db)
            .await?;
        db.commit().await?;
        Ok(replica)
    }

    /// Reads the current `last_client_seq` and `max_id_counter` for `(doc, replica)`.
    pub async fn replica_watermark(
        &self,
        doc: Uuid,
        replica: u32,
    ) -> Result<(u64, u32), StoreError> {
        let replica_i32 = integer::<_, i32>(replica, "replica")?;
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT last_client_seq, max_id_counter FROM replicas WHERE workbook_id = $1 AND replica = $2",
        )
        .bind(doc)
        .bind(replica_i32)
        .fetch_optional(&self.pool)
        .await?;
        let (last_cseq, max_idc) = row.ok_or(StoreError::NotFound)?;
        Ok((
            u64::try_from(last_cseq.max(0)).unwrap_or(0),
            u32::try_from(max_idc.max(0)).unwrap_or(0),
        ))
    }

    pub async fn list_branches(&self, doc: Uuid) -> Result<Vec<BranchRecord>, StoreError> {
        let rows: Vec<BranchRow> = sqlx::query_as(
            "SELECT id, workbook_id, parent_id, fork_seq, kind, status, title, created_by,
                    est_bytes, agent_replica, merge_seq,
                    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
                    to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at,
                    to_char(merged_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS merged_at
             FROM branches
             WHERE workbook_id = $1
             ORDER BY CASE WHEN kind = 'main' THEN 0 ELSE 1 END, created_at DESC
             LIMIT 100",
        )
        .bind(doc)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_branch(
        &self,
        doc: Uuid,
        branch_id: Uuid,
    ) -> Result<Option<BranchRecord>, StoreError> {
        let row: Option<BranchRow> = sqlx::query_as(
            "SELECT id, workbook_id, parent_id, fork_seq, kind, status, title, created_by,
                    est_bytes, agent_replica, merge_seq,
                    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
                    to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at,
                    to_char(merged_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS merged_at
             FROM branches
             WHERE workbook_id = $1 AND id = $2",
        )
        .bind(doc)
        .bind(branch_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Creates a new branch forked from `MAIN_BRANCH` at `fork_seq` and writes
    /// its immutable `seq = 0` base snapshot before publishing DB metadata.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_branch(
        &self,
        doc: Uuid,
        created_by: Uuid,
        kind: &str,
        title: &str,
        fork_seq: u64,
        base_workbook: Workbook<BoundFormula>,
        watermarks: Vec<ReplicaWatermark>,
        permutes: Vec<CommittedPermute>,
        agent_replica: Option<u32>,
    ) -> Result<BranchRecord, StoreError> {
        if kind != "ai_draft" && kind != "user" {
            return Err(StoreError::InvalidInput(
                "branch kind must be `ai_draft` or `user`".to_owned(),
            ));
        }
        if self.disk_over_hard_limit() {
            return Err(StoreError::DiskFull(
                "server storage capacity reached".to_owned(),
            ));
        }
        let branch_id = Uuid::new_v4();
        let est_bytes = integer::<_, i64>(base_workbook.estimated_bytes(), "branch est_bytes")?;
        let fork_seq_i64 = integer::<_, i64>(fork_seq, "fork_seq")?;
        let agent_rep_i32 = agent_replica
            .map(|r| integer::<_, i32>(r, "agent_replica"))
            .transpose()?;
        let dir = self.data_dir.clone();

        let snap = tokio::task::spawn_blocking(move || {
            SnapshotFile::write_branch(
                &dir,
                doc,
                branch_id,
                0,
                &base_workbook,
                watermarks,
                &permutes,
            )
        })
        .await??;
        let snap_size_i64 = integer::<_, i64>(snap.size(), "snapshot size")?;

        let result: Result<(), StoreError> = async {
            let mut db = self.pool.begin().await?;
            let owner_id: Uuid = sqlx::query_scalar(
                "SELECT owner_id FROM workbooks WHERE id = $1 FOR UPDATE",
            )
            .bind(doc)
            .fetch_optional(&mut *db)
            .await?
            .ok_or(StoreError::NotFound)?;

            let user_bytes: i64 = sqlx::query_scalar(
                "SELECT storage_bytes FROM users WHERE id = $1 FOR UPDATE",
            )
            .bind(owner_id)
            .fetch_one(&mut *db)
            .await?;
            let used = u64::try_from(user_bytes.max(0)).unwrap_or(0);
            if used.saturating_add(snap.size()) > self.limits.user_quota_bytes {
                return Err(StoreError::QuotaExceeded(
                    "owner storage quota exceeded".to_owned(),
                ));
            }

            sqlx::query(
                "INSERT INTO branches
                 (id, workbook_id, parent_id, fork_seq, kind, status, title, created_by, est_bytes, agent_replica)
                 VALUES ($1, $2, $3, $4, $5, 'active', $6, $7, $8, $9)",
            )
            .bind(branch_id)
            .bind(doc)
            .bind(MAIN_BRANCH)
            .bind(fork_seq_i64)
            .bind(kind)
            .bind(title)
            .bind(created_by)
            .bind(est_bytes)
            .bind(agent_rep_i32)
            .execute(&mut *db)
            .await?;

            snap.publish_in_tx(&mut db).await?;
            sqlx::query("UPDATE users SET storage_bytes = storage_bytes + $2 WHERE id = $1")
                .bind(owner_id)
                .bind(snap_size_i64)
                .execute(&mut *db)
                .await?;
            db.commit().await?;
            Ok(())
        }
        .await;

        if let Err(err) = result {
            let branch_dir = self
                .data_dir
                .join("snapshots")
                .join(doc.to_string())
                .join("branches")
                .join(branch_id.to_string());
            let _ = tokio::fs::remove_dir_all(branch_dir).await;
            return Err(err);
        }
        self.refresh_disk_used().await?;
        self.get_branch(doc, branch_id)
            .await?
            .ok_or(StoreError::NotFound)
    }

    pub async fn abandon_branch(&self, doc: Uuid, branch_id: Uuid) -> Result<(), StoreError> {
        if branch_id == MAIN_BRANCH {
            return Err(StoreError::InvalidInput(
                "cannot abandon the main branch".to_owned(),
            ));
        }
        let mut db = self.pool.begin().await?;
        let res = sqlx::query(
            "UPDATE branches SET status = 'abandoned', updated_at = now()
             WHERE workbook_id = $1 AND id = $2 AND kind <> 'main'",
        )
        .bind(doc)
        .bind(branch_id)
        .execute(&mut *db)
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        sqlx::query(
            "UPDATE agent_runs SET status = 'rejected', updated_at = now(), finished_at = COALESCE(finished_at, now())
             WHERE workbook_id = $1 AND branch_id = $2 AND status = 'waiting_review'",
        )
        .bind(doc)
        .bind(branch_id)
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        Ok(())
    }

    pub async fn mark_branch_merged(
        &self,
        doc: Uuid,
        branch_id: Uuid,
        merge_seq: u64,
    ) -> Result<(), StoreError> {
        let merge_seq_i64 = integer::<_, i64>(merge_seq, "merge_seq")?;
        let mut db = self.pool.begin().await?;
        sqlx::query(
            "UPDATE branches
             SET status = 'merged', merge_seq = $3, merged_at = now(), updated_at = now()
             WHERE workbook_id = $1 AND id = $2",
        )
        .bind(doc)
        .bind(branch_id)
        .bind(merge_seq_i64)
        .execute(&mut *db)
        .await?;
        sqlx::query(
            "UPDATE agent_runs
             SET status = 'merged', updated_at = now(), finished_at = COALESCE(finished_at, now())
             WHERE workbook_id = $1 AND branch_id = $2 AND status IN ('waiting_review', 'running')",
        )
        .bind(doc)
        .bind(branch_id)
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        Ok(())
    }

    /// Loads every committed transaction on `(doc, branch_id)` in ascending `seq` order.
    pub async fn load_branch_txns(
        &self,
        doc: Uuid,
        branch_id: Uuid,
    ) -> Result<Vec<WireCommittedTxnV3>, StoreError> {
        let rows: Vec<(i64, i32, i64, Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT seq, replica, client_seq, user_id, payload
             FROM txns
             WHERE workbook_id = $1 AND branch_id = $2
             ORDER BY seq ASC",
        )
        .bind(doc)
        .bind(branch_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (seq_i64, replica_i32, client_seq_i64, user_id, payload) in rows {
            let seq = integer(seq_i64, "txn seq")?;
            let record = decode_log(&payload, seq)?;
            out.push(WireCommittedTxnV3 {
                seq,
                replica: integer(replica_i32, "replica")?,
                client_seq: integer(client_seq_i64, "client seq")?,
                user_id: user_id.into_bytes(),
                timestamp_ms: record.timestamp_ms,
                txn: WireTxnV3 {
                    label: record.label,
                    ops: record.ops,
                },
            });
        }
        Ok(out)
    }

    /// Reconstructs the workbook state of `(doc, branch_id)` at `target_seq`
    /// (for example `fork_seq` on `MAIN_BRANCH` or `seq = 0` on a draft branch).
    pub async fn load_workbook_at_seq(
        &self,
        doc: Uuid,
        branch_id: Uuid,
        target_seq: u64,
    ) -> Result<Workbook<BoundFormula>, StoreError> {
        let target_i64 = integer::<_, i64>(target_seq, "target_seq")?;
        let mut db = self.pool.begin().await?;
        let snaps = snapshot::list_desc_branch(&mut db, doc, branch_id).await?;
        let mut chosen = None;
        for cand in snaps {
            if cand.seq <= target_i64
                && tokio::fs::try_exists(self.data_dir.join(cand.storage_key())).await?
            {
                chosen = Some(cand);
                break;
            }
        }
        let snap_seq = chosen.as_ref().map_or(0, |s| s.seq);
        let tail: Vec<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT seq, payload FROM txns
             WHERE workbook_id = $1 AND branch_id = $2 AND seq > $3 AND seq <= $4
             ORDER BY seq ASC",
        )
        .bind(doc)
        .bind(branch_id)
        .bind(snap_seq)
        .bind(target_i64)
        .fetch_all(&mut *db)
        .await?;
        db.commit().await?;

        let dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || {
            let (mut wb, _) = match chosen {
                Some(file) => file.read_snapshot(&dir)?,
                None => (Workbook::new(), Vec::new()),
            };
            for (seq_i64, payload) in tail {
                let seq = integer(seq_i64, "txn seq")?;
                let record = decode_log(&payload, seq)?;
                let (_, ops) = ops_from_wire_txn_v3(WireTxnV3 {
                    label: record.label,
                    ops: record.ops,
                })
                .map_err(|_| StoreError::Invalid("txn decode"))?;
                for op in ops {
                    wb.apply(op)
                        .map_err(|_| StoreError::Invalid("txn replay"))?;
                }
            }
            Ok(wb)
        })
        .await?
    }

    pub async fn create_agent_run(
        &self,
        doc: Uuid,
        branch_id: Uuid,
        user_id: Uuid,
        prompt: &str,
        model: &str,
        provider: &str,
    ) -> Result<AgentRunRecord, StoreError> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO agent_runs (id, workbook_id, branch_id, user_id, prompt, status, model, provider)
             VALUES ($1, $2, $3, $4, $5, 'running', $6, $7)",
        )
        .bind(id)
        .bind(doc)
        .bind(branch_id)
        .bind(user_id)
        .bind(prompt)
        .bind(model)
        .bind(provider)
        .execute(&self.pool)
        .await?;
        self.get_agent_run(doc, id)
            .await?
            .ok_or(StoreError::NotFound)
    }

    pub async fn get_agent_run(
        &self,
        doc: Uuid,
        run_id: Uuid,
    ) -> Result<Option<AgentRunRecord>, StoreError> {
        let row: Option<AgentRunRow> = sqlx::query_as(
            "SELECT id, workbook_id, branch_id, user_id, prompt, status, model, provider,
                    step_count, resume_count, prompt_tokens, cached_prompt_tokens, completion_tokens,
                    summary, error_message,
                    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
                    to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at,
                    to_char(finished_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS finished_at
             FROM agent_runs
             WHERE workbook_id = $1 AND id = $2",
        )
        .bind(doc)
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    pub async fn list_agent_runs(&self, doc: Uuid) -> Result<Vec<AgentRunRecord>, StoreError> {
        let rows: Vec<AgentRunRow> = sqlx::query_as(
            "SELECT id, workbook_id, branch_id, user_id, prompt, status, model, provider,
                    step_count, resume_count, prompt_tokens, cached_prompt_tokens, completion_tokens,
                    summary, error_message,
                    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
                    to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at,
                    to_char(finished_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS finished_at
             FROM agent_runs
             WHERE workbook_id = $1
             ORDER BY created_at DESC
             LIMIT 50",
        )
        .bind(doc)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn append_agent_step(
        &self,
        run_id: Uuid,
        user_id: Uuid,
        step: &PersistedStep,
    ) -> Result<(), StoreError> {
        let mut db = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO agent_steps
             (run_id, idx, kind, payload, prompt_tokens, cached_prompt_tokens, completion_tokens, latency_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (run_id, idx) DO NOTHING",
        )
        .bind(run_id)
        .bind(step.idx)
        .bind(step.kind.as_str())
        .bind(sqlx::types::Json(&step.payload))
        .bind(step.prompt_tokens)
        .bind(step.cached_prompt_tokens)
        .bind(step.completion_tokens)
        .bind(step.latency_ms)
        .execute(&mut *db)
        .await?;

        let add_steps = i32::from(matches!(
            step.kind,
            AgentStepKind::AssistantThought | AgentStepKind::ToolCall
        ));
        sqlx::query(
            "UPDATE agent_runs
             SET step_count = step_count + $2,
                 prompt_tokens = prompt_tokens + $3,
                 cached_prompt_tokens = cached_prompt_tokens + $4,
                 completion_tokens = completion_tokens + $5,
                 heartbeat_at = now(),
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(run_id)
        .bind(add_steps)
        .bind(i64::from(step.prompt_tokens.max(0)))
        .bind(i64::from(step.cached_prompt_tokens.max(0)))
        .bind(i64::from(step.completion_tokens.max(0)))
        .execute(&mut *db)
        .await?;

        let total_tok =
            i64::from(step.prompt_tokens.max(0)) + i64::from(step.completion_tokens.max(0));
        if total_tok > 0 {
            sqlx::query(
                "INSERT INTO quotas (user_id, day, llm_tokens, agent_runs, txn_count)
                 VALUES ($1, CURRENT_DATE, $2, 0, 0)
                 ON CONFLICT (user_id, day)
                 DO UPDATE SET llm_tokens = quotas.llm_tokens + EXCLUDED.llm_tokens",
            )
            .bind(user_id)
            .bind(total_tok)
            .execute(&mut *db)
            .await?;
        }
        db.commit().await?;
        Ok(())
    }

    pub async fn list_agent_steps(&self, run_id: Uuid) -> Result<Vec<PersistedStep>, StoreError> {
        let rows: Vec<AgentStepRow> = sqlx::query_as(
            "SELECT idx, kind, payload, prompt_tokens, cached_prompt_tokens, completion_tokens, latency_ms
             FROM agent_steps
             WHERE run_id = $1
             ORDER BY idx ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let kind =
                AgentStepKind::parse(&row.kind).ok_or(StoreError::Invalid("agent_steps.kind"))?;
            out.push(PersistedStep {
                idx: row.idx,
                kind,
                payload: row.payload.0,
                prompt_tokens: row.prompt_tokens,
                cached_prompt_tokens: row.cached_prompt_tokens,
                completion_tokens: row.completion_tokens,
                latency_ms: row.latency_ms,
            });
        }
        Ok(out)
    }

    pub async fn finish_agent_run(
        &self,
        run_id: Uuid,
        status: &str,
        summary: &str,
        error_message: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE agent_runs
             SET status = $2,
                 summary = $3,
                 error_message = $4,
                 updated_at = now(),
                 finished_at = now()
             WHERE id = $1",
        )
        .bind(run_id)
        .bind(status)
        .bind(summary)
        .bind(error_message)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn increment_agent_resume_count(&self, run_id: Uuid) -> Result<i32, StoreError> {
        let next: i32 = sqlx::query_scalar(
            "UPDATE agent_runs
             SET resume_count = resume_count + 1,
                 status = 'running',
                 heartbeat_at = now(),
                 updated_at = now()
             WHERE id = $1
             RETURNING resume_count",
        )
        .bind(run_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(next)
    }

    /// Scans `queued` / `running` runs on startup alongside workbook epoch and branch metadata.
    pub async fn list_interrupted_agent_runs(
        &self,
    ) -> Result<Vec<InterruptedAgentRun>, StoreError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            workbook_id: Uuid,
            branch_id: Uuid,
            user_id: Uuid,
            prompt: String,
            status: String,
            model: String,
            provider: String,
            step_count: i32,
            resume_count: i32,
            prompt_tokens: i64,
            cached_prompt_tokens: i64,
            completion_tokens: i64,
            summary: String,
            error_message: Option<String>,
            created_at: String,
            updated_at: String,
            finished_at: Option<String>,
            branch_status: String,
            agent_replica: Option<i32>,
            log_epoch: i16,
        }
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT r.id, r.workbook_id, r.branch_id, r.user_id, r.prompt, r.status, r.model, r.provider,
                    r.step_count, r.resume_count, r.prompt_tokens, r.cached_prompt_tokens, r.completion_tokens,
                    r.summary, r.error_message,
                    to_char(r.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
                    to_char(r.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at,
                    to_char(r.finished_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS finished_at,
                    b.status AS branch_status,
                    b.agent_replica,
                    w.log_epoch
             FROM agent_runs r
             JOIN branches b ON b.workbook_id = r.workbook_id AND b.id = r.branch_id
             JOIN workbooks w ON w.id = r.workbook_id
             WHERE r.status IN ('queued', 'running')
             ORDER BY r.created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let run = AgentRunRecord {
                    id: row.id,
                    workbook_id: row.workbook_id,
                    branch_id: row.branch_id,
                    user_id: row.user_id,
                    prompt: row.prompt,
                    status: row.status,
                    model: row.model,
                    provider: row.provider,
                    step_count: row.step_count,
                    resume_count: row.resume_count,
                    prompt_tokens: u64::try_from(row.prompt_tokens.max(0)).unwrap_or(0),
                    cached_prompt_tokens: u64::try_from(row.cached_prompt_tokens.max(0))
                        .unwrap_or(0),
                    completion_tokens: u64::try_from(row.completion_tokens.max(0)).unwrap_or(0),
                    summary: row.summary,
                    error_message: row.error_message,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                    finished_at: row.finished_at,
                };
                Ok(InterruptedAgentRun {
                    run,
                    branch_status: row.branch_status,
                    agent_replica: row.agent_replica.and_then(|v| u32::try_from(v).ok()),
                    workbook_epoch: integer(row.log_epoch, "log_epoch")?,
                })
            })
            .collect()
    }

    pub async fn get_daily_quota(&self, user_id: Uuid) -> Result<DailyQuotaUsage, StoreError> {
        let row: Option<(i64, i32, i32)> = sqlx::query_as(
            "SELECT llm_tokens, agent_runs, txn_count FROM quotas WHERE user_id = $1 AND day = CURRENT_DATE",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((tokens, runs, txns)) => DailyQuotaUsage {
                llm_tokens: u64::try_from(tokens.max(0)).unwrap_or(0),
                agent_runs: u32::try_from(runs.max(0)).unwrap_or(0),
                txn_count: u32::try_from(txns.max(0)).unwrap_or(0),
            },
            None => DailyQuotaUsage::default(),
        })
    }

    /// Atomically checks and reserves one agent run against the user's daily quota.
    pub async fn try_consume_daily_run_quota(
        &self,
        user_id: Uuid,
        max_runs: u32,
        max_tokens: u64,
    ) -> Result<DailyQuotaUsage, StoreError> {
        let mut db = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO quotas (user_id, day, llm_tokens, agent_runs, txn_count)
             VALUES ($1, CURRENT_DATE, 0, 0, 0)
             ON CONFLICT (user_id, day) DO NOTHING",
        )
        .bind(user_id)
        .execute(&mut *db)
        .await?;
        let (tokens, runs, txns): (i64, i32, i32) = sqlx::query_as(
            "SELECT llm_tokens, agent_runs, txn_count
             FROM quotas
             WHERE user_id = $1 AND day = CURRENT_DATE
             FOR UPDATE",
        )
        .bind(user_id)
        .fetch_one(&mut *db)
        .await?;
        let used_tokens = u64::try_from(tokens.max(0)).unwrap_or(0);
        let used_runs = u32::try_from(runs.max(0)).unwrap_or(0);
        if used_runs >= max_runs {
            return Err(StoreError::QuotaExceeded(format!(
                "daily AI run limit reached ({used_runs}/{max_runs})"
            )));
        }
        if used_tokens >= max_tokens {
            return Err(StoreError::QuotaExceeded(format!(
                "daily AI token limit reached ({used_tokens}/{max_tokens})"
            )));
        }
        sqlx::query(
            "UPDATE quotas SET agent_runs = agent_runs + 1 WHERE user_id = $1 AND day = CURRENT_DATE",
        )
        .bind(user_id)
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        Ok(DailyQuotaUsage {
            llm_tokens: used_tokens,
            agent_runs: used_runs + 1,
            txn_count: u32::try_from(txns.max(0)).unwrap_or(0),
        })
    }

    /// Cleans up disk snapshots for merged/abandoned branches older than `days` days,
    /// deleting DB rows first in a transaction so `restore.sh` never observes dangling metadata.
    pub async fn cleanup_expired_branch_snapshots(&self, days: i32) -> Result<u64, StoreError> {
        let mut db = self.pool.begin().await?;
        let rows: Vec<(Uuid, Uuid, i64, String, i64, Uuid)> = sqlx::query_as(
            "SELECT s.workbook_id, s.branch_id, s.seq, s.storage_key, s.size_bytes, w.owner_id
             FROM snapshots s
             JOIN branches b ON b.workbook_id = s.workbook_id AND b.id = s.branch_id
             JOIN workbooks w ON w.id = s.workbook_id
             WHERE b.kind <> 'main'
               AND b.status IN ('merged', 'abandoned')
               AND b.updated_at < now() - ($1 * interval '1 day')",
        )
        .bind(days)
        .fetch_all(&mut *db)
        .await?;
        if rows.is_empty() {
            return Ok(0);
        }
        let mut keys = Vec::with_capacity(rows.len());
        for (wb_id, br_id, seq, key, size_bytes, owner_id) in rows {
            sqlx::query(
                "DELETE FROM snapshots WHERE workbook_id = $1 AND branch_id = $2 AND seq = $3",
            )
            .bind(wb_id)
            .bind(br_id)
            .bind(seq)
            .execute(&mut *db)
            .await?;
            sqlx::query(
                "UPDATE users SET storage_bytes = GREATEST(0, storage_bytes - $2) WHERE id = $1",
            )
            .bind(owner_id)
            .bind(size_bytes)
            .execute(&mut *db)
            .await?;
            keys.push(key);
        }
        db.commit().await?;
        let count = keys.len() as u64;
        for key in keys {
            let _ = tokio::fs::remove_file(self.data_dir.join(key)).await;
        }
        self.refresh_disk_used().await?;
        Ok(count)
    }
}
