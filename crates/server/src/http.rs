//! Same-origin collaboration API and its OpenAPI contract.

mod auth;
mod sync;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::CookieJar;
use reticula_sync::MemberRole;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::io::ReaderStream;
use utoipa::{Modify, OpenApi, ToSchema};
use uuid::Uuid;

use crate::docs::DocRooms;
use crate::store::{MAIN_BRANCH, Store, StoreError, User, WorkbookAccess};

pub use auth::AuthConfig;

#[derive(Clone)]
pub struct ApiState {
    store: Store,
    auth: Arc<AuthConfig>,
    rooms: DocRooms,
    revocations: broadcast::Sender<[u8; 32]>,
    reports: Arc<Mutex<ReportWindow>>,
}

struct ReportWindow {
    start: Instant,
    count: u16,
}

impl ApiState {
    pub fn new(store: Store, auth: AuthConfig) -> Self {
        Self {
            rooms: DocRooms::new(store.clone()),
            store,
            auth: Arc::new(auth),
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
}

pub fn router(state: ApiState) -> Router {
    let mut routes = Router::new()
        .route("/auth/github/login", get(auth::github_login))
        .route("/auth/github/callback", get(auth::github_callback))
        .route("/auth/me", get(auth::me))
        .route("/auth/logout", post(auth::logout))
        .route("/workbooks", post(create_workbook))
        .route("/workbooks/{id}", get(workbook))
        .route("/workbooks/{id}/share-links", post(share))
        .route("/join/{token}", post(join))
        .route("/workbooks/{id}/snapshots/{seq}", get(snapshot))
        .route("/sync/{id}", get(sync::upgrade))
        .route("/client-errors", post(report))
        .route("/openapi.json", get(|| async { Json(openapi()) }));
    if state.auth.is_dev_mock() {
        routes = routes.route("/auth/dev-login", post(auth::dev_login));
    }
    routes
        .fallback(|| async { ApiError::NotFound })
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            same_origin_mutations,
        ))
        .with_state(state)
}

async fn same_origin_mutations(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if request.method() == Method::POST
        && state.auth.check_origin(request.headers()).is_err()
    {
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
    Invalid(&'static str),
    RateLimited,
    Provider,
    Internal,
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        eprintln!("collaboration API storage error: {error}");
        Self::Internal
    }
}

#[derive(Serialize, ToSchema)]
struct ErrorBody {
    error: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "authentication required"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "access denied"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found"),
            Self::Invalid(reason) => (StatusCode::BAD_REQUEST, reason),
            Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded"),
            Self::Provider => (StatusCode::BAD_GATEWAY, "GitHub authentication failed"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
        };
        (status, Json(ErrorBody { error })).into_response()
    }
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

#[derive(Serialize, ToSchema)]
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

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct WorkbookSummary {
    id: Uuid,
    branch_id: Uuid,
    title: String,
    epoch: u8,
    head_seq: String,
    role: Role,
    members: BTreeMap<String, u64>,
}

#[utoipa::path(post, path = "/api/workbooks", request_body = CreateWorkbook, security(("session" = [])),
    responses((status = 201, body = WorkbookKey), (status = 400, body = ErrorBody), (status = 401), (status = 403)))]
async fn create_workbook(
    State(state): State<ApiState>,
    jar: CookieJar,
    Json(input): Json<CreateWorkbook>,
) -> Result<impl IntoResponse, ApiError> {
    let user = auth::authenticate(&state, &jar).await?;
    let title = input.title.trim();
    if title.is_empty() || title.chars().count() > 120 || title.chars().any(char::is_control) {
        return Err(ApiError::Invalid(
            "title must contain 1 to 120 printable characters",
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

#[utoipa::path(get, path = "/api/workbooks/{id}", params(("id" = Uuid, Path)), security(("session" = [])),
    responses((status = 200, body = WorkbookSummary), (status = 401), (status = 403)))]
async fn workbook(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path(id): Path<Uuid>,
) -> Result<Json<WorkbookSummary>, ApiError> {
    let (_, book) = member(&state, &jar, id).await?;
    let members = state.store.workbook_members(id).await?;
    Ok(Json(WorkbookSummary {
        id: book.id,
        branch_id: MAIN_BRANCH,
        title: book.title,
        epoch: book.epoch,
        head_seq: book.head.to_string(),
        role: book.role.into(),
        members,
    }))
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateShareLink {
    #[serde(default)]
    single_use: bool,
    ttl_seconds: Option<u32>,
}

#[derive(Serialize, ToSchema)]
struct ShareLink {
    url: String,
}

#[utoipa::path(post, path = "/api/workbooks/{id}/share-links", params(("id" = Uuid, Path)), request_body = CreateShareLink,
    security(("session" = [])), responses((status = 201, body = ShareLink), (status = 400), (status = 401), (status = 403)))]
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
    if input
        .ttl_seconds
        .is_some_and(|ttl| !(1..=31_536_000).contains(&ttl))
    {
        return Err(ApiError::Invalid(
            "link lifetime must be between 1 second and 1 year",
        ));
    }
    let token = auth::encode_secret(&auth::random_secret()?);
    let hash = auth::token_hash(&token).ok_or(ApiError::Internal)?;
    if !state
        .store
        .create_share_link(id, user.id, &hash, input.single_use, input.ttl_seconds)
        .await?
    {
        return Err(ApiError::Forbidden);
    }
    Ok((
        StatusCode::CREATED,
        Json(ShareLink {
            url: format!("/join/{token}"),
        }),
    ))
}

#[utoipa::path(post, path = "/api/join/{token}", params(("token" = String, Path)), security(("session" = [])),
    responses((status = 200, body = WorkbookKey), (status = 401), (status = 403)))]
async fn join(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path(token): Path<String>,
) -> Result<Json<WorkbookKey>, ApiError> {
    let user = auth::authenticate(&state, &jar).await?;
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

#[utoipa::path(get, path = "/api/workbooks/{id}/snapshots/{seq}", params(("id" = Uuid, Path), ("seq" = String, Path)),
    security(("session" = [])), responses((status = 200, body = String, content_type = "application/octet-stream"), (status = 401), (status = 403), (status = 404)))]
async fn snapshot(
    State(state): State<ApiState>,
    jar: CookieJar,
    Path((id, seq)): Path<(Uuid, u64)>,
) -> Result<Response, ApiError> {
    member(&state, &jar, id).await?;
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
            "client error fields are missing or too long",
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
        return Err(ApiError::RateLimited);
    }
    window.count += 1;
    eprintln!(
        "client error from version {:?}: {:?}",
        input.version, input.summary
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(OpenApi)]
#[openapi(paths(auth::dev_login, auth::github_login, auth::github_callback, auth::me, auth::logout,
    create_workbook, workbook, share, join, snapshot, sync::upgrade, report), modifiers(&BrowserSession))]
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
