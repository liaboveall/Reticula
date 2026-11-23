//! Server-side AI analyst orchestration: concurrency guards, shared memory
//! budget leases, `ServerBranchSink` (draft commits + purple AI presence cursor),
//! SSE live event streaming, and startup crash-recovery / safe resume (`18-m4.md`).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use reticula_agent::diff::{BranchDiffPlan, compute_branch_diff};
use reticula_agent::llm::{LlmProvider, OpenAiCompatProvider, ScriptedMockProvider};
use reticula_agent::runner::{
    AgentStreamEvent, PersistedStep, StepStore, can_safely_resume, run_agent_loop,
};
use reticula_agent::tools::{AgentPresenceHint, AgentWorkbookSink, BoxFuture};
use reticula_agent::{
    AgentError, AgentRunConfig, DEFAULT_MAX_STEPS, DEFAULT_MAX_TOKENS_PER_RUN,
    DEFAULT_RUN_TIMEOUT_SECS, LlmConfig,
};
use reticula_core::id::{IdAllocator, IdRun, SheetId, pack_sheet_id, unpack_replica_id};
use reticula_core::ops::{Op, Workbook};
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::FormulaEngine;
use reticula_wire::v2::{WirePresenceRangeV1, WirePresenceV1};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, broadcast, watch};
use uuid::Uuid;

use crate::docs::{BudgetLease, DocRooms, RoomError};
use crate::ops::Metrics;
use crate::store::{AgentRunRecord, BranchRecord, MAIN_BRANCH, Store, StoreError};

/// High bit `0x80` marks server-synthesized AI cursors (`18-m4.md` §6.1, §8.1).
/// Human WebSocket connections are clamped to `0..8`, preventing spoofing.
pub const AI_CURSOR_COLOR_SLOT: u8 = 0x80 | 5;
pub const AI_DISPLAY_NAME: &str = "AI Analyst";
pub const MAX_PROMPT_CHARS: usize = 4_000;

/// Configured AI analyst LLM backend mode.
#[derive(Clone, Debug)]
pub enum AgentProviderMode {
    Disabled,
    Mock,
    Live(LlmConfig),
}

/// Runtime limits and provider settings for [`AgentManager`].
#[derive(Clone, Debug)]
pub struct AgentRuntimeConfig {
    pub mode: AgentProviderMode,
    pub daily_token_limit: u64,
    pub daily_run_limit: u32,
    pub max_concurrent_runs: usize,
    pub run_config: AgentRunConfig,
}

impl Default for AgentRuntimeConfig {
    fn default() -> Self {
        Self {
            mode: AgentProviderMode::Disabled,
            daily_token_limit: 500_000,
            daily_run_limit: 20,
            max_concurrent_runs: 2,
            run_config: AgentRunConfig::default(),
        }
    }
}

impl AgentRuntimeConfig {
    /// Deterministic mock configuration for PG18 integration and E2E tests.
    pub fn mock_for_tests() -> Self {
        Self {
            mode: AgentProviderMode::Mock,
            ..Self::default()
        }
    }

    /// Loads runtime limits and LLM mode from process env + optional `.env.local`.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(raw) = std::env::var("RETICULA_AGENT_DAILY_TOKEN_LIMIT")
            && let Ok(v) = raw.trim().parse::<u64>()
        {
            cfg.daily_token_limit = v;
        }
        if let Ok(raw) = std::env::var("RETICULA_AGENT_DAILY_RUN_LIMIT")
            && let Ok(v) = raw.trim().parse::<u32>()
        {
            cfg.daily_run_limit = v;
        }
        if let Ok(raw) = std::env::var("RETICULA_MAX_CONCURRENT_AGENT_RUNS")
            && let Ok(v) = raw.trim().parse::<usize>()
        {
            cfg.max_concurrent_runs = v.max(1);
        }
        if let Ok(raw) = std::env::var("RETICULA_AGENT_MAX_STEPS")
            && let Ok(v) = raw.trim().parse::<u32>()
        {
            cfg.run_config.max_steps = v.clamp(1, 100);
        } else {
            cfg.run_config.max_steps = DEFAULT_MAX_STEPS;
        }
        if let Ok(raw) = std::env::var("RETICULA_AGENT_MAX_TOKENS_PER_RUN")
            && let Ok(v) = raw.trim().parse::<u64>()
        {
            cfg.run_config.max_tokens = v.max(1_000);
        } else {
            cfg.run_config.max_tokens = DEFAULT_MAX_TOKENS_PER_RUN;
        }
        if let Ok(raw) = std::env::var("RETICULA_AGENT_TIMEOUT_SECS")
            && let Ok(v) = raw.trim().parse::<u64>()
        {
            cfg.run_config.timeout = Duration::from_secs(v.clamp(5, 900));
        } else {
            cfg.run_config.timeout = Duration::from_secs(DEFAULT_RUN_TIMEOUT_SECS);
        }

