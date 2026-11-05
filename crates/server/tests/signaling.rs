//! WebSocket signaling integration tests.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::time::Duration;

use chattercrab_protocol::{ClientMsg, ErrorCode, PeerId, ServerMsg};
use common::{Event, start, start_with};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

fn chat(text: &str) -> ClientMsg {
    ClientMsg::Chat { text: text.into() }
}

#[tokio::test]
async fn join_welcome_and_peer_list() {
    let server = start().await;
    let room = server.create_room("r").await;
    let (mut a, a_id, welcome) = server.join(room.id.as_str(), "ada").await;
    let ServerMsg::Welcome {
        peers,
        history,
        screen_publisher,
        ..
    } = welcome
    else {
        unreachable!()
    };
    assert!(peers.is_empty());
    assert!(history.is_empty());
    assert_eq!(screen_publisher, None);

    let (mut b, b_id, welcome) = server.join(room.id.as_str(), "grace").await;
    let ServerMsg::Welcome { peers, .. } = welcome else {
        unreachable!()
    };
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].id, a_id);
    assert_eq!(peers[0].display_name.as_str(), "ada");

    match a.recv().await {
        ServerMsg::PeerJoined { peer } => {
            assert_eq!(peer.id, b_id);
            assert_eq!(peer.display_name.as_str(), "grace");
        }
        other => panic!("expected peer_joined, got {other:?}"),
    }

    let (_c, c_id, welcome) = server.join(room.id.as_str(), "linus").await;
    let ServerMsg::Welcome { peers, .. } = welcome else {
        unreachable!()
    };
    let ids: Vec<PeerId> = peers.iter().map(|p| p.id).collect();
    assert_eq!(ids, [a_id, b_id], "peers listed in join order");
    assert!(matches!(a.recv().await, ServerMsg::PeerJoined { peer } if peer.id == c_id));
    assert!(matches!(b.recv().await, ServerMsg::PeerJoined { peer } if peer.id == c_id));

    let rooms = server.rooms().await;
    assert_eq!(rooms[0].member_count, 3);

    b.close().await;
    assert_eq!(a.recv().await, ServerMsg::PeerLeft { peer_id: b_id });
}

#[tokio::test]
async fn offer_answer_ice_relay() {
    let server = start().await;
    let room = server.create_room("r").await;
    let (mut a, a_id, _) = server.join(room.id.as_str(), "ada").await;
    let (mut b, b_id, _) = server.join(room.id.as_str(), "grace").await;
    let _ = a.recv().await; // peer_joined

    a.send(&ClientMsg::Offer {
        to: b_id,
        sdp: "v=0 offer".into(),
    })
    .await;
    assert_eq!(
        b.recv().await,
        ServerMsg::Offer {
            from: a_id,
            sdp: "v=0 offer".into()
        }
    );
    b.send(&ClientMsg::Answer {
        to: a_id,
        sdp: "v=0 answer".into(),
    })
    .await;
    assert_eq!(
        a.recv().await,
        ServerMsg::Answer {
            from: b_id,
            sdp: "v=0 answer".into()
        }
    );
    a.send(&ClientMsg::Ice {
        to: b_id,
        candidate: "candidate:1 1 udp 1 127.0.0.1 9 typ host".into(),
        sdp_mid: Some("0".into()),
        sdp_mline_index: Some(0),
    })
    .await;
    assert_eq!(
        b.recv().await,
        ServerMsg::Ice {
            from: a_id,
            candidate: "candidate:1 1 udp 1 127.0.0.1 9 typ host".into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
        }
    );

    // Unknown recipient: non-fatal error, socket stays open.
    a.send(&ClientMsg::Offer {
        to: PeerId::random(),
        sdp: "x".into(),
    })
    .await;
    assert!(matches!(
        a.recv().await,
        ServerMsg::Error {
            code: ErrorCode::PeerNotFound,
            ..
        }
    ));
    a.send(&ClientMsg::Ping).await;
    assert_eq!(a.recv().await, ServerMsg::Pong);
}

#[tokio::test]
async fn join_timeout_closes_socket() {
    let server = start().await;
    let room = server.create_room("r").await;
    let mut client = server.connect(room.id.as_str()).await;
    client.expect_reject(ErrorCode::JoinTimeout, 1008).await;
}

