//! Same-origin collaboration API and its OpenAPI contract.

mod auth;
mod sync;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::CookieJar;
use reticula_sync::MemberRole;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::io::ReaderStream;
use utoipa::{Modify, OpenApi, ToSchema};
use uuid::Uuid;

use crate::docs::{DocRooms, RoomError};
use crate::ops::{AnonRateLimitLayer, GovernanceConfig, Metrics, RateLimiter};
use crate::store::{
    AccountSummary, AnonymousShareAccess, HistoryPage, MAIN_BRANCH, MemberEntry, ShareLinkRecord,
    Store, StoreError, User, VersionEntry, WorkbookAccess, WorkbookLists, WorkbookVisibility,
};

pub use auth::AuthConfig;

#[derive(Clone)]
pub struct ApiState {
    store: Store,
    auth: Arc<AuthConfig>,
    rooms: DocRooms,
    metrics: Arc<Metrics>,
    limiter: Arc<RateLimiter>,
    revocations: broadcast::Sender<[u8; 32]>,
    reports: Arc<Mutex<ReportWindow>>,
}

struct ReportWindow {
    start: Instant,
    count: u16,
}

impl ApiState {
    pub fn new(store: Store, auth: AuthConfig) -> Self {
        Self::with_governance(store, auth, GovernanceConfig::default())
    }

    pub fn with_governance(store: Store, auth: AuthConfig, gov: GovernanceConfig) -> Self {
        let metrics = Arc::new(Metrics::default());
        let limiter = Arc::new(RateLimiter::new(gov, Arc::clone(&metrics)));
        let rooms =
            DocRooms::with_governance(store.clone(), gov.max_loaded_bytes, Arc::clone(&metrics));
        Self {
            store,
            auth: Arc::new(auth),
            rooms,
            metrics,
            limiter,
            revocations: broadcast::channel(128).0,
            reports: Arc::new(Mutex::new(ReportWindow {
                start: Instant::now(),
                count: 0,
            })),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    pub fn render_metrics(&self) -> String {
        let used = self.store.disk_used_bytes();
        let over_soft = self.store.disk_over_soft_limit();
        self.limiter
            .note_disk_soft_state(over_soft, used, self.store.limits().disk_soft_bytes);
        self.metrics.render_prometheus(used, over_soft)
    }
}

pub fn router(state: ApiState) -> Router {
    let import_limit = state.store.limits().max_import_bytes;
    let mut standard = Router::new()
        .route("/auth/github/login", get(auth::github_login))
        .route("/auth/github/callback", get(auth::github_callback))
        .route("/auth/me", get(auth::me))
        .route("/auth/logout", post(auth::logout))
        .route("/workbooks", get(list_workbooks).post(create_workbook))
        .route(
            "/workbooks/{id}",
            get(workbook).patch(update_workbook).delete(delete_workbook),
        )
        .route("/workbooks/{id}/members", get(list_members))
        .route(
            "/workbooks/{id}/members/{user_id}",
            patch(update_member).delete(remove_member),
        )
        .route(
            "/workbooks/{id}/share-links",
            get(list_share_links).post(share),
        )
        .route(
            "/workbooks/{id}/share-links/{link_id}",
            delete(revoke_share_link),
        )
        .route("/workbooks/{id}/history", get(history))
        .route("/workbooks/{id}/txns", get(txns))
        .route(
            "/workbooks/{id}/versions",
            get(list_versions).post(create_version),
        )
        .route(
            "/workbooks/{id}/versions/{version_id}",
            delete(delete_version),
        )
        .route("/join/{token}", post(join))
        .route("/workbooks/{id}/snapshots/{seq}", get(snapshot))
        .route("/account", get(account).delete(delete_account))
        .route("/sync/{id}", get(sync::upgrade))
        .route("/client-errors", post(report))
        .route("/openapi.json", get(|| async { Json(openapi()) }));
    if state.auth.is_dev_mock() {
        standard = standard.route("/auth/dev-login", post(auth::dev_login));
    }
    let standard = standard.layer(DefaultBodyLimit::max(16 * 1024));

    let import_routes = Router::new()
        .route("/workbooks/import", post(import_workbook))
        .layer(DefaultBodyLimit::max(import_limit));

    standard
        .merge(import_routes)
        .fallback(|| async { ApiError::NotFound })
        .layer(middleware::from_fn_with_state(
            state.clone(),
            same_origin_mutations,
        ))
        .layer(AnonRateLimitLayer::new(Arc::clone(&state.limiter)))
        .with_state(state)
}

fn render_metrics_response(api: Option<ApiState>) -> Response {
    let Some(api) = api else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "collaboration storage unavailable\n",
        )
            .into_response();
    };
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-store"),
            ),
        ],
        api.render_metrics(),
    )
        .into_response()
}

