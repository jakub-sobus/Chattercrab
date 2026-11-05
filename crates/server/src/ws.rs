//! WebSocket signaling: one task per connection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket};
use chattercrab_protocol::{
    ClientMsg, DisplayName, ErrorCode, PeerId, ServerMsg, validate_chat_text,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};
use tokio_util::sync::CancellationToken;

use crate::state::{AppState, JoinError, OUTBOUND_QUEUE_CAPACITY, Room, ScreenStopOutcome};

/// Close code: normal closure.
pub const CLOSE_NORMAL: u16 = 1000;
/// Close code: server going away.
pub const CLOSE_GOING_AWAY: u16 = 1001;
/// Close code: policy violation.
pub const CLOSE_POLICY: u16 = 1008;
/// Close code: internal error (used for heartbeat failure).
pub const CLOSE_INTERNAL: u16 = 1011;

/// Maximum time a single socket write may take before the peer is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum time spent sending the final close frame.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

type Sink = SplitSink<WebSocket, Message>;
type Stream = SplitStream<WebSocket>;

/// Why the join phase ended without a member.
enum JoinFailure {
    /// The socket closed or errored.
    Closed,
    /// Protocol violation: report and close with `1008`.
    Reject(ErrorCode, String),
}

/// Serves one upgraded WebSocket for `room` until it closes.
pub async fn serve_connection(
    state: Arc<AppState>,
    room: Arc<Room>,
    socket: WebSocket,
    remote: SocketAddr,
) {
    let (mut sink, mut stream) = socket.split();

    let joined = tokio::select! {
        biased;
        () = state.shutdown.cancelled() => {
            close(&mut sink, CLOSE_GOING_AWAY, "server shutting down").await;
            return;
        }
        res = timeout(state.config.join_timeout, wait_for_join(&mut stream)) => res,
    };
    let display_name = match joined {
        Ok(Ok(name)) => name,
        Ok(Err(JoinFailure::Closed)) => return,
        Ok(Err(JoinFailure::Reject(code, message))) => {
            reject(&mut sink, code, message).await;
            return;
        }
        Err(_elapsed) => {
            reject(
                &mut sink,
                ErrorCode::JoinTimeout,
                "no join within timeout".into(),
            )
            .await;
            return;
        }
    };

    let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
    let kick = state.shutdown.child_token();
    let me = match room
        .join(
            display_name.clone(),
            tx,
            kick.clone(),
            state.config.max_room_size,
        )
        .await
    {
        Ok(id) => id,
        Err(JoinError::RoomFull) => {
            let msg = format!("room is full ({} members)", state.config.max_room_size);
            reject(&mut sink, ErrorCode::RoomFull, msg).await;
            return;
        }
        Err(JoinError::RoomNotFound) => {
            reject(
                &mut sink,
                ErrorCode::RoomNotFound,
                "room no longer exists".into(),
            )
            .await;
            return;
        }
    };
    tracing::info!(room = %room.id, peer = %me, name = %display_name, %remote, "peer joined");

    let close_frame = member_loop(&state, &room, me, &kick, &mut sink, &mut stream, rx).await;

    room.leave(&state, me).await;
    tracing::info!(room = %room.id, peer = %me, "peer left");
    if let Some((code, reason)) = close_frame {
        close(&mut sink, code, reason).await;
    }
}

/// Reads frames until a valid `join` arrives.
async fn wait_for_join(stream: &mut Stream) -> Result<DisplayName, JoinFailure> {
    loop {
        let text = match stream.next().await {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return Err(JoinFailure::Closed),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Binary(_))) => {
                return Err(JoinFailure::Reject(
                    ErrorCode::BadRequest,
                    "binary frames are not supported".into(),
                ));
            }
            Some(Ok(Message::Text(text))) => text,
        };
        return match serde_json::from_str::<ClientMsg>(&text) {
            Ok(ClientMsg::Join { display_name }) => Ok(display_name),
            Ok(_) => Err(JoinFailure::Reject(
                ErrorCode::NotJoined,
                "the first message must be join".into(),
            )),
            Err(err) => {
                let is_join = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .is_some_and(|v| v.get("t").and_then(|t| t.as_str()) == Some("join"));
                if is_join {
                    Err(JoinFailure::Reject(
                        ErrorCode::InvalidDisplayName,
                        "display name must be 1-32 characters without control characters".into(),
                    ))
                } else {
                    Err(JoinFailure::Reject(ErrorCode::BadRequest, err.to_string()))
                }
            }
        };
    }
}