#[tokio::test]
async fn message_before_join_is_rejected() {
    let server = start().await;
    let room = server.create_room("r").await;
    let mut client = server.connect(room.id.as_str()).await;
    client.send(&ClientMsg::Ping).await;
    client.expect_reject(ErrorCode::NotJoined, 1008).await;

    let mut client = server.connect(room.id.as_str()).await;
    client
        .send_raw(r#"{"t":"join","display_name":"   "}"#)
        .await;
    client
        .expect_reject(ErrorCode::InvalidDisplayName, 1008)
        .await;

    let mut client = server.connect(room.id.as_str()).await;
    client.send_raw("garbage").await;
    client.expect_reject(ErrorCode::BadRequest, 1008).await;
}

#[tokio::test]
async fn bad_messages_after_join_are_not_fatal() {
    let server = start().await;
    let room = server.create_room("r").await;
    let (mut a, _, _) = server.join(room.id.as_str(), "ada").await;
    a.send_raw(r#"{"t":"nope"}"#).await;
    assert!(matches!(
        a.recv().await,
        ServerMsg::Error {
            code: ErrorCode::BadRequest,
            ..
        }
    ));
    a.join("again").await;
    assert!(matches!(
        a.recv().await,
        ServerMsg::Error {
            code: ErrorCode::AlreadyJoined,
            ..
        }
    ));
    a.ws.send(Message::binary(vec![1u8, 2, 3])).await.unwrap();
    assert!(matches!(
        a.recv().await,
        ServerMsg::Error {
            code: ErrorCode::BadRequest,
            ..
        }
    ));
    a.send(&ClientMsg::Ping).await;
    assert_eq!(a.recv().await, ServerMsg::Pong);
}

#[tokio::test]
async fn room_full_rejected() {
    let server = start_with(|c| c.max_room_size = 2).await;
    let room = server.create_room("r").await;
    let (_a, _, _) = server.join(room.id.as_str(), "ada").await;
    let (_b, _, _) = server.join(room.id.as_str(), "grace").await;
    let mut c = server.connect(room.id.as_str()).await;
    c.join("linus").await;
    c.expect_reject(ErrorCode::RoomFull, 1008).await;
}

#[tokio::test]
async fn screen_lock_semantics() {
    let server = start().await;
    let room = server.create_room("r").await;
    let (mut a, a_id, _) = server.join(room.id.as_str(), "ada").await;
    let (mut b, _b_id, _) = server.join(room.id.as_str(), "grace").await;
    let _ = a.recv().await; // peer_joined

    a.send(&ClientMsg::ScreenRequest).await;
    assert_eq!(a.recv().await, ServerMsg::ScreenGranted);
    assert_eq!(a.recv().await, ServerMsg::ScreenStarted { publisher: a_id });
    assert_eq!(b.recv().await, ServerMsg::ScreenStarted { publisher: a_id });

    // Repeated request from the publisher: granted again, no broadcast.
    a.send(&ClientMsg::ScreenRequest).await;
    assert_eq!(a.recv().await, ServerMsg::ScreenGranted);

    b.send(&ClientMsg::ScreenRequest).await;
    match b.recv().await {
        ServerMsg::ScreenDenied { reason } => assert!(reason.contains("ada"), "{reason}"),
        other => panic!("expected screen_denied, got {other:?}"),
    }
    b.send(&ClientMsg::ScreenStop).await;
    assert!(matches!(
        b.recv().await,
        ServerMsg::Error {
            code: ErrorCode::NotScreenPublisher,
            ..
        }
    ));

    // Newcomers learn the current publisher from welcome.
    let (mut c, _, welcome) = server.join(room.id.as_str(), "linus").await;
    assert!(matches!(
        welcome,
        ServerMsg::Welcome { screen_publisher: Some(p), .. } if p == a_id
    ));
    let _ = a.recv().await;
    let _ = b.recv().await;

    a.send(&ClientMsg::ScreenStop).await;
    assert_eq!(a.recv().await, ServerMsg::ScreenStopped);
    assert_eq!(b.recv().await, ServerMsg::ScreenStopped);
    assert_eq!(c.recv().await, ServerMsg::ScreenStopped);

    // b takes the lock, then disconnects: the lock is released for everyone.
    b.send(&ClientMsg::ScreenRequest).await;
    assert_eq!(b.recv().await, ServerMsg::ScreenGranted);
    let ServerMsg::ScreenStarted { publisher } = a.recv().await else {
        panic!("expected screen_started")
    };
    let _ = c.recv().await;
    b.close().await;
    assert_eq!(a.recv().await, ServerMsg::PeerLeft { peer_id: publisher });
    assert_eq!(a.recv().await, ServerMsg::ScreenStopped);
    assert_eq!(c.recv().await, ServerMsg::PeerLeft { peer_id: publisher });
    assert_eq!(c.recv().await, ServerMsg::ScreenStopped);

    c.send(&ClientMsg::ScreenRequest).await;
    assert_eq!(c.recv().await, ServerMsg::ScreenGranted);
}

#[tokio::test]
async fn chat_echo_history_and_limits() {
    let server = start().await;
    let room = server.create_room("r").await;
    let (mut a, a_id, _) = server.join(room.id.as_str(), "ada").await;

    a.send(&chat("  hello  ")).await;
    match a.recv().await {
        ServerMsg::Chat { msg } => {
            assert_eq!(msg.id, 1);
            assert_eq!(msg.from, a_id);
            assert_eq!(msg.text, "hello", "trimmed");
            assert_eq!(msg.display_name.as_str(), "ada");
            assert!(msg.ts > 0);
        }
        other => panic!("expected chat echo, got {other:?}"),
    }

    a.send(&chat(&"x".repeat(2001))).await;
    assert!(matches!(
        a.recv().await,
        ServerMsg::Error {
            code: ErrorCode::ChatTooLong,
            ..
        }
    ));
    a.send(&chat("   ")).await;
    assert!(matches!(
        a.recv().await,
        ServerMsg::Error {
            code: ErrorCode::ChatTooLong,
            ..
        }
    ));

    for i in 2..=205 {
        a.send(&chat(&format!("m{i}"))).await;
    }
    for i in 2..=205u64 {
        let ServerMsg::Chat { msg } = a.recv().await else {
            panic!("expected chat")
        };
        assert_eq!(msg.id, i);
    }

    let (mut b, _, welcome) = server.join(room.id.as_str(), "grace").await;
    let ServerMsg::Welcome { history, .. } = welcome else {
        unreachable!()
    };
    assert_eq!(history.len(), 200, "history capped at 200");
    assert_eq!(history.first().unwrap().id, 6, "oldest first");
    assert_eq!(history.last().unwrap().id, 205);
    assert_eq!(history.last().unwrap().text, "m205");
    assert!(history.windows(2).all(|w| w[0].id < w[1].id));

    // Broadcast reaches everyone, including the sender.
    let _ = a.recv().await; // peer_joined
    b.send(&chat("hi all")).await;
    assert!(matches!(a.recv().await, ServerMsg::Chat { msg } if msg.text == "hi all"));
    assert!(matches!(b.recv().await, ServerMsg::Chat { msg } if msg.text == "hi all"));
}

#[tokio::test]
async fn room_deleted_when_last_member_leaves() {
    let server = start().await;
    let room = server.create_room("r").await;
    let (a, _, _) = server.join(room.id.as_str(), "ada").await;
    assert_eq!(server.rooms().await.len(), 1);
    a.close().await;
    let deadline = tokio::time::Instant::now() + common::WAIT;
    while !server.rooms().await.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "room not deleted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let err = tokio_tungstenite::connect_async(server.ws_url(room.id.as_str()))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 404
    ));
}

