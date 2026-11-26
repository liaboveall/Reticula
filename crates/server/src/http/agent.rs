//! Branch management, three-way diff/merge, and AI agent HTTP + SSE routes.

use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::CookieJar;
use futures_util::Stream;
use reticula_agent::diff::{
    BranchDiff, ClosureIssue, MergePreviewResponse, MergeSelection, preview_merge,
};
use reticula_sync::MemberRole;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::io::ReaderStream;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{ApiError, ApiState, auth, check_write_rate};
use crate::agent::AgentStartError;
use crate::docs::MergeCommitOutcome;
use crate::store::{
    AgentRunDetail, AgentRunRecord, BranchRecord, MAIN_BRANCH, User, WorkbookAccess,
};

/// Strict member-only check enforcing the 6-identity permission matrix:
/// - unauthenticated -> `401`
/// - anonymous share-link header without member session -> `403`
/// - signed-in non-member -> `403`
/// - signed-in member -> `Ok((User, WorkbookAccess))`
pub(super) async fn member_only(
    state: &ApiState,
    jar: &CookieJar,
    headers: &HeaderMap,
    doc: Uuid,
) -> Result<(User, WorkbookAccess), ApiError> {
    if let Some((user, _)) = auth::authenticate_optional(state, jar).await? {
        let access = state
            .store
            .workbook_access(doc, user.id)
            .await?
            .ok_or(ApiError::Forbidden)?;
        return Ok((user, access));
    }
    if headers
        .get("x-reticula-share")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.is_empty())
    {
        return Err(ApiError::Forbidden);
    }
    Err(ApiError::Unauthorized)
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateBranchRequest {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct StatusResult {
    pub status: String,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct MergeBranchResponse {
    pub merge_seq: String,
    pub committed_txns: u32,
    pub committed_ops: u32,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct MergeConflictBody {
    pub error: String,
    pub code: String,
    pub closure_issues: Vec<ClosureIssue>,
    pub unresolved_conflicts: Vec<String>,
    pub diff: BranchDiff,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct StartAgentRunRequest {
    pub prompt: String,
}

#[utoipa::path(get, path = "/api/workbooks/{id}/branches",
    params(("id" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 200, body = Vec<BranchRecord>), (status = 401), (status = 403)))]
pub(super) async fn list_branches(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<BranchRecord>>, ApiError> {
    let _ = member_only(&state, &jar, &headers, id).await?;
    Ok(Json(state.store.list_branches(id).await?))
}

#[utoipa::path(post, path = "/api/workbooks/{id}/branches",
    params(("id" = Uuid, Path)),
    request_body = CreateBranchRequest,
    security(("session" = [])),
    responses((status = 201, body = BranchRecord), (status = 400), (status = 401), (status = 403), (status = 422), (status = 429), (status = 507)))]
pub(super) async fn create_branch(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<CreateBranchRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let (user, access) = member_only(&state, &jar, &headers, id).await?;
    if !access.role.can_write() {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    let kind = input.kind.as_deref().unwrap_or("user").trim();
    if kind != "user" && kind != "ai_draft" {
        return Err(ApiError::Invalid(
            "branch kind must be `user` or `ai_draft`".into(),
        ));
    }
    let title = input.title.as_deref().unwrap_or("Draft branch").trim();
    if title.is_empty() || title.chars().count() > 120 || title.chars().any(char::is_control) {
        return Err(ApiError::Invalid(
            "branch title must contain 1 to 120 printable characters".into(),
        ));
    }
    let main_snap = state.rooms.snapshot_state(id, MAIN_BRANCH).await?;
    let branch = state
        .store
        .create_branch(
            id,
            user.id,
            kind,
            title,
            main_snap.head_seq,
            main_snap.workbook,
            main_snap.watermarks,
            main_snap.permutes,
            None,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(branch)))
}

#[utoipa::path(delete, path = "/api/workbooks/{id}/branches/{bid}",
    params(("id" = Uuid, Path), ("bid" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 200, body = StatusResult), (status = 400), (status = 401), (status = 403), (status = 404), (status = 429)))]
