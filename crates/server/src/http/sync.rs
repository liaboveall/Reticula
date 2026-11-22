//! Authenticated and anonymous link-view transport for document actors.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Extension;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum_extra::extract::cookie::CookieJar;
use reticula_sync::MemberRole;
use reticula_wire::sync_frame::{
    MAX_SYNC_FRAME_BYTES, SyncFrameError, decode_sync_frame_any, encode_sync_frame,
    encode_sync_frame_v2,
};
use reticula_wire::v2::{SYNC_PROTOCOL_VERSION, WireRejectReasonV1, WireSyncFrameV1};
use reticula_wire::v3::{SYNC_PROTOCOL_VERSION_V2, WireSyncFrameV2};
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout};
use uuid::Uuid;

use super::{ApiError, ApiState, auth};
use crate::docs::{
    CLOSE_ACCESS_REVOKED, CLOSE_SERVER_BUSY, RoomConnection, RoomError, RoomOutbound,
};
use crate::ops::{AnonWsPermit, client_ip_parts};

const HEARTBEAT: Duration = Duration::from_secs(30);
const IDLE: Duration = Duration::from_secs(60);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
pub(super) struct SyncQuery {
    share: Option<String>,
}

enum WsPrincipal {
    Member {
        user_id: Uuid,
        role: MemberRole,
        display_name: String,
        session_hash: [u8; 32],
    },
    Anonymous {
        link_id: Uuid,
        _permit: AnonWsPermit,
    },
}

#[utoipa::path(get, path = "/api/sync/{id}", operation_id = "sync",
    params(("id" = Uuid, Path), ("share" = Option<String>, Query)),
    security(("session" = [])),
    responses((status = 101, description = "Binary sync protocol v1"), (status = 400), (status = 401), (status = 403), (status = 429), (status = 503)))]
pub(super) async fn upgrade(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Query(query): Query<SyncQuery>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    jar: CookieJar,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    state.auth.check_origin(&headers)?;
    let revocations = state.revocations.subscribe();
    let authed = auth::authenticate_optional(&state, &jar).await?;

    let principal = if let Some((ref user, hash)) = authed
        && let Some(access) = state.store.workbook_access(id, user.id).await?
    {
        WsPrincipal::Member {
            user_id: user.id,
            role: access.role,
            display_name: user.name.clone(),
            session_hash: hash,
        }
    } else if let Some(token) = query.share.as_deref().filter(|t| !t.is_empty()) {
        let ip = client_ip_parts(peer.map(|Extension(info)| info.0), &headers);
        if authed.is_none() && crate::ops::has_session_cookie(&headers) {
            state
                .limiter
                .check_anon_ip(ip)
                .map_err(ApiError::RateLimited)?;
        }
        let hash = auth::token_hash(token).ok_or(ApiError::Forbidden)?;
        let anon = state
            .store
            .resolve_anonymous_share(id, &hash)
            .await?
            .ok_or(ApiError::Forbidden)?;
        let permit = state
            .limiter
            .try_acquire_anon_ws(id, ip)
            .ok_or(ApiError::RateLimited(60))?;
        WsPrincipal::Anonymous {
            link_id: anon.link_id,
            _permit: permit,
        }
    } else if authed.is_some() {
        return Err(ApiError::Forbidden);
    } else {
        return Err(ApiError::Unauthorized);
    };

    let upgrade = upgrade.map_err(|_| ApiError::Invalid("expected a WebSocket upgrade".into()))?;
    if state.rooms.would_be_busy(id).await {
        return Err(ApiError::Busy);
    }

    Ok(upgrade
        .max_message_size(MAX_SYNC_FRAME_BYTES)
        .max_frame_size(MAX_SYNC_FRAME_BYTES)
        .max_write_buffer_size(16 * 1024 * 1024)
        .on_upgrade(move |mut socket| async move {
            let Some(hello) = handshake(&mut socket).await else {
                return;
            };
            let (user_id, role, display_name, share_link_id, session_hash, _anon_guard) =
                match principal {
                    WsPrincipal::Member {
                        user_id,
                        role,
                        display_name,
                        session_hash,
                    } => (user_id, role, display_name, None, Some(session_hash), None),
                    WsPrincipal::Anonymous { link_id, _permit } => (
                        link_id,
                        MemberRole::Viewer,
                        "Viewer".to_owned(),
                        Some(link_id),
                        None,
                        Some(_permit),
                    ),
                };

            let reject_proto = match &hello {
                WireSyncFrameV2::Hello {
                    protocol_version, ..
                } if *protocol_version == SYNC_PROTOCOL_VERSION => SYNC_PROTOCOL_VERSION,
                _ => SYNC_PROTOCOL_VERSION_V2,
            };
            match state
                .rooms
                .join_session(id, user_id, role, display_name, share_link_id, hello)
                .await
            {
                Ok(mut room) => {
                    run_connection(&mut socket, &mut room, &state, session_hash, revocations).await;
                }
                Err(RoomError::Rejected(reason)) => {
                    reject_handshake(&mut socket, reject_proto, reason).await;
                }
                Err(RoomError::Busy) => {
                    close_ws(&mut socket, CLOSE_SERVER_BUSY, "ServerBusy").await;
                }
                Err(RoomError::AccessRevoked) => {
                    close_ws(&mut socket, CLOSE_ACCESS_REVOKED, "Access revoked").await;
                }
                Err(RoomError::Unavailable) => {}
            }
        }))
}