#[tokio::test]
async fn heartbeat_drops_unresponsive_peer() {
    let server = start_with(|c| c.ping_interval = Duration::from_millis(100)).await;
    let room = server.create_room("r").await;
    let (mut a, _, _) = server.join(room.id.as_str(), "ada").await;
    // b joins and then never reads, so it never answers pings.
    let (b, b_id, _) = server.join(room.id.as_str(), "grace").await;
    assert!(matches!(a.recv().await, ServerMsg::PeerJoined { .. }));
    // a keeps reading (tungstenite answers pings while reading), so only b is dropped.
    assert_eq!(a.recv().await, ServerMsg::PeerLeft { peer_id: b_id });
    drop(b);
}

#[tokio::test]
async fn slow_consumer_is_kicked() {
    let server = start().await;
    let room = server.create_room("r").await;
    let (a, _, _) = server.join(room.id.as_str(), "ada").await;
    // The slow peer joins and never reads its socket.
    let (slow, slow_id, _) = server.join(room.id.as_str(), "slowpoke").await;

    let (mut sink, mut stream) = a.ws.split();
    let reader = tokio::spawn(async move {
        while let Some(Ok(frame)) = stream.next().await {
            if let Message::Text(text) = frame
                && let Ok(ServerMsg::PeerLeft { peer_id }) = serde_json::from_str(&text)
            {
                return Some(peer_id);
            }
        }
        None
    });
    let big = "x".repeat(1900);
    for _ in 0..20_000 {
        if reader.is_finished() {
            break;
        }
        let json = serde_json::to_string(&chat(&big)).unwrap();
        if sink.send(Message::text(json)).await.is_err() {
            break;
        }
    }
    let left = tokio::time::timeout(Duration::from_secs(20), reader)
        .await
        .expect("slow consumer was not kicked")
        .unwrap();
    assert_eq!(left, Some(slow_id));
    drop(slow);
}

#[tokio::test]
async fn graceful_shutdown_sends_going_away() {
    let server = start().await;
    let room = server.create_room("r").await;
    let (mut a, _, _) = server.join(room.id.as_str(), "ada").await;
    let started = std::time::Instant::now();
    server.signals.shutdown.cancel();
    assert_eq!(a.next_event().await, Event::Closed(Some(1001)));
    tokio::time::timeout(Duration::from_secs(5), server.task)
        .await
        .expect("server did not stop in time")
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(4));
}