pub(super) async fn abandon_branch(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, bid)): Path<(Uuid, Uuid)>,
) -> Result<Json<StatusResult>, ApiError> {
    let (user, access) = member_only(&state, &jar, &headers, id).await?;
    if !access.role.can_write() {
        return Err(ApiError::Forbidden);
    }
    if bid == MAIN_BRANCH {
        return Err(ApiError::Invalid("cannot abandon the main branch".into()));
    }
    let branch = state
        .store
        .get_branch(id, bid)
        .await?
        .ok_or(ApiError::NotFound)?;
    if access.role != MemberRole::Owner && branch.created_by != user.id {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    state.agent.cancel_for_branch(id, bid).await;
    state.rooms.shutdown_branch_room(id, bid).await;
    state.store.abandon_branch(id, bid).await?;
    if branch.kind == "ai_draft" {
        state.metrics.record_agent_run_status("rejected");
    }
    Ok(Json(StatusResult {
        status: "abandoned".to_owned(),
    }))
}

#[utoipa::path(get, path = "/api/workbooks/{id}/branches/{bid}/snapshots/{seq}",
    params(("id" = Uuid, Path), ("bid" = Uuid, Path), ("seq" = String, Path)),
    security(("session" = [])),
    responses((status = 200, body = String, content_type = "application/octet-stream"), (status = 401), (status = 403), (status = 404)))]
pub(super) async fn branch_snapshot(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, bid, seq)): Path<(Uuid, Uuid, u64)>,
) -> Result<Response, ApiError> {
    let _ = member_only(&state, &jar, &headers, id).await?;
    let snapshot = state
        .store
        .snapshot_branch(id, bid, seq)
        .await?
        .ok_or(ApiError::NotFound)?;
    let length = snapshot.size();
    let root = state.store.data_dir().to_owned();
    let file = tokio::task::spawn_blocking(move || snapshot.open(&root))
        .await
        .map_err(|_| ApiError::Internal)??;
    let body = Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file)));
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, length.to_string()),
        ],
        body,
    )
        .into_response())
}

#[utoipa::path(get, path = "/api/workbooks/{id}/branches/{bid}/diff",
    params(("id" = Uuid, Path), ("bid" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 200, body = BranchDiff), (status = 400), (status = 401), (status = 403), (status = 404)))]
pub(super) async fn branch_diff(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, bid)): Path<(Uuid, Uuid)>,
) -> Result<Json<BranchDiff>, ApiError> {
    let _ = member_only(&state, &jar, &headers, id).await?;
    if bid == MAIN_BRANCH {
        return Err(ApiError::Invalid("diff requires a non-main branch".into()));
    }
    let branch = state
        .store
        .get_branch(id, bid)
        .await?
        .ok_or(ApiError::NotFound)?;
    let plan = state.agent.compute_diff_for_branch(id, &branch).await?;
    Ok(Json(plan.diff))
}

#[utoipa::path(post, path = "/api/workbooks/{id}/branches/{bid}/merge/preview",
    params(("id" = Uuid, Path), ("bid" = Uuid, Path)),
    request_body = MergeSelection,
    security(("session" = [])),
    responses((status = 200, body = MergePreviewResponse), (status = 400), (status = 401), (status = 403), (status = 404), (status = 413)))]
pub(super) async fn preview_branch_merge(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, bid)): Path<(Uuid, Uuid)>,
    Json(selection): Json<MergeSelection>,
) -> Result<Json<MergePreviewResponse>, ApiError> {
    let (_, access) = member_only(&state, &jar, &headers, id).await?;
    if !access.role.can_write() {
        return Err(ApiError::Forbidden);
    }
    if bid == MAIN_BRANCH {
        return Err(ApiError::Invalid("cannot merge main into itself".into()));
    }
    let branch = state
        .store
        .get_branch(id, bid)
        .await?
        .ok_or(ApiError::NotFound)?;
    let plan = state.agent.compute_diff_for_branch(id, &branch).await?;
    Ok(Json(preview_merge(&plan, &selection)))
}

#[utoipa::path(post, path = "/api/workbooks/{id}/branches/{bid}/merge",
    params(("id" = Uuid, Path), ("bid" = Uuid, Path)),
    request_body = MergeSelection,
    security(("session" = [])),
    responses(
        (status = 200, body = MergeBranchResponse),
        (status = 400),
        (status = 401),
        (status = 403),
        (status = 404),
        (status = 409, body = MergeConflictBody),
        (status = 413),
        (status = 429),
        (status = 507),
    ))]
