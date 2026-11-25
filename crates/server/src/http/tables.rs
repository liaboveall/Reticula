//! Read-only tables of a workbook: the list, uploads in pieces, and reads of
//! ranges of bytes.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Extension;
use axum::Json;
use axum::body::{Body, BodyDataStream};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::CookieJar;
use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use utoipa::ToSchema;
use uuid::Uuid;

use super::range::{Wanted, content_range, wanted};
use super::{
    AnonBudget, ApiError, ApiState, ErrorBody, ReadPrincipal, agent::member_only, read_access,
    to_the_end,
};
use crate::ops::TableReader;
use crate::store::{MAX_TABLE_CHUNK_BYTES, NewTable, TableList, TableRecord, User};

const UPLOAD_OFFSET: &str = "upload-offset";
const READ_CHUNK_BYTES: usize = 64 * 1024;
/// How long, and how much of it, to go on taking in a body that was refused.
/// Closing the connection on it would lose the answer.
const DRAIN_FOR: Duration = Duration::from_secs(10);
const DRAIN_BYTES: u64 = 2 * MAX_TABLE_CHUNK_BYTES;

/// Who may change the tables of a workbook: its editors and its owner.
async fn writer(
    state: &ApiState,
    jar: &CookieJar,
    headers: &HeaderMap,
    doc: Uuid,
) -> Result<User, ApiError> {
    let (user, access) = member_only(state, jar, headers, doc).await?;
    if access.role.can_write() {
        Ok(user)
    } else {
        Err(ApiError::Forbidden)
    }
}

fn check_rate(state: &ApiState, user: &User) -> Result<(), ApiError> {
    state
        .limiter
        .check_user_table_write(user.id)
        .map_err(ApiError::RateLimited)
}

#[derive(Deserialize)]
pub(super) struct TablesQuery {
    since: Option<String>,
}

#[utoipa::path(get, path = "/api/workbooks/{id}/tables",
    params(
        ("id" = Uuid, Path),
        ("since" = Option<String>, Query, description = "Version of the list the client has"),
        ("X-Reticula-Share" = Option<String>, Header),
    ),
    security(("session" = [])),
    responses((status = 200, body = TableList), (status = 401), (status = 403), (status = 429)))]
pub(super) async fn list_tables(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Path(id): Path<Uuid>,
    Query(query): Query<TablesQuery>,
) -> Result<Json<TableList>, ApiError> {
    let peer = peer.map(|Extension(info)| info.0);
    let with_uploads =
        match read_access(&state, &jar, &headers, peer, id, AnonBudget::Tables).await? {
            ReadPrincipal::Member(_, book) => book.role.can_write(),
            ReadPrincipal::Anonymous(_) => false,
        };
    Ok(Json(
        state
            .store
            .list_tables(id, query.since.as_deref(), with_uploads)
            .await?,
    ))
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateTable {
    id: Uuid,
    name: String,
    bytes: u64,
    /// SHA-256 of the file in hexadecimal digits.
    sha256: String,
}

#[utoipa::path(post, path = "/api/workbooks/{id}/tables",
    params(("id" = Uuid, Path)),
    request_body = CreateTable,
    security(("session" = [])),
    responses(
        (status = 201, body = TableRecord),
        (status = 200, body = TableRecord, description = "Declared before with the same ID and bytes"),
        (status = 400), (status = 401), (status = 403, body = ErrorBody), (status = 404),
        (status = 409, body = ErrorBody), (status = 413, body = ErrorBody), (status = 422, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 507, body = ErrorBody)))]
pub(super) async fn create_table(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<CreateTable>,
) -> Result<(StatusCode, Json<TableRecord>), ApiError> {
    let user = writer(&state, &jar, &headers, id).await?;
    check_rate(&state, &user)?;
    let sha256 = digest(&input.sha256)
        .ok_or_else(|| ApiError::Invalid("sha256 must have 64 hexadecimal digits".into()))?;
    let table = NewTable {
        id: input.id,
        name: input.name,
        bytes: input.bytes,
        sha256,
    };
    let (record, created) =
        to_the_end(async move { Ok(state.store.create_table(id, user.id, table).await?) }).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(record)))
}