        if std::env::var("RETICULA_LLM_MODE")
            .ok()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("mock"))
        {
            cfg.mode = AgentProviderMode::Mock;
            return cfg;
        }

        match LlmConfig::from_env_and_dotenv(Some(Path::new(".env.local"))) {
            Ok(Some(llm)) => {
                cfg.mode = AgentProviderMode::Live(llm);
            }
            Ok(None) => {
                cfg.mode = AgentProviderMode::Disabled;
            }
            Err(err) => {
                tracing::warn!(error = %err, "LLM configuration invalid; cloud AI analyst disabled");
                cfg.mode = AgentProviderMode::Disabled;
            }
        }
        cfg
    }
}

/// Live SSE message forwarded to `/api/workbooks/{id}/agent/runs/{rid}/events`.
/// Only durable `step` events carry `id: Some(step_cursor)` so browser `Last-Event-ID`
/// reconnects never skip durable steps after receiving ephemeral `text_delta` / `presence` frames.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SseMessage {
    pub id: Option<u64>,
    pub event: String,
    pub data: JsonValue,
}

#[derive(Debug, Error)]
pub enum AgentStartError {
    #[error("AI analyst is not configured on this server")]
    NotConfigured,
    #[error("{0}")]
    InvalidPrompt(String),
    #[error("an AI analyst run is already in progress for your account")]
    UserAlreadyRunning,
    #[error("server AI analyst capacity or memory budget is saturated")]
    Busy,
    #[error(transparent)]
    Store(#[from] StoreError),
}

struct ActiveRunEntry {
    workbook_id: Uuid,
    branch_id: Uuid,
    cancel_tx: watch::Sender<bool>,
    events_tx: broadcast::Sender<SseMessage>,
    done_rx: watch::Receiver<bool>,
}

#[derive(Default)]
struct ActiveRuns {
    by_run: HashMap<Uuid, ActiveRunEntry>,
    by_user: HashMap<Uuid, HashSet<Uuid>>,
    starting_users: HashSet<Uuid>,
}

/// RAII reservation preventing concurrent `start_run` calls by the same user from
/// racing past the per-user active-run guard across async setup awaits.
struct StartingUserGuard {
    active: Arc<Mutex<ActiveRuns>>,
    user_id: Uuid,
    released: bool,
}

impl StartingUserGuard {
    fn disarm(mut self) {
        self.released = true;
    }
}

impl Drop for StartingUserGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let active = Arc::clone(&self.active);
        let user_id = self.user_id;
        if let Ok(mut guard) = active.try_lock() {
            guard.starting_users.remove(&user_id);
        } else {
            tokio::spawn(async move {
                active.lock().await.starting_users.remove(&user_id);
            });
        }
    }
}

#[derive(Clone)]
pub struct AgentManager {
    store: Store,
    rooms: DocRooms,
    metrics: Arc<Metrics>,
    config: AgentRuntimeConfig,
    provider: Option<Arc<dyn LlmProvider>>,
    global_slots: Arc<Semaphore>,
    active: Arc<Mutex<ActiveRuns>>,
}