pub fn metrics_route(api: Option<ApiState>) -> Router {
    Router::new().route(
        "/metrics",
        get(move |req: Request| {
            let api = api.clone();
            async move {
                let loopback = req
                    .extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .is_some_and(|ConnectInfo(addr)| addr.ip().is_loopback());
                if !loopback {
                    return (StatusCode::FORBIDDEN, "forbidden\n").into_response();
                }
                render_metrics_response(api)
            }
        }),
    )
}

/// Metrics-only router for an internal listener mapped to host loopback.
pub fn internal_metrics_route(api: Option<ApiState>) -> Router {
    Router::new().route(
        "/metrics",
        get(move || {
            let api = api.clone();
            async move { render_metrics_response(api) }
        }),
    )
}

async fn same_origin_mutations(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    let mutates = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let mut response = if mutates && state.auth.check_origin(request.headers()).is_err() {
        ApiError::Forbidden.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

#[derive(Debug)]
enum ApiError {
    Unauthorized,
    Forbidden,
    NotFound,
    Invalid(String),
    Unprocessable(String),
    Conflict(String),
    Quota(String),
    DiskFull(String),
    RateLimited(u64),
    Busy,
    Provider,
    Internal,
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound => Self::NotFound,
            StoreError::Forbidden => Self::Forbidden,
            StoreError::InvalidInput(msg) => Self::Unprocessable(msg),
            StoreError::Conflict(msg) => Self::Conflict(msg),
            StoreError::QuotaExceeded(msg) => Self::Quota(msg),
            StoreError::DiskFull(msg) => Self::DiskFull(msg),
            other => {
                tracing::error!(error = %other, "collaboration storage failure");
                Self::Internal
            }
        }
    }
}

impl From<RoomError> for ApiError {
    fn from(error: RoomError) -> Self {
        match error {
            RoomError::Busy => Self::Busy,
            RoomError::Unavailable => Self::NotFound,
            RoomError::AccessRevoked | RoomError::Rejected(_) => Self::Forbidden,
        }
    }
}

#[derive(Serialize, ToSchema)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let retry_after = match self {
            Self::RateLimited(secs) => Some(secs),
            _ => None,
        };
        let (status, error) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "authentication required".into()),
            Self::Forbidden => (StatusCode::FORBIDDEN, "access denied".into()),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            Self::Invalid(reason) => (StatusCode::BAD_REQUEST, reason),
            Self::Unprocessable(reason) => (StatusCode::UNPROCESSABLE_ENTITY, reason),
            Self::Conflict(reason) => (StatusCode::CONFLICT, reason),
            Self::Quota(reason) => (StatusCode::FORBIDDEN, reason),
            Self::DiskFull(reason) => (StatusCode::INSUFFICIENT_STORAGE, reason),
            Self::RateLimited(_) => (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded".into()),
            Self::Busy => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server document budget busy".into(),
            ),
            Self::Provider => (
                StatusCode::BAD_GATEWAY,
                "GitHub authentication failed".into(),
            ),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error".into(),
            ),
        };
        let mut resp = (status, Json(ErrorBody { error })).into_response();
        if let Some(secs) = retry_after
            && let Ok(val) = HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, val);
        }
        resp
    }
}

fn check_write_rate(state: &ApiState, user_id: Uuid) -> Result<(), ApiError> {
    state
        .limiter
        .check_user_write(user_id)
        .map_err(ApiError::RateLimited)
}

async fn member(
    state: &ApiState,
    jar: &CookieJar,
    id: Uuid,
) -> Result<(User, WorkbookAccess), ApiError> {
    let user = auth::authenticate(state, jar).await?;
    let book = state
        .store
        .workbook_access(id, user.id)
        .await?
        .ok_or(ApiError::Forbidden)?;
    Ok((user, book))
}