fn digest(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0; 32];
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(at * 2..at * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpdateTable {
    #[serde(default)]
    name: Option<String>,
    /// Where in the list the table goes, counting from 0.
    #[serde(default)]
    position: Option<u32>,
}

#[utoipa::path(patch, path = "/api/workbooks/{id}/tables/{tid}",
    params(("id" = Uuid, Path), ("tid" = Uuid, Path)),
    request_body = UpdateTable,
    security(("session" = [])),
    responses((status = 200, body = TableRecord), (status = 400), (status = 401), (status = 403), (status = 404), (status = 422, body = ErrorBody), (status = 429)))]
pub(super) async fn update_table(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, tid)): Path<(Uuid, Uuid)>,
    Json(input): Json<UpdateTable>,
) -> Result<Json<TableRecord>, ApiError> {
    let user = writer(&state, &jar, &headers, id).await?;
    check_rate(&state, &user)?;
    Ok(Json(
        state
            .store
            .update_table(id, tid, user.id, input.name.as_deref(), input.position)
            .await?,
    ))
}

#[utoipa::path(delete, path = "/api/workbooks/{id}/tables/{tid}",
    params(("id" = Uuid, Path), ("tid" = Uuid, Path)),
    security(("session" = [])),
    responses((status = 204), (status = 401), (status = 403), (status = 404), (status = 429)))]
pub(super) async fn delete_table(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, tid)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let user = writer(&state, &jar, &headers, id).await?;
    check_rate(&state, &user)?;
    to_the_end(async move { Ok(state.store.delete_table(id, tid, user.id).await?) }).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(post, path = "/api/workbooks/{id}/tables/{tid}/complete",
    params(("id" = Uuid, Path), ("tid" = Uuid, Path)),
    security(("session" = [])),
    responses(
        (status = 200, body = TableRecord),
        (status = 401), (status = 403), (status = 404),
        (status = 409, description = "Not all bytes have arrived: uploadOffset says how many"),
        (status = 422, body = ErrorBody, description = "The bytes are not those of the table and were thrown away"),
        (status = 429, body = ErrorBody)))]
pub(super) async fn complete_table(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, tid)): Path<(Uuid, Uuid)>,
) -> Result<Json<TableRecord>, ApiError> {
    let user = writer(&state, &jar, &headers, id).await?;
    check_rate(&state, &user)?;
    to_the_end(async move { Ok(Json(state.store.complete_table(id, tid, user.id).await?)) }).await
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct TableUpload {
    /// How many bytes have arrived, which is where the next request continues.
    upload_offset: u64,
}

#[utoipa::path(patch, path = "/api/workbooks/{id}/tables/{tid}/data",
    params(
        ("id" = Uuid, Path),
        ("tid" = Uuid, Path),
        ("Upload-Offset" = String, Header, description = "How many bytes have arrived before these"),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    security(("session" = [])),
    responses(
        (status = 200, body = TableUpload),
        (status = 400), (status = 401), (status = 403), (status = 404),
        (status = 409, description = "The upload continues elsewhere: uploadOffset says where"),
        (status = 413, body = ErrorBody), (status = 429, body = ErrorBody), (status = 507, body = ErrorBody)))]
pub(super) async fn upload_table_data(
    State(state): State<ApiState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((id, tid)): Path<(Uuid, Uuid)>,
    body: Body,
) -> Result<Json<TableUpload>, ApiError> {
    to_the_end(async move {
        let mut body = body.into_data_stream();
        let received = receive(&state, &jar, &headers, (id, tid), &mut body).await;
        if received.is_err() {
            drain(&headers, &mut body).await;
        }
        Ok(Json(TableUpload {
            upload_offset: received?,
        }))
    })
    .await
}

async fn receive(
    state: &ApiState,
    jar: &CookieJar,
    headers: &HeaderMap,
    (id, tid): (Uuid, Uuid),
    body: &mut BodyDataStream,
) -> Result<u64, ApiError> {
    let user = writer(state, jar, headers, id).await?;
    let offset = headers
        .get(UPLOAD_OFFSET)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| ApiError::Invalid("Upload-Offset must say where the bytes go".into()))?;
    state
        .limiter
        .check_table_upload(user.id)
        .map_err(ApiError::RateLimited)?;
    let mut arrived = 0_u64;
    let received = {
        let mut counted = body.inspect_ok(|bytes| arrived += bytes.len() as u64);
        state
            .store
            .receive_table_data(
                id,
                tid,
                user.id,
                offset,
                content_length(headers),
                &mut counted,
            )
            .await
    };
    state.limiter.charge_table_upload(user.id, arrived);
    Ok(received?)
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