pub(super) async fn merge_branch(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, bid)): Path<(Uuid, Uuid)>,
    Json(selection): Json<MergeSelection>,
) -> Result<Json<MergeBranchResponse>, ApiError> {
    let (user, access) = member_only(&state, &jar, &headers, id).await?;
    if !access.role.can_write() {
        return Err(ApiError::Forbidden);
    }
    if bid == MAIN_BRANCH {
        return Err(ApiError::Invalid("cannot merge main into itself".into()));
    }
    check_write_rate(&state, user.id)?;
    let branch = state
        .store
        .get_branch(id, bid)
        .await?
        .ok_or(ApiError::NotFound)?;
    if branch.status != "active" {
        return Err(ApiError::Conflict(format!(
            "branch is already {}",
            branch.status
        )));
    }
    if state.agent.has_active_run_for_branch(id, bid).await {
        return Err(ApiError::Conflict(
            "cannot merge branch while an AI analyst run is still in progress".to_owned(),
        ));
    }
    let fork_seq: u64 = branch.fork_seq.parse().map_err(|_| ApiError::Internal)?;
    let base = state.store.load_workbook_at_seq(id, bid, 0).await?;
    let draft_snap = state.rooms.snapshot_state(id, bid).await?;
    let draft_txns = state.store.load_branch_txns(id, bid).await?;
    let label = if branch.title.is_empty() {
        "Merge AI draft".to_owned()
    } else {
        format!("Merge: {}", branch.title)
    };

    let outcome = state
        .rooms
        .apply_merge(
            id,
            bid,
            user.id,
            access.role,
            base,
            draft_snap.workbook,
            fork_seq,
            draft_txns,
            selection,
            label,
        )
        .await?;

    match outcome {
        MergeCommitOutcome::Merged {
            merge_seq,
            committed_txns,
            committed_ops,
        } => {
            // The draft was marked merged together with the changes it put on `main`.
            state.rooms.shutdown_branch_room(id, bid).await;
            if branch.kind == "ai_draft" {
                state.metrics.record_agent_run_status("merged");
            }
            Ok(Json(MergeBranchResponse {
                merge_seq: merge_seq.to_string(),
                committed_txns: u32::try_from(committed_txns).unwrap_or(u32::MAX),
                committed_ops: u32::try_from(committed_ops).unwrap_or(u32::MAX),
            }))
        }
        MergeCommitOutcome::Conflict {
            code,
            message,
            diff,
            closure_issues,
            unresolved_conflicts,
        } => {
            let body = MergeConflictBody {
                error: message,
                code: code.to_owned(),
                closure_issues,
                unresolved_conflicts,
                diff,
            };
            Err(ApiError::ConflictJson(
                serde_json::to_value(body).unwrap_or_else(|_| json!({ "error": "merge conflict" })),
            ))
        }
        MergeCommitOutcome::DiskFull => {
            Err(ApiError::DiskFull("server storage capacity reached".into()))
        }
        MergeCommitOutcome::Rejected(reason) => Err(ApiError::Unprocessable(format!(
            "merge rejected: {reason:?}"
        ))),
        MergeCommitOutcome::RemapFailed(msg) => Err(ApiError::Unprocessable(msg)),
        MergeCommitOutcome::Storage(err) => Err(err.into()),
    }
}

#[utoipa::path(get, path = "/api/workbooks/{id}/agent/runs",
    params(("id" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 200, body = Vec<AgentRunRecord>), (status = 401), (status = 403)))]
pub(super) async fn list_agent_runs(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<AgentRunRecord>>, ApiError> {
    let _ = member_only(&state, &jar, &headers, id).await?;
    Ok(Json(state.store.list_agent_runs(id).await?))
}

#[utoipa::path(post, path = "/api/workbooks/{id}/agent/runs",
    params(("id" = Uuid, Path)),
    request_body = StartAgentRunRequest,
    security(("session" = [])),
    responses(
        (status = 201, body = AgentRunRecord),
        (status = 400),
        (status = 401),
        (status = 403),
        (status = 409),
        (status = 429),
        (status = 503),
        (status = 507),
    ))]
pub(super) async fn start_agent_run(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<StartAgentRunRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let (user, access) = member_only(&state, &jar, &headers, id).await?;
    if !access.role.can_write() {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    let run = state
        .agent
        .start_run(id, user.id, &input.prompt)
        .await
        .map_err(|err| match err {
            AgentStartError::NotConfigured => {
                ApiError::Unavailable("AI analyst is not configured on this server".into())
            }
            AgentStartError::InvalidPrompt(msg) => ApiError::Invalid(msg),
            AgentStartError::UserAlreadyRunning => ApiError::Conflict(err.to_string()),
            AgentStartError::Busy => ApiError::Busy,
            AgentStartError::Store(e) => e.into(),
        })?;
    Ok((StatusCode::CREATED, Json(run)))
}

#[utoipa::path(get, path = "/api/workbooks/{id}/agent/runs/{rid}",
    params(("id" = Uuid, Path), ("rid" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 200, body = AgentRunDetail), (status = 401), (status = 403), (status = 404)))]
