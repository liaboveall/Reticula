//! Authenticated transport for the document actor and binary sync protocol.

use std::time::Duration;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum_extra::extract::cookie::CookieJar;
use reticula_wire::sync_frame::{
    MAX_SYNC_FRAME_BYTES, SyncFrameError, decode_sync_frame, encode_sync_frame,
};
use reticula_wire::v2::{WireRejectReasonV1, WireSyncFrameV1};
use tokio::sync::broadcast;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout};
use uuid::Uuid;

use super::{ApiError, ApiState, auth, member};
use crate::docs::{RoomConnection, RoomError};

const HEARTBEAT: Duration = Duration::from_secs(30);
const IDLE: Duration = Duration::from_secs(60);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[utoipa::path(get, path = "/api/sync/{id}", operation_id = "sync", params(("id" = Uuid, Path)), security(("session" = [])),
    responses((status = 101, description = "Binary sync protocol v1"), (status = 400), (status = 401), (status = 403)))]
pub(super) async fn upgrade(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    jar: CookieJar,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    state.auth.check_origin(&headers)?;
    let revocations = state.revocations.subscribe();
    let hash = auth::session_hash(&state.auth, &jar).ok_or(ApiError::Unauthorized)?;
    let (user, access) = member(&state, &jar, id).await?;
    let upgrade = upgrade.map_err(|_| ApiError::Invalid("expected a WebSocket upgrade"))?;
    Ok(upgrade
        .max_message_size(MAX_SYNC_FRAME_BYTES)
        .max_frame_size(MAX_SYNC_FRAME_BYTES)
        .max_write_buffer_size(16 * 1024 * 1024)
        .on_upgrade(move |mut socket| async move {
            let Some(hello) = handshake(&mut socket).await else {
                return;
            };
            match state
                .rooms
                .join(id, user.id, access.role, user.name, hello)
                .await
            {
                Ok(mut room) => {
                    run_connection(&mut socket, &mut room, &state, hash, revocations).await;
                }
                Err(RoomError::Rejected(reason)) => {
                    reject_handshake(&mut socket, reason).await;
                }
                Err(RoomError::Unavailable) => {}
            }
        }))
}

async fn handshake(socket: &mut WebSocket) -> Option<WireSyncFrameV1> {
    let frame = match timeout(SEND_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Binary(data)))) => decode_sync_frame(&data),
        _ => return None,
    };
    match frame {
        Ok(hello @ WireSyncFrameV1::Hello { .. }) => Some(hello),
        Err(SyncFrameError::UnsupportedVersion(_)) => {
            reject_handshake(socket, WireRejectReasonV1::UpgradeRequired).await;
            None
        }
        _ => {
            reject_handshake(socket, WireRejectReasonV1::InvalidOp).await;
            None
        }
    }
}

async fn reject_handshake(socket: &mut WebSocket, reason: WireRejectReasonV1) {
    send_frame(
        socket,
        &WireSyncFrameV1::Reject {
            client_seq: 0,
            reason,
        },
    )
    .await;
}

async fn send_frame(socket: &mut WebSocket, frame: &WireSyncFrameV1) -> bool {
    let Ok(bytes) = encode_sync_frame(frame) else {
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
    hash: [u8; 32],
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
            revoked = revocations.recv() => {
                if !revoked.is_ok_and(|revoked| revoked != hash) { break }
            }
            () = sleep_until(last_rx + IDLE) => break,
            _ = heartbeat.tick() => {
                if !state.store.session_user(&hash).await.is_ok_and(|user| user.is_some()) { break }
                ping = ping.wrapping_add(1);
                if !send_frame(socket, &WireSyncFrameV1::Ping(ping)).await { break }
            }
            outbound = room.recv() => {
                let Some(bytes) = outbound else { break };
                if !send_bytes(socket, axum::body::Bytes::from_owner(bytes)).await { break }
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
    match decode_sync_frame(&bytes) {
        Ok(WireSyncFrameV1::Push { txns }) => matches!(
            timeout(SEND_TIMEOUT, room.push(txns, bytes.len())).await,
            Ok(Ok(()))
        ),
        Ok(WireSyncFrameV1::Presence(value)) => matches!(
            timeout(SEND_TIMEOUT, room.presence(value)).await,
            Ok(Ok(()))
        ),
        Ok(WireSyncFrameV1::Ping(nonce)) => send_frame(socket, &WireSyncFrameV1::Pong(nonce)).await,
        Ok(WireSyncFrameV1::Pong(_)) => true,
        _ => false,
    }
}
