//! Shared helpers for the server integration tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::net::SocketAddr;
use std::time::Duration;

use chattercrab_protocol::{ClientMsg, DisplayName, PeerId, RoomSummary, ServerMsg};
use chattercrab_server::{Config, ServerSignals, run};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// Generous upper bound for anything a test waits on.
pub const WAIT: Duration = Duration::from_secs(5);

pub struct TestServer {
    pub addr: SocketAddr,
    pub signals: ServerSignals,
    pub task: JoinHandle<Result<(), chattercrab_server::ServerError>>,
    pub http: reqwest::Client,
}

/// Fast-timing test configuration.
pub fn test_config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        join_timeout: Duration::from_millis(500),
        ping_interval: Duration::from_secs(30),
        sweep_interval: Duration::from_secs(60),
        shutdown_grace: Duration::from_secs(3),
        ..Config::default()
    }
}

pub async fn start() -> TestServer {
    start_with(|_| {}).await
}

pub async fn start_with(customize: impl FnOnce(&mut Config)) -> TestServer {
    let mut config = test_config();
    customize(&mut config);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let signals = ServerSignals::default();
    let task = tokio::spawn(run(config, listener, signals.clone()));
    TestServer {
        addr,
        signals,
        task,
        http: http_client(),
    }
}

/// HTTP client for tests. In a full workspace build, Cargo feature unification gives
/// reqwest the client crate's `rustls-no-provider` feature, which requires a process-wide
/// crypto provider before a `Client` can be built.
fn http_client() -> reqwest::Client {
    // Fails harmlessly if another test already installed it.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

impl TestServer {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn ws_url(&self, room: &str) -> String {
        format!("ws://{}/ws/{}", self.addr, room)
    }

    pub async fn create_room(&self, name: &str) -> RoomSummary {
        let resp = self
            .http
            .post(self.url("/rooms"))
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "create room");
        resp.json().await.unwrap()
    }

    pub async fn rooms(&self) -> Vec<RoomSummary> {
        self.http
            .get(self.url("/rooms"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    pub async fn connect(&self, room: &str) -> Client {
        let (ws, _) = connect_async(self.ws_url(room)).await.unwrap();
        Client { ws }
    }

    /// Connects and joins; returns the client, its peer id and the welcome message.
    pub async fn join(&self, room: &str, name: &str) -> (Client, PeerId, ServerMsg) {
        let mut client = self.connect(room).await;
        client.join(name).await;
        let welcome = client.recv().await;
        let ServerMsg::Welcome { you, .. } = &welcome else {
            panic!("expected welcome, got {welcome:?}");
        };
        (client, *you, welcome)
    }
}

pub struct Client {
    pub ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Client {
    pub async fn send(&mut self, msg: &ClientMsg) {
        let json = serde_json::to_string(msg).unwrap();
        self.ws.send(Message::text(json)).await.unwrap();
    }

    pub async fn send_raw(&mut self, text: &str) {
        self.ws.send(Message::text(text.to_owned())).await.unwrap();
    }

    pub async fn join(&mut self, name: &str) {
        self.send(&ClientMsg::Join {
            display_name: DisplayName::new(name).unwrap(),
        })
        .await;
    }

    /// Next server message (skipping control frames); panics on timeout or close.
    pub async fn recv(&mut self) -> ServerMsg {
        match self.next_event().await {
            Event::Msg(m) => m,
            Event::Closed(code) => panic!("socket closed ({code:?}) while waiting for a message"),
        }
    }

    /// Next message or close.
    pub async fn next_event(&mut self) -> Event {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let frame = tokio::time::timeout_at(deadline, self.ws.next())
                .await
                .expect("timed out waiting for a server frame");
            match frame {
                Some(Ok(Message::Text(text))) => {
                    return Event::Msg(serde_json::from_str(&text).unwrap());
                }
                Some(Ok(Message::Close(frame))) => {
                    return Event::Closed(frame.map(|f| u16::from(f.code)));
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return Event::Closed(None),
            }
        }
    }

    /// Expects an `error` with `code`, then a close frame with `close_code`.
    pub async fn expect_reject(&mut self, code: chattercrab_protocol::ErrorCode, close_code: u16) {
        match self.recv().await {
            ServerMsg::Error { code: got, .. } => assert_eq!(got, code),
            other => panic!("expected error {code:?}, got {other:?}"),
        }
        assert_eq!(self.next_event().await, Event::Closed(Some(close_code)));
    }

    /// Asserts nothing arrives for `dur`.
    pub async fn expect_silence(&mut self, dur: Duration) {
        loop {
            match tokio::time::timeout(dur, self.ws.next()).await {
                Err(_) => return,
                Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
                Ok(other) => panic!("expected silence, got {other:?}"),
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Msg(ServerMsg),
    Closed(Option<u16>),
}