async fn handshake(socket: &mut WebSocket) -> Option<WireSyncFrameV2> {
    let frame = match timeout(SEND_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Binary(data)))) => decode_sync_frame_any(&data),
        _ => return None,
    };
    match frame {
        Ok((
            frame_ver,
            hello @ WireSyncFrameV2::Hello {
                protocol_version, ..
            },
        )) => {
            if frame_ver != protocol_version {
                reject_handshake(socket, frame_ver, WireRejectReasonV1::InvalidOp).await;
                return None;
            }
            Some(hello)
        }
        Err(SyncFrameError::UnsupportedVersion(_)) => {
            reject_handshake(
                socket,
                SYNC_PROTOCOL_VERSION,
                WireRejectReasonV1::UpgradeRequired,
            )
            .await;
            None
        }
        _ => {
            reject_handshake(
                socket,
                SYNC_PROTOCOL_VERSION_V2,
                WireRejectReasonV1::InvalidOp,
            )
            .await;
            None
        }
    }
}

async fn reject_handshake(
    socket: &mut WebSocket,
    protocol_version: u8,
    reason: WireRejectReasonV1,
) {
    send_frame(
        socket,
        protocol_version,
        &WireSyncFrameV2::Reject {
            client_seq: 0,
            reason,
        },
    )
    .await;
}

async fn close_ws(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = timeout(
        SEND_TIMEOUT,
        socket.send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }))),
    )
    .await;
}

async fn send_frame(socket: &mut WebSocket, protocol_version: u8, frame: &WireSyncFrameV2) -> bool {
    let encoded = if protocol_version == SYNC_PROTOCOL_VERSION_V2 {
        encode_sync_frame_v2(frame)
    } else if let Ok(v1) = WireSyncFrameV1::try_from(frame) {
        encode_sync_frame(&v1)
    } else {
        return false;
    };
    let Ok(bytes) = encoded else {
        return false;
    };
    send_bytes(socket, bytes.into()).await
}

async fn send_bytes(socket: &mut WebSocket, bytes: axum::body::Bytes) -> bool {
    matches!(
        timeout(SEND_TIMEOUT, socket.send(Message::Binary(bytes))).await,
        Ok(Ok(()))
    )
}

async fn run_connection(
    socket: &mut WebSocket,
    room: &mut RoomConnection,
    state: &ApiState,
    session_hash: Option<[u8; 32]>,
    mut revocations: broadcast::Receiver<[u8; 32]>,
) {
    let mut last_rx = Instant::now();
    let mut heartbeat = interval_at(last_rx + HEARTBEAT, HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ping = 0_u64;
    let mut messages = 0_u16;
    let mut window = last_rx;
    loop {
        tokio::select! {
            revoked = revocations.recv(), if session_hash.is_some() => {
                if let Some(hash) = session_hash
                    && !revoked.is_ok_and(|revoked| revoked != hash)
                {
                    close_ws(socket, CLOSE_ACCESS_REVOKED, "Access revoked").await;
                    break;
                }
            }
            () = sleep_until(last_rx + IDLE) => break,
            _ = heartbeat.tick() => {
                if let Some(hash) = session_hash
                    && !state.store.session_user(&hash).await.is_ok_and(|user| user.is_some())
                {
                    close_ws(socket, CLOSE_ACCESS_REVOKED, "Session expired").await;
                    break;
                }
                ping = ping.wrapping_add(1);
                if !send_frame(socket, room.protocol_version(), &WireSyncFrameV2::Ping(ping)).await {
                    break;
                }
            }
            outbound = room.recv_event() => {
                let Some(event) = outbound else { break };
                match event {
                    RoomOutbound::Frame(bytes) => {
                        if !send_bytes(socket, axum::body::Bytes::from_owner(bytes)).await { break }
                    }
                    RoomOutbound::Close { code, reason } => {
                        close_ws(socket, code, reason).await;
                        break;
                    }
                }
            }
            inbound = socket.recv() => {
                let Some(Ok(inbound)) = inbound else { break };
                last_rx = Instant::now();
                if last_rx.duration_since(window) >= Duration::from_secs(1) {
                    messages = 0;
                    window = last_rx;
                }
                messages += 1;
                if messages > 128 || !handle_message(socket, room, inbound).await { break }
            }
        }
    }
}

async fn handle_message(socket: &mut WebSocket, room: &RoomConnection, message: Message) -> bool {
    let bytes = match message {
        Message::Binary(bytes) => bytes,
        Message::Ping(_) | Message::Pong(_) => return true,
        Message::Close(_) | Message::Text(_) => return false,
    };
    match decode_sync_frame_any(&bytes) {
        Ok((frame_ver, _)) if frame_ver != room.protocol_version() => false,
        Ok((_, WireSyncFrameV2::Push { txns })) => matches!(
            timeout(SEND_TIMEOUT, room.push(txns, bytes.len())).await,
            Ok(Ok(()))
        ),
        Ok((_, WireSyncFrameV2::Presence(value))) => matches!(
            timeout(SEND_TIMEOUT, room.presence(value)).await,
            Ok(Ok(()))
        ),
        Ok((_, WireSyncFrameV2::Ping(nonce))) => {
            send_frame(
                socket,
                room.protocol_version(),
                &WireSyncFrameV2::Pong(nonce),
            )
            .await
        }
        Ok((_, WireSyncFrameV2::Pong(_))) => true,
        _ => false,
    }
}