enum ReadPrincipal {
    Member(WorkbookAccess),
    Anonymous(AnonymousShareAccess),
}

async fn read_access(
    state: &ApiState,
    jar: &CookieJar,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    doc: Uuid,
) -> Result<ReadPrincipal, ApiError> {
    let authed = auth::authenticate_optional(state, jar).await?;
    if let Some((user, _)) = &authed
        && let Some(book) = state.store.workbook_access(doc, user.id).await?
    {
        return Ok(ReadPrincipal::Member(book));
    }
    if let Some(token) = headers
        .get("x-reticula-share")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        if authed.is_none() && crate::ops::has_session_cookie(headers) {
            let ip = crate::ops::client_ip_parts(peer, headers);
            state
                .limiter
                .check_anon_ip(ip)
                .map_err(ApiError::RateLimited)?;
        }
        let hash = auth::token_hash(token).ok_or(ApiError::Forbidden)?;
        let anon = state
            .store
            .resolve_anonymous_share(doc, &hash)
            .await?
            .ok_or(ApiError::Forbidden)?;
        return Ok(ReadPrincipal::Anonymous(anon));
    }
    if authed.is_some() {
        Err(ApiError::Forbidden)
    } else {
        Err(ApiError::Unauthorized)
    }
}

#[derive(Deserialize, ToSchema)]
struct CreateWorkbook {
    title: String,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct WorkbookKey {
    id: Uuid,
    branch_id: Uuid,
}

#[derive(Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum Role {
    Owner,
    Editor,
    Viewer,
}

impl From<MemberRole> for Role {
    fn from(role: MemberRole) -> Self {
        match role {
            MemberRole::Owner => Self::Owner,
            MemberRole::Editor => Self::Editor,
            MemberRole::Viewer => Self::Viewer,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum ShareRole {
    Editor,
    Viewer,
}

impl From<ShareRole> for MemberRole {
    fn from(role: ShareRole) -> Self {
        match role {
            ShareRole::Editor => Self::Editor,
            ShareRole::Viewer => Self::Viewer,
        }
    }
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct WorkbookSummary {
    id: Uuid,
    branch_id: Uuid,
    title: String,
    visibility: WorkbookVisibility,
    epoch: u8,
    head_seq: String,
    role: Role,
    members: BTreeMap<String, u64>,
}

#[utoipa::path(get, path = "/api/workbooks", security(("session" = [])),
    responses((status = 200, body = WorkbookLists), (status = 401)))]
async fn list_workbooks(
    State(state): State<ApiState>,
    jar: CookieJar,
) -> Result<Json<WorkbookLists>, ApiError> {
    let user = auth::authenticate(&state, &jar).await?;
    Ok(Json(state.store.list_workbooks(user.id).await?))
}

#[utoipa::path(post, path = "/api/workbooks", request_body = CreateWorkbook, security(("session" = [])),
    responses((status = 201, body = WorkbookKey), (status = 400, body = ErrorBody), (status = 401), (status = 403), (status = 429), (status = 507)))]
async fn create_workbook(
    State(state): State<ApiState>,
    jar: CookieJar,
    Json(input): Json<CreateWorkbook>,
) -> Result<impl IntoResponse, ApiError> {
    let user = auth::authenticate(&state, &jar).await?;
    check_write_rate(&state, user.id)?;
    let title = input.title.trim();
    if title.is_empty() || title.chars().count() > 120 || title.chars().any(char::is_control) {
        return Err(ApiError::Invalid(
            "title must contain 1 to 120 printable characters".into(),
        ));
    }
    let id = state.store.create_workbook(user.id, title).await?;
    Ok((
        StatusCode::CREATED,
        Json(WorkbookKey {
            id,
            branch_id: MAIN_BRANCH,
        }),
    ))
}

#[utoipa::path(post, path = "/api/workbooks/import",
    params(("X-Workbook-Title" = String, Header, description = "Percent-encoded UTF-8 workbook title")),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    security(("session" = [])),
    responses((status = 201, body = WorkbookKey), (status = 400, body = ErrorBody), (status = 401), (status = 403), (status = 413), (status = 422), (status = 429), (status = 507)))]