async fn drain(headers: &HeaderMap, body: &mut BodyDataStream) {
    if content_length(headers).is_some_and(|length| length > DRAIN_BYTES) {
        return;
    }
    let mut left = DRAIN_BYTES;
    let _ = tokio::time::timeout(DRAIN_FOR, async {
        while let Some(Ok(bytes)) = body.next().await {
            let Some(rest) = left.checked_sub(bytes.len() as u64) else {
                break;
            };
            left = rest;
        }
    })
    .await;
}

/// Serves `HEAD` as well.
#[utoipa::path(get, path = "/api/workbooks/{id}/tables/{tid}/data",
    params(
        ("id" = Uuid, Path),
        ("tid" = Uuid, Path),
        ("Range" = Option<String>, Header, description = "One range of bytes"),
        ("If-Range" = Option<String>, Header, description = "ETag the range is of"),
        ("X-Reticula-Share" = Option<String>, Header),
    ),
    security(("session" = [])),
    responses(
        (status = 200, body = Vec<u8>, content_type = "application/octet-stream", description = "All of the file, which its SHA-256 is the ETag of"),
        (status = 206, body = Vec<u8>, content_type = "application/octet-stream", description = "The range"),
        (status = 401), (status = 403), (status = 404), (status = 416), (status = 429)))]
pub(super) async fn table_data(
    State(state): State<ApiState>,
    method: Method,
    jar: CookieJar,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Path((id, tid)): Path<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    let peer = peer.map(|Extension(info)| info.0);
    let reader = match read_access(&state, &jar, &headers, peer, id, AnonBudget::Tables).await? {
        ReadPrincipal::Member(user, _) => TableReader::User(user.id),
        ReadPrincipal::Anonymous(_) => {
            TableReader::Anonymous(state.limiter.client_ip(peer, &headers))
        }
    };
    let table = state
        .store
        .ready_table(id, tid)
        .await?
        .ok_or(ApiError::NotFound)?;
    let etag = etag(&table.sha256);
    let is_get = method == Method::GET;
    // A range is of what GET would send.
    let wanted = if is_get {
        wanted(&headers, &etag, table.bytes)
    } else {
        Wanted::All
    };
    let (start, end) = match wanted {
        Wanted::All => (0, table.bytes),
        Wanted::Part(start, end) => (start, end),
        Wanted::Unsatisfiable => (0, 0),
    };
    let length = end.saturating_sub(start);
    state
        .limiter
        .check_table_download(reader, if is_get { length } else { 0 })
        .map_err(ApiError::RateLimited)?;

    let mut response = if wanted == Wanted::Unsatisfiable {
        StatusCode::RANGE_NOT_SATISFIABLE.into_response()
    } else {
        let opened = state.store.open_table(&table).await?;
        let body = if is_get {
            let mut file = tokio::fs::File::from_std(opened.file);
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|_| ApiError::Internal)?;
            Body::from_stream(ReaderStream::with_capacity(
                file.take(length),
                READ_CHUNK_BYTES,
            ))
        } else {
            Body::empty()
        };
        let mut response = Response::new(body);
        let out = response.headers_mut();
        out.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        out.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        response
    };
    let part = match wanted {
        Wanted::All => None,
        Wanted::Part(start, end) => {
            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            content_range(Some((start, end)), table.bytes)
        }
        Wanted::Unsatisfiable => content_range(None, table.bytes),
    };
    let out = response.headers_mut();
    if let Some(part) = part {
        out.insert(header::CONTENT_RANGE, part);
    }
    out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    out.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).map_err(|_| ApiError::Internal)?,
    );
    Ok(response)
}

fn etag(sha256: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(66);
    out.push('"');
    for byte in sha256 {
        let _ = write!(out, "{byte:02x}");
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_64_hexadecimal_digits() {
        let digits = "0123456789abcdefABCDEF".repeat(3);
        let read = digest(&digits[..64]).unwrap();
        assert_eq!(read[..2], [0x01, 0x23]);
        assert_eq!(etag(&read)[1..65], digits[..64].to_lowercase());
        for bad in [
            "",
            &digits[..63],
            &digits[..65],
            &format!("{}g", &digits[..63]),
            &format!("+{}", &digits[..63]),
            // As many bytes, and not as many digits.
            &format!("{}é", &digits[..62]),
        ] {
            assert_eq!(digest(bad), None, "{bad}");
        }
    }
}