/// Main loop of a joined member. Returns the close frame to send, if any.
async fn member_loop(
    state: &AppState,
    room: &Arc<Room>,
    me: PeerId,
    kick: &CancellationToken,
    sink: &mut Sink,
    stream: &mut Stream,
    mut rx: mpsc::Receiver<ServerMsg>,
) -> Option<(u16, &'static str)> {
    let period = state.config.ping_interval;
    let mut ping = interval_at(Instant::now() + period, period);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut awaiting_pong = false;
    let mut missed_pongs = 0u32;

    loop {
        tokio::select! {
            biased;
            () = kick.cancelled() => {
                return if state.shutdown.is_cancelled() {
                    Some((CLOSE_GOING_AWAY, "server shutting down"))
                } else {
                    Some((CLOSE_POLICY, "slow consumer"))
                };
            }
            queued = rx.recv() => {
                let msg = queued?;
                if !send_msg(sink, kick, &msg).await {
                    return None;
                }
            }
            frame = stream.next() => {
                let reply = match frame {
                    None | Some(Err(_)) => return None,
                    Some(Ok(Message::Close(_))) => return None,
                    Some(Ok(Message::Ping(_))) => None,
                    Some(Ok(Message::Pong(_))) => {
                        awaiting_pong = false;
                        missed_pongs = 0;
                        None
                    }
                    Some(Ok(Message::Binary(_))) => Some(error(
                        ErrorCode::BadRequest,
                        "binary frames are not supported".into(),
                    )),
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMsg>(&text) {
                        Ok(msg) => handle_message(room, me, msg).await,
                        Err(err) => Some(error(ErrorCode::BadRequest, err.to_string())),
                    },
                };
                if let Some(reply) = reply
                    && !send_msg(sink, kick, &reply).await
                {
                    return None;
                }
            }
            _ = ping.tick() => {
                if awaiting_pong {
                    missed_pongs += 1;
                    if missed_pongs >= 2 {
                        tracing::info!(room = %room.id, peer = %me, "heartbeat timeout");
                        return Some((CLOSE_INTERNAL, "heartbeat timeout"));
                    }
                }
                awaiting_pong = true;
                if !send_frame(sink, kick, Message::Ping(Default::default())).await {
                    return None;
                }
            }
        }
    }
}

/// Applies one client message; returns a direct reply for the sender, if any.
async fn handle_message(room: &Arc<Room>, me: PeerId, msg: ClientMsg) -> Option<ServerMsg> {
    match msg {
        ClientMsg::Join { .. } => Some(error(
            ErrorCode::AlreadyJoined,
            "already joined this room".into(),
        )),
        ClientMsg::Offer { to, sdp } => relay(room, to, ServerMsg::Offer { from: me, sdp }).await,
        ClientMsg::Answer { to, sdp } => relay(room, to, ServerMsg::Answer { from: me, sdp }).await,
        ClientMsg::Ice {
            to,
            candidate,
            sdp_mid,
            sdp_mline_index,
        } => {
            let msg = ServerMsg::Ice {
                from: me,
                candidate,
                sdp_mid,
                sdp_mline_index,
            };
            relay(room, to, msg).await
        }
        ClientMsg::Chat { text } => match validate_chat_text(&text) {
            Ok(text) => {
                room.chat(me, text.to_owned()).await;
                None
            }
            Err(err) => Some(error(ErrorCode::ChatTooLong, err.to_string())),
        },
        ClientMsg::ScreenRequest => Some(room.screen_request(me).await),
        ClientMsg::ScreenStop => match room.screen_stop(me).await {
            ScreenStopOutcome::Stopped => None,
            ScreenStopOutcome::NotPublisher => Some(error(
                ErrorCode::NotScreenPublisher,
                "you are not sharing your screen".into(),
            )),
        },
        ClientMsg::Ping => Some(ServerMsg::Pong),
    }
}

async fn relay(room: &Room, to: PeerId, msg: ServerMsg) -> Option<ServerMsg> {
    if room.relay(to, msg).await {
        None
    } else {
        Some(error(
            ErrorCode::PeerNotFound,
            format!("peer {to} is not in this room"),
        ))
    }
}

fn error(code: ErrorCode, message: String) -> ServerMsg {
    ServerMsg::Error { code, message }
}

/// Serializes and writes `msg`. Returns false if the connection should end.
async fn send_msg(sink: &mut Sink, kick: &CancellationToken, msg: &ServerMsg) -> bool {
    match serde_json::to_string(msg) {
        Ok(json) => send_frame(sink, kick, Message::Text(json.into())).await,
        Err(err) => {
            tracing::error!(%err, "failed to serialize server message");
            false
        }
    }
}

/// Writes one frame, giving up if the member is kicked or the write stalls.
async fn send_frame(sink: &mut Sink, kick: &CancellationToken, frame: Message) -> bool {
    tokio::select! {
        biased;
        () = kick.cancelled() => false,
        res = timeout(WRITE_TIMEOUT, sink.send(frame)) => matches!(res, Ok(Ok(()))),
    }
}

/// Sends an `error` message followed by a `1008` close frame.
async fn reject(sink: &mut Sink, code: ErrorCode, message: String) {
    tracing::debug!(?code, %message, "rejecting connection");
    if let Ok(json) = serde_json::to_string(&error(code, message)) {
        let _ = timeout(CLOSE_TIMEOUT, sink.send(Message::Text(json.into()))).await;
    }
    close(sink, CLOSE_POLICY, "policy violation").await;
}

/// Sends a close frame (bounded in time) and flushes.
async fn close(sink: &mut Sink, code: u16, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: Utf8Bytes::from_static(reason),
    };
    let _ = timeout(CLOSE_TIMEOUT, async {
        let _ = sink.send(Message::Close(Some(frame))).await;
        let _ = sink.close().await;
    })
    .await;
}