pub(super) async fn get_agent_run(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, rid)): Path<(Uuid, Uuid)>,
) -> Result<Json<AgentRunDetail>, ApiError> {
    let _ = member_only(&state, &jar, &headers, id).await?;
    let run = state
        .store
        .get_agent_run(id, rid)
        .await?
        .ok_or(ApiError::NotFound)?;
    let steps = state.store.list_agent_steps(rid).await?;
    Ok(Json(AgentRunDetail { run, steps }))
}

#[utoipa::path(post, path = "/api/workbooks/{id}/agent/runs/{rid}/cancel",
    params(("id" = Uuid, Path), ("rid" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 200, body = StatusResult), (status = 401), (status = 403), (status = 404), (status = 429)))]
pub(super) async fn cancel_agent_run(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, rid)): Path<(Uuid, Uuid)>,
) -> Result<Json<StatusResult>, ApiError> {
    let (user, access) = member_only(&state, &jar, &headers, id).await?;
    if !access.role.can_write() {
        return Err(ApiError::Forbidden);
    }
    let run = state
        .store
        .get_agent_run(id, rid)
        .await?
        .ok_or(ApiError::NotFound)?;
    if access.role != MemberRole::Owner && run.user_id != user.id {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    let signalled = state.agent.cancel_run(rid).await;
    if !signalled && matches!(run.status.as_str(), "queued" | "running") {
        state
            .store
            .finish_agent_run(rid, "cancelled", "", Some("cancelled"))
            .await?;
        state.metrics.record_agent_run_status("cancelled");
    }
    Ok(Json(StatusResult {
        status: "cancelled".to_owned(),
    }))
}

#[utoipa::path(get, path = "/api/workbooks/{id}/agent/runs/{rid}/events",
    params(("id" = Uuid, Path), ("rid" = Uuid, Path)),
    security(("session" = [])),
    responses(
        (status = 200, description = "Server-Sent Events stream of agent steps and status", content_type = "text/event-stream"),
        (status = 401),
        (status = 403),
        (status = 404),
    ))]
pub(super) async fn agent_run_events(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, rid)): Path<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    let _ = member_only(&state, &jar, &headers, id).await?;
    let last_event_id: u64 = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    // Subscribe BEFORE reading persisted steps so live events are never missed in the gap.
    let live_rx = state.agent.subscribe_events(rid).await;
    let run = state
        .store
        .get_agent_run(id, rid)
        .await?
        .ok_or(ApiError::NotFound)?;
    let history = state.store.list_agent_steps(rid).await?;

    let stream = build_sse_stream(run, history, last_event_id, live_rx);
    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    );
    let mut resp = sse.into_response();
    resp.headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(resp)
}

fn build_sse_stream(
    run: AgentRunRecord,
    history: Vec<reticula_agent::runner::PersistedStep>,
    last_event_id: u64,
    live_rx: Option<tokio::sync::broadcast::Receiver<crate::agent::SseMessage>>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(async move {
        let mut max_step_id = last_event_id;
        for step in history {
            let step_id = u64::try_from(step.idx.max(0)).unwrap_or(0) + 1;
            if step_id <= last_event_id {
                continue;
            }
            max_step_id = max_step_id.max(step_id);
            if let Ok(ev) = Event::default()
                .id(step_id.to_string())
                .event("step")
                .json_data(&step)
                && tx.send(Ok(ev)).await.is_err()
            {
                return;
            }
        }

        let terminal_or_review = matches!(
            run.status.as_str(),
            "waiting_review" | "merged" | "rejected" | "failed" | "cancelled"
        );
        if terminal_or_review || live_rx.is_none() {
            let payload = json!({
                "status": run.status,
                "summary": run.summary,
                "error": run.error_message,
            });
            if let Ok(ev) = Event::default().event("status").json_data(payload) {
                let _ = tx.send(Ok(ev)).await;
            }
            return;
        }

        if let Some(mut rx_live) = live_rx {
            loop {
                match rx_live.recv().await {
                    Ok(msg) => {
                        if let Some(step_id) = msg.id {
                            if msg.event == "step" && step_id <= max_step_id {
                                continue;
                            }
                            if msg.event == "step" {
                                max_step_id = max_step_id.max(step_id);
                            }
                        }
                        let is_status = msg.event == "status";
                        let mut builder = Event::default().event(msg.event);
                        if let Some(step_id) = msg.id {
                            builder = builder.id(step_id.to_string());
                        }
                        if let Ok(ev) = builder.json_data(&msg.data)
                            && tx.send(Ok(ev)).await.is_err()
                        {
                            return;
                        }
                        if is_status {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}
