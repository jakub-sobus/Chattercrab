//! REST endpoint integration tests.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::num::NonZeroU32;
use std::time::Duration;

use chattercrab_protocol::{ApiError, ErrorCode, HealthResponse};
use common::{start, start_with};

#[tokio::test]
async fn health_reports_ok() {
    let server = start().await;
    let resp = server.http.get(server.url("/health")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: HealthResponse = resp.json().await.unwrap();
    assert_eq!(body.status, "ok");
    assert_eq!(body.version, env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn create_and_list_rooms() {
    let server = start().await;
    assert!(server.rooms().await.is_empty());
    let a = server.create_room("alpha").await;
    let b = server.create_room("  beta  ").await;
    assert_eq!(a.member_count, 0);
    assert_eq!(b.name.as_str(), "beta");
    assert_eq!(a.id.as_str().len(), 12);
    assert_ne!(a.id, b.id);
    let rooms = server.rooms().await;
    assert_eq!(rooms.len(), 2);
    assert_eq!(rooms[0].id, a.id, "sorted by created_at");
}

#[tokio::test]
async fn create_room_validation() {
    let server = start().await;
    let resp = server
        .http
        .post(server.url("/rooms"))
        .json(&serde_json::json!({ "name": "   " }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::InvalidRoomName);

    let resp = server
        .http
        .post(server.url("/rooms"))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::BadRequest);

    let resp = server
        .http
        .post(server.url("/rooms"))
        .body(vec![b'x'; 32 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413, "body limit");
}

#[tokio::test]
async fn max_rooms_enforced() {
    let server = start_with(|c| c.max_rooms = 2).await;
    server.create_room("one").await;
    server.create_room("two").await;
    let resp = server
        .http
        .post(server.url("/rooms"))
        .json(&serde_json::json!({ "name": "three" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::MaxRooms);
}

#[tokio::test]
async fn create_room_rate_limited() {
    let server = start_with(|c| {
        c.create_room_per_minute = NonZeroU32::new(1).unwrap();
        c.create_room_burst = NonZeroU32::new(2).unwrap();
    })
    .await;
    server.create_room("one").await;
    server.create_room("two").await;
    let resp = server
        .http
        .post(server.url("/rooms"))
        .json(&serde_json::json!({ "name": "three" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::RateLimited);
}

#[tokio::test]
async fn ice_servers_returns_configured_stun_url() {
    let server = start().await;
    let resp = server
        .http
        .get(server.url("/ice-servers"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({ "urls": ["stun:stun.l.google.com:19302"] })
    );
    assert!(body.get("username").is_none());
    assert!(body.get("credential").is_none());

    let gone = server
        .http
        .get(server.url("/turn-credentials"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 404);
}

#[tokio::test]
async fn ice_servers_reflects_custom_stun_url() {
    let server = start_with(|c| {
        c.stun_url = "stun:stun.example.com:3478".into();
    })
    .await;
    let resp = server
        .http
        .get(server.url("/ice-servers"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({ "urls": ["stun:stun.example.com:3478"] })
    );
}

#[tokio::test]
async fn ice_servers_rate_limited() {
    let server = start_with(|c| {
        c.ice_per_minute = NonZeroU32::new(1).unwrap();
    })
    .await;
    let first = server
        .http
        .get(server.url("/ice-servers"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let resp = server
        .http
        .get(server.url("/ice-servers"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.code, ErrorCode::RateLimited);
    assert!(err.message.contains("ICE"));
}

#[tokio::test]
async fn ws_to_unknown_room_is_404() {
    let server = start().await;
    let err = tokio_tungstenite::connect_async(server.ws_url("doesnotexist"))
        .await
        .unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => assert_eq!(resp.status(), 404),
        other => panic!("expected HTTP 404, got {other:?}"),
    }
}

#[tokio::test]
async fn idle_empty_rooms_are_evicted() {
    let server = start_with(|c| {
        c.room_idle_ttl = Duration::from_millis(200);
        c.sweep_interval = Duration::from_millis(50);
    })
    .await;
    let idle = server.create_room("idle").await;
    let busy = server.create_room("busy").await;
    let (_client, _, _) = server.join(busy.id.as_str(), "ada").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let rooms = server.rooms().await;
    assert!(rooms.iter().all(|r| r.id != idle.id), "idle room evicted");
    let busy_now = rooms
        .iter()
        .find(|r| r.id == busy.id)
        .expect("busy room kept");
    assert_eq!(busy_now.member_count, 1);
}