async fn import_workbook(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let user = auth::authenticate(&state, &jar).await?;
    check_write_rate(&state, user.id)?;
    let encoded_title = headers
        .get("x-workbook-title")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::Invalid("X-Workbook-Title header is required".into()))?;
    let title = percent_decode_utf8(encoded_title)
        .ok_or_else(|| ApiError::Invalid("X-Workbook-Title is not valid UTF-8".into()))?;
    let id = state
        .store
        .import_workbook(user.id, &title, body.to_vec())
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(WorkbookKey {
            id,
            branch_id: MAIN_BRANCH,
        }),
    ))
}

#[utoipa::path(get, path = "/api/workbooks/{id}",
    params(("id" = Uuid, Path), ("X-Reticula-Share" = Option<String>, Header)),
    security(("session" = [])),
    responses((status = 200, body = WorkbookSummary), (status = 401), (status = 403)))]
async fn workbook(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Path(id): Path<Uuid>,
) -> Result<Json<WorkbookSummary>, ApiError> {
    match read_access(&state, &jar, &headers, peer.map(|Extension(c)| c.0), id).await? {
        ReadPrincipal::Member(book) => {
            let members = state.store.workbook_members(id).await?;
            Ok(Json(WorkbookSummary {
                id: book.id,
                branch_id: MAIN_BRANCH,
                title: book.title,
                visibility: book.visibility,
                epoch: book.epoch,
                head_seq: book.head.to_string(),
                role: book.role.into(),
                members,
            }))
        }
        ReadPrincipal::Anonymous(anon) => Ok(Json(WorkbookSummary {
            id: anon.id,
            branch_id: MAIN_BRANCH,
            title: anon.title,
            visibility: WorkbookVisibility::LinkView,
            epoch: anon.epoch,
            head_seq: anon.head.to_string(),
            role: Role::Viewer,
            members: BTreeMap::new(),
        })),
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct UpdateWorkbook {
    title: Option<String>,
    visibility: Option<WorkbookVisibility>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct UpdatedWorkbook {
    id: Uuid,
    title: String,
    visibility: WorkbookVisibility,
}

#[utoipa::path(patch, path = "/api/workbooks/{id}", params(("id" = Uuid, Path)), request_body = UpdateWorkbook,
    security(("session" = [])),
    responses((status = 200, body = UpdatedWorkbook), (status = 400), (status = 401), (status = 403), (status = 422), (status = 429)))]
async fn update_workbook(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path(id): Path<Uuid>,
    Json(input): Json<UpdateWorkbook>,
) -> Result<Json<UpdatedWorkbook>, ApiError> {
    let (user, access) = member(&state, &jar, id).await?;
    if access.role != MemberRole::Owner {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    let (title, visibility) = state
        .store
        .update_workbook(id, input.title, input.visibility)
        .await?;
    if visibility == WorkbookVisibility::Private {
        state.rooms.kick_all_anonymous(id).await;
    }
    Ok(Json(UpdatedWorkbook {
        id,
        title,
        visibility,
    }))
}

#[utoipa::path(delete, path = "/api/workbooks/{id}", params(("id" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 204), (status = 401), (status = 403), (status = 404), (status = 429)))]
async fn delete_workbook(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let (user, access) = member(&state, &jar, id).await?;
    if access.role != MemberRole::Owner {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    state.rooms.shutdown_room(id).await;
    state.store.delete_workbook(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/api/workbooks/{id}/members", params(("id" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 200, body = Vec<MemberEntry>), (status = 401), (status = 403)))]
async fn list_members(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<MemberEntry>>, ApiError> {
    member(&state, &jar, id).await?;
    Ok(Json(state.store.list_members(id).await?))
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct UpdateMemberRole {
    role: ShareRole,
}

#[utoipa::path(patch, path = "/api/workbooks/{id}/members/{user_id}",
    params(("id" = Uuid, Path), ("user_id" = Uuid, Path)),
    request_body = UpdateMemberRole,
    security(("session" = [])),
    responses((status = 204), (status = 400), (status = 401), (status = 403), (status = 404), (status = 422), (status = 429)))]
async fn update_member(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path((id, target_user_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<UpdateMemberRole>,
) -> Result<StatusCode, ApiError> {
    let (user, access) = member(&state, &jar, id).await?;
    if access.role != MemberRole::Owner {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    let role: MemberRole = input.role.into();
    state
        .store
        .update_member_role(id, target_user_id, role)
        .await?;
    state
        .rooms
        .update_member_role(id, target_user_id, Some(role))
        .await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(delete, path = "/api/workbooks/{id}/members/{user_id}",
    params(("id" = Uuid, Path), ("user_id" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 204), (status = 401), (status = 403), (status = 404), (status = 422), (status = 429)))]
async fn remove_member(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path((id, target_user_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let (user, access) = member(&state, &jar, id).await?;
    if access.role != MemberRole::Owner && user.id != target_user_id {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    state.store.remove_member(id, target_user_id).await?;
    state
        .rooms
        .update_member_role(id, target_user_id, None)
        .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateShareLink {
    #[serde(default)]
    role: Option<ShareRole>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    single_use: Option<bool>,
    ttl_seconds: Option<u32>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct ShareLink {
    id: Uuid,
    url: String,
    role: ShareRole,
}

#[utoipa::path(get, path = "/api/workbooks/{id}/share-links", params(("id" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 200, body = Vec<ShareLinkRecord>), (status = 401), (status = 403)))]
async fn list_share_links(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ShareLinkRecord>>, ApiError> {
    let (_, access) = member(&state, &jar, id).await?;
    if access.role != MemberRole::Owner {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(state.store.list_share_links(id).await?))
}

#[utoipa::path(post, path = "/api/workbooks/{id}/share-links", params(("id" = Uuid, Path)), request_body = CreateShareLink,
    security(("session" = [])), responses((status = 201, body = ShareLink), (status = 400), (status = 401), (status = 403), (status = 422), (status = 429)))]
async fn share(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path(id): Path<Uuid>,
    Json(input): Json<CreateShareLink>,
) -> Result<impl IntoResponse, ApiError> {
    let (user, access) = member(&state, &jar, id).await?;
    if access.role != MemberRole::Owner {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    if input
        .ttl_seconds
        .is_some_and(|ttl| !(1..=31_536_000).contains(&ttl))
    {
        return Err(ApiError::Invalid(
            "link lifetime must be between 1 second and 1 year".into(),
        ));
    }
    let share_role = input.role.unwrap_or(ShareRole::Editor);
    let token = auth::encode_secret(&auth::random_secret()?);
    let hash = auth::token_hash(&token).ok_or(ApiError::Internal)?;
    let link_id = state
        .store
        .create_share_link_with_options(
            id,
            user.id,
            &hash,
            share_role.into(),
            input.label.as_deref().unwrap_or(""),
            input.single_use.unwrap_or(false),
            input.ttl_seconds,
        )
        .await?
        .ok_or(ApiError::Forbidden)?;
    let url = match share_role {
        ShareRole::Editor => format!("/join/{token}"),
        ShareRole::Viewer => format!("/app?collab={id}&share={token}"),
    };
    Ok((
        StatusCode::CREATED,
        Json(ShareLink {
            id: link_id,
            url,
            role: share_role,
        }),
    ))
}

#[utoipa::path(delete, path = "/api/workbooks/{id}/share-links/{link_id}",
    params(("id" = Uuid, Path), ("link_id" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 204), (status = 401), (status = 403), (status = 404), (status = 429)))]
async fn revoke_share_link(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path((id, link_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let (user, access) = member(&state, &jar, id).await?;
    if access.role != MemberRole::Owner {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    if !state.store.revoke_share_link(id, link_id).await? {
        return Err(ApiError::NotFound);
    }
    state.rooms.revoke_share_link(id, link_id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(post, path = "/api/join/{token}", params(("token" = String, Path)), security(("session" = [])),
    responses((status = 200, body = WorkbookKey), (status = 401), (status = 403), (status = 429)))]
async fn join(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path(token): Path<String>,
) -> Result<Json<WorkbookKey>, ApiError> {
    let user = auth::authenticate(&state, &jar).await?;
    check_write_rate(&state, user.id)?;
    let hash = auth::token_hash(&token).ok_or(ApiError::Forbidden)?;
    let id = state
        .store
        .join_via_link(user.id, &hash)
        .await?
        .ok_or(ApiError::Forbidden)?;
    Ok(Json(WorkbookKey {
        id,
        branch_id: MAIN_BRANCH,
    }))
}

#[utoipa::path(get, path = "/api/workbooks/{id}/snapshots/{seq}",
    params(("id" = Uuid, Path), ("seq" = String, Path), ("X-Reticula-Share" = Option<String>, Header)),
    security(("session" = [])), responses((status = 200, body = String, content_type = "application/octet-stream"), (status = 401), (status = 403), (status = 404)))]
async fn snapshot(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Path((id, seq)): Path<(Uuid, u64)>,
) -> Result<Response, ApiError> {
    let _ = read_access(&state, &jar, &headers, peer.map(|Extension(c)| c.0), id).await?;
    let snapshot = state
        .store
        .snapshot(id, seq)
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

#[derive(Deserialize)]
struct HistoryQuery {
    before_seq: Option<u64>,
    limit: Option<u32>,
}

#[utoipa::path(get, path = "/api/workbooks/{id}/history",
    params(
        ("id" = Uuid, Path),
        ("before_seq" = Option<String>, Query),
        ("limit" = Option<u32>, Query),
        ("X-Reticula-Share" = Option<String>, Header),
    ),
    security(("session" = [])),
    responses((status = 200, body = HistoryPage), (status = 401), (status = 403)))]
async fn history(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Path(id): Path<Uuid>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<HistoryPage>, ApiError> {
    let _ = read_access(&state, &jar, &headers, peer.map(|Extension(c)| c.0), id).await?;
    Ok(Json(
        state
            .store
            .list_history(id, query.before_seq, query.limit.unwrap_or(100))
            .await?,
    ))
}

#[derive(Deserialize)]
struct TxnsQuery {
    after_seq: Option<u64>,
    limit: Option<u32>,
}

#[utoipa::path(get, path = "/api/workbooks/{id}/txns",
    params(
        ("id" = Uuid, Path),
        ("after_seq" = Option<String>, Query),
        ("limit" = Option<u32>, Query),
        ("X-Reticula-Share" = Option<String>, Header),
    ),
    security(("session" = [])),
    responses((status = 200, body = String, content_type = "application/octet-stream"), (status = 401), (status = 403)))]
async fn txns(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Path(id): Path<Uuid>,
    Query(query): Query<TxnsQuery>,
) -> Result<Response, ApiError> {
    let _ = read_access(&state, &jar, &headers, peer.map(|Extension(c)| c.0), id).await?;
    let payload = state
        .store
        .fetch_txns_after(id, query.after_seq.unwrap_or(0), query.limit.unwrap_or(500))
        .await?;
    let length = payload.len();
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, length.to_string()),
        ],
        payload,
    )
        .into_response())
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateVersion {
    seq: String,
    name: String,
}

#[utoipa::path(get, path = "/api/workbooks/{id}/versions",
    params(("id" = Uuid, Path), ("X-Reticula-Share" = Option<String>, Header)),
    security(("session" = [])),
    responses((status = 200, body = Vec<VersionEntry>), (status = 401), (status = 403)))]
async fn list_versions(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<VersionEntry>>, ApiError> {
    let _ = read_access(&state, &jar, &headers, peer.map(|Extension(c)| c.0), id).await?;
    Ok(Json(state.store.list_versions(id).await?))
}

#[utoipa::path(post, path = "/api/workbooks/{id}/versions",
    params(("id" = Uuid, Path)),
    request_body = CreateVersion,
    security(("session" = [])),
    responses((status = 201, body = VersionEntry), (status = 400), (status = 401), (status = 403), (status = 409), (status = 422), (status = 429)))]
async fn create_version(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path(id): Path<Uuid>,
    Json(input): Json<CreateVersion>,
) -> Result<impl IntoResponse, ApiError> {
    let (user, access) = member(&state, &jar, id).await?;
    if !access.role.can_write() {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    let seq: u64 = input
        .seq
        .parse()
        .map_err(|_| ApiError::Invalid("invalid version sequence".into()))?;
    let created = state
        .store
        .create_version(id, user.id, seq, &input.name)
        .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

#[utoipa::path(delete, path = "/api/workbooks/{id}/versions/{version_id}",
    params(("id" = Uuid, Path), ("version_id" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 204), (status = 401), (status = 403), (status = 404), (status = 429)))]
async fn delete_version(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path((id, version_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let (user, access) = member(&state, &jar, id).await?;
    if !access.role.can_write() {
        return Err(ApiError::Forbidden);
    }
    check_write_rate(&state, user.id)?;
    if !state.store.delete_version(id, version_id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/api/account", security(("session" = [])),
    responses((status = 200, body = AccountSummary), (status = 401)))]
async fn account(
    State(state): State<ApiState>,
    jar: CookieJar,
) -> Result<Json<AccountSummary>, ApiError> {
    let user = auth::authenticate(&state, &jar).await?;
    Ok(Json(state.store.account_summary(user.id).await?))
}

#[derive(Deserialize, ToSchema)]
struct DeleteAccount {
    confirm: String,
}

#[utoipa::path(delete, path = "/api/account", request_body = DeleteAccount, security(("session" = [])),
    responses((status = 204), (status = 400), (status = 401), (status = 429)))]
async fn delete_account(
    State(state): State<ApiState>,
    jar: CookieJar,
    Json(input): Json<DeleteAccount>,
) -> Result<impl IntoResponse, ApiError> {
    let user = auth::authenticate(&state, &jar).await?;
    check_write_rate(&state, user.id)?;
    if input.confirm.trim() != "DELETE" {
        return Err(ApiError::Invalid(
            "account deletion requires confirm = \"DELETE\"".into(),
        ));
    }
    let lists = state.store.list_workbooks(user.id).await?;
    for wb in &lists.owned {
        state.rooms.shutdown_room(wb.id).await;
    }
    for wb in &lists.shared {
        state.rooms.update_member_role(wb.id, user.id, None).await;
    }
    if let Some(hash) = auth::session_hash(&state.auth, &jar) {
        let _ = state.revocations.send(hash);
    }
    let outcome = state.store.delete_account(user.id).await?;
    for id in outcome.owned_workbooks {
        state.rooms.shutdown_room(id).await;
    }
    for id in outcome.shared_workbooks {
        state.rooms.update_member_role(id, user.id, None).await;
    }
    Ok((
        auth::clear_session_cookie(&state.auth, jar),
        StatusCode::NO_CONTENT,
    ))
}

#[derive(Deserialize, ToSchema)]
struct ClientError {
    version: String,
    summary: String,
}

#[utoipa::path(post, path = "/api/client-errors", request_body = ClientError,
    responses((status = 204), (status = 400), (status = 403), (status = 429)))]
async fn report(
    State(state): State<ApiState>,
    Json(input): Json<ClientError>,
) -> Result<StatusCode, ApiError> {
    if input.version.is_empty()
        || input.version.len() > 80
        || input.summary.is_empty()
        || input.summary.len() > 1024
    {
        return Err(ApiError::Invalid(
            "client error fields are missing or too long".into(),
        ));
    }
    let mut window = state.reports.lock().map_err(|_| ApiError::Internal)?;
    if window.start.elapsed() >= Duration::from_secs(60) {
        *window = ReportWindow {
            start: Instant::now(),
            count: 0,
        };
    }
    if window.count >= 60 {
        return Err(ApiError::RateLimited(60));
    }
    window.count += 1;
    tracing::warn!(
        client_version = %input.version,
        summary = %input.summary,
        "client error reported"
    );
    Ok(StatusCode::NO_CONTENT)
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[derive(OpenApi)]
#[openapi(
    paths(
        auth::dev_login,
        auth::github_login,
        auth::github_callback,
        auth::me,
        auth::logout,
        list_workbooks,
        create_workbook,
        import_workbook,
        workbook,
        update_workbook,
        delete_workbook,
        list_members,
        update_member,
        remove_member,
        list_share_links,
        share,
        revoke_share_link,
        join,
        snapshot,
        history,
        txns,
        list_versions,
        create_version,
        delete_version,
        account,
        delete_account,
        sync::upgrade,
        report
    ),
    modifiers(&BrowserSession)
)]
struct Contract;

struct BrowserSession;

impl Modify for BrowserSession {
    fn modify(&self, spec: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
        if let Some(components) = spec.components.as_mut() {
            components.add_security_scheme(
                "session",
                SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new(auth::SESSION_COOKIE))),
            );
        }
    }
}

pub fn openapi() -> utoipa::openapi::OpenApi {
    Contract::openapi()
}