impl AgentManager {
    pub fn new(
        store: Store,
        rooms: DocRooms,
        metrics: Arc<Metrics>,
        config: AgentRuntimeConfig,
    ) -> Self {
        let provider: Option<Arc<dyn LlmProvider>> = match &config.mode {
            AgentProviderMode::Disabled => None,
            AgentProviderMode::Mock => Some(Arc::new(ScriptedMockProvider::server_mock())),
            AgentProviderMode::Live(llm_cfg) => match OpenAiCompatProvider::new(llm_cfg.clone()) {
                Ok(p) => Some(Arc::new(p)),
                Err(err) => {
                    tracing::error!(error = %err, "failed to initialize OpenAI-compatible LLM provider");
                    None
                }
            },
        };
        let slots = config.max_concurrent_runs.max(1);
        Self {
            store,
            rooms,
            metrics,
            config,
            provider,
            global_slots: Arc::new(Semaphore::new(slots)),
            active: Arc::new(Mutex::new(ActiveRuns::default())),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.provider.is_some()
    }

    pub fn config(&self) -> &AgentRuntimeConfig {
        &self.config
    }

    /// Starts a new AI draft branch and persistent agent run.
    pub async fn start_run(
        &self,
        workbook_id: Uuid,
        user_id: Uuid,
        raw_prompt: &str,
    ) -> Result<AgentRunRecord, AgentStartError> {
        let Some(provider) = self.provider.as_ref().map(Arc::clone) else {
            return Err(AgentStartError::NotConfigured);
        };
        let prompt = validate_prompt(raw_prompt)?;

        // Atomically reserve the per-user active slot BEFORE any async awaits to eliminate TOCTOU races.
        let starting_guard = {
            let mut guard = self.active.lock().await;
            if guard
                .by_user
                .get(&user_id)
                .is_some_and(|runs| !runs.is_empty())
                || guard.starting_users.contains(&user_id)
            {
                return Err(AgentStartError::UserAlreadyRunning);
            }
            guard.starting_users.insert(user_id);
            StartingUserGuard {
                active: Arc::clone(&self.active),
                user_id,
                released: false,
            }
        };

        let global_permit = Arc::clone(&self.global_slots)
            .try_acquire_owned()
            .map_err(|_| AgentStartError::Busy)?;

        let main_snap = self.rooms.snapshot_state(workbook_id, MAIN_BRANCH).await?;
        let draft_budget_bytes =
            draft_engine_budget_bytes(main_snap.workbook.estimated_bytes() as u64);
        let budget_lease = self
            .rooms
            .acquire_budget_lease(draft_budget_bytes)
            .await
            .map_err(|err| match err {
                RoomError::Busy => AgentStartError::Busy,
                _ => AgentStartError::Store(StoreError::NotFound),
            })?;

        self.store
            .try_consume_daily_run_quota(
                user_id,
                self.config.daily_run_limit,
                self.config.daily_token_limit,
            )
            .await?;

        let agent_replica = self
            .store
            .allocate_replica(workbook_id, main_snap.epoch, user_id)
            .await?;
        let title = summarize_branch_title(&prompt);
        let branch = self
            .store
            .create_branch(
                workbook_id,
                user_id,
                "ai_draft",
                &title,
                main_snap.head_seq,
                main_snap.workbook.clone(),
                main_snap.watermarks,
                main_snap.permutes,
                Some(agent_replica),
            )
            .await?;
        let run = self
            .store
            .create_agent_run(
                workbook_id,
                branch.id,
                user_id,
                &prompt,
                provider.model_name(),
                provider.provider_name(),
            )
            .await?;

        self.spawn_run_task(
            run.clone(),
            main_snap.epoch,
            agent_replica,
            1,
            1,
            main_snap.workbook,
            provider,
            global_permit,
            budget_lease,
        )
        .await;
        starting_guard.disarm();

        Ok(run)
    }

    /// Scans PostgreSQL on startup for `queued` / `running` runs interrupted by a
    /// crash (`SIGKILL` / restart) and either resumes cleanly from persisted
    /// `agent_steps` or transitions unsafe runs deterministically to `failed`
    /// (`18-m4.md` §2.3 & §6.3).
    pub async fn recover_interrupted_runs(&self) -> Result<(), StoreError> {
        let interrupted = self.store.list_interrupted_agent_runs().await?;
        for item in interrupted {
            let run = item.run;
            let Some(agent_replica) = item.agent_replica else {
                self.fail_interrupted(run.id, "missing agent replica metadata")
                    .await?;
                continue;
            };
            if item.branch_status != "active" {
                self.fail_interrupted(run.id, "draft branch is no longer active")
                    .await?;
                continue;
            }
            let (rep_epoch, _) = unpack_replica_id(agent_replica);
            if rep_epoch != item.workbook_epoch {
                self.fail_interrupted(
                    run.id,
                    "workbook log_epoch advanced since run started; cannot resume safely",
                )
                .await?;
                continue;
            }
            let Some(provider) = self.provider.as_ref().map(Arc::clone) else {
                self.fail_interrupted(run.id, "AI analyst is disabled on restart")
                    .await?;
                continue;
            };
            let steps = self.store.list_agent_steps(run.id).await?;
            if !can_safely_resume(&steps, run.resume_count) {
                let reason = if run.resume_count >= 2 {
                    "maximum crash recovery resume limit (2) exceeded"
                } else {
                    "run interrupted mid-mutating tool call; cannot resume idempotently"
                };
                self.fail_interrupted(run.id, reason).await?;
                continue;
            }

            let Ok(global_permit) = Arc::clone(&self.global_slots).try_acquire_owned() else {
                self.fail_interrupted(run.id, "agent concurrency slots saturated on startup")
                    .await?;
                continue;
            };
            let Ok(branch_snap) = self
                .rooms
                .snapshot_state(run.workbook_id, run.branch_id)
                .await
            else {
                self.fail_interrupted(run.id, "failed to load draft branch state on recovery")
                    .await?;
                continue;
            };
            let draft_bytes =
                draft_engine_budget_bytes(branch_snap.workbook.estimated_bytes() as u64);
            let Ok(budget_lease) = self.rooms.acquire_budget_lease(draft_bytes).await else {
                self.fail_interrupted(run.id, "shared memory budget saturated on startup")
                    .await?;
                continue;
            };
            let Ok((last_cseq, max_id_counter)) = self
                .store
                .replica_watermark(run.workbook_id, agent_replica)
                .await
            else {
                self.fail_interrupted(run.id, "missing agent replica watermark on recovery")
                    .await?;
                continue;
            };

            let _next_resume = self.store.increment_agent_resume_count(run.id).await?;
            tracing::info!(
                run_id = %run.id,
                workbook_id = %run.workbook_id,
                branch_id = %run.branch_id,
                persisted_steps = steps.len(),
                "resuming interrupted AI agent run from persisted steps"
            );
            self.spawn_run_task(
                run,
                branch_snap.epoch,
                agent_replica,
                last_cseq.saturating_add(1),
                max_id_counter.saturating_add(1).max(1),
                branch_snap.workbook,
                provider,
                global_permit,
                budget_lease,
            )
            .await;
        }
        Ok(())
    }

    async fn fail_interrupted(&self, run_id: Uuid, reason: &str) -> Result<(), StoreError> {
        tracing::warn!(%run_id, reason, "marking interrupted AI agent run as failed");
        self.store
            .finish_agent_run(run_id, "failed", "", Some(reason))
            .await?;
        self.metrics.record_agent_run_status("failed");
        Ok(())
    }

    /// Cancels a running agent task if active. Returns `true` if a live task was signalled.
    pub async fn cancel_run(&self, run_id: Uuid) -> bool {
        let guard = self.active.lock().await;
        if let Some(entry) = guard.by_run.get(&run_id) {
            let _ = entry.cancel_tx.send(true);
            return true;
        }
        false
    }

    /// Cancels all active runs for `user_id` and waits until every background task
    /// has finished writing to PostgreSQL (required before `delete_account`).
    pub async fn cancel_all_for_user(&self, user_id: Uuid) {
        let receivers: Vec<watch::Receiver<bool>> = {
            let guard = self.active.lock().await;
            guard
                .by_user
                .get(&user_id)
                .map(|run_ids| {
                    run_ids
                        .iter()
                        .filter_map(|rid| guard.by_run.get(rid))
                        .map(|entry| {
                            let _ = entry.cancel_tx.send(true);
                            entry.done_rx.clone()
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        for mut rx in receivers {
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                while !*rx.borrow() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
    }

    /// Cancels all active runs targeting a specific workbook (used before `delete_workbook`).
    pub async fn cancel_all_for_workbook(&self, workbook_id: Uuid) {
        let receivers: Vec<watch::Receiver<bool>> = {
            let guard = self.active.lock().await;
            guard
                .by_run
                .values()
                .filter(|entry| entry.workbook_id == workbook_id)
                .map(|entry| {
                    let _ = entry.cancel_tx.send(true);
                    entry.done_rx.clone()
                })
                .collect()
        };
        for mut rx in receivers {
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                while !*rx.borrow() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
    }

    /// Cancels any active run attached to a specific branch (used before abandoning a branch).
    pub async fn cancel_for_branch(&self, workbook_id: Uuid, branch_id: Uuid) {
        let receivers: Vec<watch::Receiver<bool>> = {
            let guard = self.active.lock().await;
            guard
                .by_run
                .values()
                .filter(|entry| entry.workbook_id == workbook_id && entry.branch_id == branch_id)
                .map(|entry| {
                    let _ = entry.cancel_tx.send(true);
                    entry.done_rx.clone()
                })
                .collect()
        };
        for mut rx in receivers {
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                while !*rx.borrow() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
    }

    /// Returns `true` if an AI analyst task is currently executing on `(workbook_id, branch_id)`.
    pub async fn has_active_run_for_branch(&self, workbook_id: Uuid, branch_id: Uuid) -> bool {
        let guard = self.active.lock().await;
        guard
            .by_run
            .values()
            .any(|entry| entry.workbook_id == workbook_id && entry.branch_id == branch_id)
    }

    /// Subscribes to live SSE events if `run_id` is currently running.
    pub async fn subscribe_events(&self, run_id: Uuid) -> Option<broadcast::Receiver<SseMessage>> {
        let guard = self.active.lock().await;
        guard
            .by_run
            .get(&run_id)
            .map(|entry| entry.events_tx.subscribe())
    }

    /// Computes a full three-way diff plan between `(doc, branch)` base (`seq=0`),
    /// draft head, and live `MAIN_BRANCH` head.
    pub async fn compute_diff_for_branch(
        &self,
        doc: Uuid,
        branch: &BranchRecord,
    ) -> Result<BranchDiffPlan, StoreError> {
        let fork_seq: u64 = branch
            .fork_seq
            .parse()
            .map_err(|_| StoreError::Invalid("branch fork_seq"))?;
        let base = self.store.load_workbook_at_seq(doc, branch.id, 0).await?;
        let draft_snap = self.rooms.snapshot_state(doc, branch.id).await?;
        let main_snap = self.rooms.snapshot_state(doc, MAIN_BRANCH).await?;
        let draft_txns = self.store.load_branch_txns(doc, branch.id).await?;
        let plan = tokio::task::spawn_blocking(move || {
            compute_branch_diff(
                &base,
                &draft_snap.workbook,
                &main_snap.workbook,
                fork_seq,
                main_snap.head_seq,
                &draft_txns,
            )
        })
        .await?;
        Ok(plan)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "spawning a durable run binds run metadata, replica watermarks, workbook, provider, and resource leases"
    )]
    async fn spawn_run_task(
        &self,
        run: AgentRunRecord,
        epoch: u8,
        agent_replica: u32,
        next_client_seq: u64,
        start_id_counter: u32,
        draft_workbook: Workbook<BoundFormula>,
        provider: Arc<dyn LlmProvider>,
        global_permit: OwnedSemaphorePermit,
        budget_lease: BudgetLease,
    ) {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        let (events_tx, _) = broadcast::channel(256);

        {
            let mut guard = self.active.lock().await;
            guard.starting_users.remove(&run.user_id);
            guard.by_run.insert(
                run.id,
                ActiveRunEntry {
                    workbook_id: run.workbook_id,
                    branch_id: run.branch_id,
                    cancel_tx,
                    events_tx: events_tx.clone(),
                    done_rx,
                },
            );
            guard.by_user.entry(run.user_id).or_default().insert(run.id);
        }

        let store = self.store.clone();
        let rooms = self.rooms.clone();
        let metrics = Arc::clone(&self.metrics);
        let active = Arc::clone(&self.active);
        let run_config = self.config.run_config;

        tokio::spawn(async move {
            let _permit = global_permit;
            let _active_guard = ActiveRunMetricGuard::new(Arc::clone(&metrics));

            let engine = tokio::task::spawn_blocking(move || {
                FormulaEngine::from_workbook_recalc_all(draft_workbook)
            })
            .await;

            let engine = match engine {
                Ok(e) => e,
                Err(err) => {
                    let msg = format!("draft engine initialization failed: {err}");
                    let _ = store
                        .finish_agent_run(run.id, "failed", "", Some(&msg))
                        .await;
                    metrics.record_agent_run_status("failed");
                    let _ = events_tx.send(SseMessage {
                        id: None,
                        event: "status".to_owned(),
                        data: json!({ "status": "failed", "summary": "", "error": msg }),
                    });
                    cleanup_active(&active, run.id, run.user_id, &done_tx).await;
                    return;
                }
            };

            let Some(mut sink) = ServerBranchSink::new(
                rooms.clone(),
                run.workbook_id,
                run.branch_id,
                run.user_id,
                epoch,
                agent_replica,
                next_client_seq,
                start_id_counter,
                engine,
                budget_lease,
            ) else {
                let msg = "failed to initialize draft IdAllocator".to_owned();
                let _ = store
                    .finish_agent_run(run.id, "failed", "", Some(&msg))
                    .await;
                metrics.record_agent_run_status("failed");
                cleanup_active(&active, run.id, run.user_id, &done_tx).await;
                return;
            };

            let step_store = PostgresStepStore {
                store: store.clone(),
                run_id: run.id,
                user_id: run.user_id,
            };

            let result = run_agent_loop(
                &run.prompt,
                provider.as_ref(),
                &mut sink,
                &step_store,
                run_config,
                Some(cancel_rx),
                |ev| match ev {
                    AgentStreamEvent::Step { step } => {
                        metrics.add_agent_tokens(
                            u64::try_from(step.prompt_tokens.max(0)).unwrap_or(0),
                            u64::try_from(step.cached_prompt_tokens.max(0)).unwrap_or(0),
                            u64::try_from(step.completion_tokens.max(0)).unwrap_or(0),
                        );
                        let id = u64::try_from(step.idx.max(0)).unwrap_or(0) + 1;
                        let _ = events_tx.send(SseMessage {
                            id: Some(id),
                            event: "step".to_owned(),
                            data: serde_json::to_value(&step).unwrap_or_else(|_| json!({})),
                        });
                    }
                    AgentStreamEvent::TextDelta { text } => {
                        let _ = events_tx.send(SseMessage {
                            id: None,
                            event: "text_delta".to_owned(),
                            data: json!({ "text": text }),
                        });
                    }
                    AgentStreamEvent::Presence { presence } => {
                        let _ = events_tx.send(SseMessage {
                            id: None,
                            event: "presence".to_owned(),
                            data: serde_json::to_value(presence).unwrap_or_else(|_| json!({})),
                        });
                    }
                    AgentStreamEvent::Status { status, summary } => {
                        let _ = events_tx.send(SseMessage {
                            id: None,
                            event: "status".to_owned(),
                            data: json!({ "status": status, "summary": summary }),
                        });
                    }
                },
            )
            .await;

            match result {
                Ok(outcome) => {
                    // Abandon empty draft branches when the analyst answered without edits (`18-m4.md` §6.3).
                    if outcome.commits_written == 0 {
                        let _ = store.abandon_branch(run.workbook_id, run.branch_id).await;
                    }
                    let _ = store
                        .finish_agent_run(run.id, "waiting_review", &outcome.summary, None)
                        .await;
                    metrics.record_agent_run_status("waiting_review");
                    let _ = events_tx.send(SseMessage {
                        id: None,
                        event: "status".to_owned(),
                        data: json!({
                            "status": "waiting_review",
                            "summary": outcome.summary,
                            "commitsWritten": outcome.commits_written,
                        }),
                    });
                }
                Err(AgentError::Cancelled) => {
                    let _ = store
                        .finish_agent_run(run.id, "cancelled", "", Some("cancelled"))
                        .await;
                    metrics.record_agent_run_status("cancelled");
                    let _ = events_tx.send(SseMessage {
                        id: None,
                        event: "status".to_owned(),
                        data: json!({ "status": "cancelled", "summary": "" }),
                    });
                }
                Err(err) => {
                    let msg = err.to_string();
                    let _ = store
                        .finish_agent_run(run.id, "failed", "", Some(&msg))
                        .await;
                    metrics.record_agent_run_status("failed");
                    let _ = events_tx.send(SseMessage {
                        id: None,
                        event: "status".to_owned(),
                        data: json!({ "status": "failed", "summary": "", "error": msg }),
                    });
                }
            }

            cleanup_active(&active, run.id, run.user_id, &done_tx).await;
        });
    }
}

async fn cleanup_active(
    active: &Arc<Mutex<ActiveRuns>>,
    run_id: Uuid,
    user_id: Uuid,
    done_tx: &watch::Sender<bool>,
) {
    {
        let mut guard = active.lock().await;
        guard.by_run.remove(&run_id);
        if let Some(set) = guard.by_user.get_mut(&user_id) {
            set.remove(&run_id);
            if set.is_empty() {
                guard.by_user.remove(&user_id);
            }
        }
    }
    let _ = done_tx.send(true);
}

struct ActiveRunMetricGuard {
    metrics: Arc<Metrics>,
}

impl ActiveRunMetricGuard {
    fn new(metrics: Arc<Metrics>) -> Self {
        metrics.inc_agent_runs_active();
        Self { metrics }
    }
}

impl Drop for ActiveRunMetricGuard {
    fn drop(&mut self) {
        self.metrics.dec_agent_runs_active();
    }
}

struct PostgresStepStore {
    store: Store,
    run_id: Uuid,
    user_id: Uuid,
}

impl StepStore for PostgresStepStore {
    fn append_step<'a>(&'a self, step: PersistedStep) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.store
                .append_agent_step(self.run_id, self.user_id, &step)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn list_steps<'a>(&'a self) -> BoxFuture<'a, Result<Vec<PersistedStep>, String>> {
        Box::pin(async move {
            self.store
                .list_agent_steps(self.run_id)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

struct ServerBranchSink {
    rooms: DocRooms,
    doc: Uuid,
    branch_id: Uuid,
    user_id: Uuid,
    epoch: u8,
    replica: u32,
    next_client_seq: u64,
    id_alloc: IdAllocator,
    next_sheet_counter: u8,
    engine: FormulaEngine,
    budget_lease: BudgetLease,
}

impl ServerBranchSink {
    #[expect(
        clippy::too_many_arguments,
        reason = "sink owns branch coordinates, replica counters, FormulaEngine, and memory budget lease"
    )]
    fn new(
        rooms: DocRooms,
        doc: Uuid,
        branch_id: Uuid,
        user_id: Uuid,
        epoch: u8,
        replica: u32,
        next_client_seq: u64,
        start_id_counter: u32,
        engine: FormulaEngine,
        budget_lease: BudgetLease,
    ) -> Option<Self> {
        let id_alloc = IdAllocator::new(epoch, replica, start_id_counter.max(1))?;
        Some(Self {
            rooms,
            doc,
            branch_id,
            user_id,
            epoch,
            replica,
            next_client_seq: next_client_seq.max(1),
            id_alloc,
            next_sheet_counter: 1,
            engine,
            budget_lease,
        })
    }

    fn build_wire_presence(&self, hint: AgentPresenceHint) -> Option<WirePresenceV1> {
        let sheet = self.engine.workbook().sheet(hint.sheet)?;
        let start_row = sheet.row_axis().pos_to_id(hint.row)?;
        let start_col = sheet.col_axis().pos_to_id(hint.col)?;
        let end_row = sheet
            .row_axis()
            .pos_to_id(hint.end_row)
            .unwrap_or(start_row);
        let end_col = sheet
            .col_axis()
            .pos_to_id(hint.end_col)
            .unwrap_or(start_col);
        let selection = if end_row != start_row || end_col != start_col {
            Some(WirePresenceRangeV1 {
                start_row,
                start_col,
                end_row,
                end_col,
            })
        } else {
            None
        };
        Some(WirePresenceV1 {
            replica: self.replica,
            user_id: self.user_id.into_bytes(),
            display_name: AI_DISPLAY_NAME.to_owned(),
            color_slot: AI_CURSOR_COLOR_SLOT,
            sheet_id: hint.sheet.0,
            cursor_row: start_row,
            cursor_col: start_col,
            selection,
        })
    }
}

impl AgentWorkbookSink for ServerBranchSink {
    fn engine(&self) -> &FormulaEngine {
        &self.engine
    }

    fn engine_mut(&mut self) -> &mut FormulaEngine {
        &mut self.engine
    }

    fn alloc_row_run(&mut self, len: u32) -> Result<IdRun, String> {
        self.id_alloc
            .alloc_run(len)
            .ok_or_else(|| "row ID space exhausted".to_owned())
    }

    fn alloc_col_run(&mut self, len: u32) -> Result<IdRun, String> {
        self.id_alloc
            .alloc_run(len)
            .ok_or_else(|| "column ID space exhausted".to_owned())
    }

    fn alloc_sheet_id(&mut self) -> Result<SheetId, String> {
        let mut c = self.next_sheet_counter.max(1);
        loop {
            let sid = pack_sheet_id(self.epoch, self.replica, c)
                .ok_or_else(|| "sheet ID space exhausted".to_owned())?;
            if self.engine.workbook().sheet(sid).is_none() {
                self.next_sheet_counter = c.saturating_add(1);
                return Ok(sid);
            }
            if c == u8::MAX {
                return Err("sheet ID space exhausted".to_owned());
            }
            c = c.saturating_add(1);
        }
    }

    fn commit_ops<'a>(
        &'a mut self,
        label: String,
        ops: Vec<Op<BoundFormula>>,
        presence: Option<AgentPresenceHint>,
    ) -> BoxFuture<'a, Result<u64, String>> {
        Box::pin(async move {
            let seq = self
                .rooms
                .server_commit(
                    self.doc,
                    self.branch_id,
                    self.user_id,
                    self.replica,
                    self.next_client_seq,
                    label,
                    ops.clone(),
                )
                .await
                .map_err(|e| format!("draft branch commit failed: {e}"))?;
            self.next_client_seq += 1;

            for op in ops {
                self.engine
                    .apply_op(op)
                    .map_err(|e| format!("local draft engine apply failed: {e:?}"))?;
            }
            self.engine.recalc_all();
            self.budget_lease.update_bytes(draft_engine_budget_bytes(
                self.engine.workbook().estimated_bytes() as u64,
            ));

            if let Some(hint) = presence
                && let Some(wire) = self.build_wire_presence(hint)
            {
                self.rooms
                    .broadcast_presence(self.doc, self.branch_id, wire)
                    .await;
            }
            Ok(seq)
        })
    }

    fn emit_presence<'a>(&'a mut self, presence: AgentPresenceHint) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(wire) = self.build_wire_presence(presence) {
                self.rooms
                    .broadcast_presence(self.doc, self.branch_id, wire)
                    .await;
            }
        })
    }
}

/// Estimates `2×` draft workbook RAM reserved against `DocRooms::max_loaded_bytes`
/// (`18-m4.md` §2.3: FormulaEngine dependency graph + evaluation caches).
pub fn draft_engine_budget_bytes(est_workbook_bytes: u64) -> u64 {
    est_workbook_bytes.max(64 * 1024).saturating_mul(2)
}

fn validate_prompt(raw: &str) -> Result<String, AgentStartError> {
    let trimmed = raw.trim();
    let count = trimmed.chars().count();
    if count == 0 || count > MAX_PROMPT_CHARS {
        return Err(AgentStartError::InvalidPrompt(format!(
            "prompt must contain between 1 and {MAX_PROMPT_CHARS} characters"
        )));
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(AgentStartError::InvalidPrompt(
            "prompt contains unsupported control characters".to_owned(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn summarize_branch_title(prompt: &str) -> String {
    let first_line = prompt.lines().next().unwrap_or(prompt).trim();
    let mut out = String::from("AI: ");
    for ch in first_line.chars().take(60) {
        if !ch.is_control() {
            out.push(ch);
        }
    }
    out
}
