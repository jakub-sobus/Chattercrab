//! WebSocket signaling messages.

use serde::{Deserialize, Serialize};

use crate::{DisplayName, ErrorCode, PeerId, UnixMillis};

/// A member of a room as seen by other members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Server-assigned peer id.
    pub id: PeerId,
    /// Display name chosen at join.
    pub display_name: DisplayName,
}

/// A chat message stored in room history and broadcast to members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Monotonically increasing id within the room.
    pub id: u64,
    /// Sender.
    pub from: PeerId,
    /// Sender's display name at the time of sending.
    pub display_name: DisplayName,
    /// Message text (1 to 2000 characters).
    pub text: String,
    /// Server receive time.
    pub ts: UnixMillis,
}

/// Messages sent by a client to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Join the room addressed by the WebSocket URL. Must be the first message.
    Join {
        /// Name shown to other members.
        display_name: DisplayName,
    },
    /// SDP offer for one peer.
    Offer {
        /// Recipient.
        to: PeerId,
        /// Session description.
        sdp: String,
    },
    /// SDP answer for one peer.
    Answer {
        /// Recipient.
        to: PeerId,
        /// Session description.
        sdp: String,
    },
    /// Trickle ICE candidate for one peer.
    Ice {
        /// Recipient.
        to: PeerId,
        /// Candidate attribute; empty signals end-of-candidates.
        candidate: String,
        /// Media stream identification tag.
        sdp_mid: Option<String>,
        /// Index of the m-line the candidate belongs to.
        sdp_mline_index: Option<u16>,
    },
    /// Chat message for the whole room.
    Chat {
        /// Message text.
        text: String,
    },
    /// Ask for the room's screen-share lock.
    ScreenRequest,
    /// Release the screen-share lock.
    ScreenStop,
    /// Application-level keepalive; answered with `pong`.
    Ping,
}

/// Messages sent by the server to a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Reply to `join`.
    Welcome {
        /// The receiver's own peer id.
        you: PeerId,
        /// Other members already in the room.
        peers: Vec<PeerInfo>,
        /// Recent chat history, oldest first.
        history: Vec<ChatMessage>,
        /// Current screen publisher, if any.
        screen_publisher: Option<PeerId>,
    },
    /// A new member joined.
    PeerJoined {
        /// The new member.
        peer: PeerInfo,
    },
    /// A member left.
    PeerLeft {
        /// The departed member.
        peer_id: PeerId,
    },
    /// Relayed SDP offer.
    Offer {
        /// Sender.
        from: PeerId,
        /// Session description.
        sdp: String,
    },
    /// Relayed SDP answer.
    Answer {
        /// Sender.
        from: PeerId,
        /// Session description.
        sdp: String,
    },
    /// Relayed ICE candidate.
    Ice {
        /// Sender.
        from: PeerId,
        /// Candidate attribute; empty signals end-of-candidates.
        candidate: String,
        /// Media stream identification tag.
        sdp_mid: Option<String>,
        /// Index of the m-line the candidate belongs to.
        sdp_mline_index: Option<u16>,
    },
    /// A chat message (including the receiver's own, as confirmation).
    Chat {
        /// The message.
        msg: ChatMessage,
    },
    /// The receiver now holds the screen-share lock.
    ScreenGranted,
    /// The screen-share lock is held by someone else.
    ScreenDenied {
        /// Human-readable reason.
        reason: String,
    },
    /// A member started sharing their screen.
    ScreenStarted {
        /// The publisher.
        publisher: PeerId,
    },
    /// The screen share ended.
    ScreenStopped,
    /// A request failed.
    Error {
        /// Machine-readable code.
        code: ErrorCode,
        /// Human-readable description.
        message: String,
    },
    /// Reply to `ping`.
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> PeerId {
        format!("00000000-0000-4000-8000-0000000000{n:02x}")
            .parse()
            .unwrap()
    }

    fn name(s: &str) -> DisplayName {
        DisplayName::new(s).unwrap()
    }

    fn chat_message() -> ChatMessage {
        ChatMessage {
            id: 17,
            from: peer(1),
            display_name: name("ada"),
            text: "hello".into(),
            ts: 1_793_875_200_000,
        }
    }

    fn all_client_msgs() -> Vec<ClientMsg> {
        vec![
            ClientMsg::Join {
                display_name: name("ada"),
            },
            ClientMsg::Offer {
                to: peer(2),
                sdp: "v=0\r\n".into(),
            },
            ClientMsg::Answer {
                to: peer(2),
                sdp: "v=0\r\n".into(),
            },
            ClientMsg::Ice {
                to: peer(2),
                candidate: "candidate:1 1 udp 2130706431 192.168.1.20 50214 typ host".into(),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
            },
            ClientMsg::Ice {
                to: peer(2),
                candidate: String::new(),
                sdp_mid: None,
                sdp_mline_index: None,
            },
            ClientMsg::Chat {
                text: "hello".into(),
            },
            ClientMsg::ScreenRequest,
            ClientMsg::ScreenStop,
            ClientMsg::Ping,
        ]
    }

    fn all_server_msgs() -> Vec<ServerMsg> {
        vec![
            ServerMsg::Welcome {
                you: peer(1),
                peers: vec![PeerInfo {
                    id: peer(2),
                    display_name: name("grace"),
                }],
                history: vec![chat_message()],
                screen_publisher: Some(peer(2)),
            },
            ServerMsg::Welcome {
                you: peer(1),
                peers: vec![],
                history: vec![],
                screen_publisher: None,
            },
            ServerMsg::PeerJoined {
                peer: PeerInfo {
                    id: peer(3),
                    display_name: name("linus"),
                },
            },
            ServerMsg::PeerLeft { peer_id: peer(3) },
            ServerMsg::Offer {
                from: peer(1),
                sdp: "v=0\r\n".into(),
            },
            ServerMsg::Answer {
                from: peer(1),
                sdp: "v=0\r\n".into(),
            },
            ServerMsg::Ice {
                from: peer(1),
                candidate: "candidate:1 1 udp 1 10.0.0.1 9 typ host".into(),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
            },
            ServerMsg::Chat {
                msg: chat_message(),
            },
            ServerMsg::ScreenGranted,
            ServerMsg::ScreenDenied {
                reason: "grace is already sharing".into(),
            },
            ServerMsg::ScreenStarted { publisher: peer(2) },
            ServerMsg::ScreenStopped,
            ServerMsg::Error {
                code: ErrorCode::PeerNotFound,
                message: "no such peer".into(),
            },
            ServerMsg::Pong,
        ]
    }

    #[test]
    fn every_client_msg_roundtrips() {
        for msg in all_client_msgs() {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ClientMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(back, msg, "roundtrip of {json}");
        }
    }

    #[test]
    fn every_server_msg_roundtrips() {
        for msg in all_server_msgs() {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ServerMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(back, msg, "roundtrip of {json}");
        }
    }

    #[test]
    fn tag_field_and_snake_case_names() {
        let tags = |json: String| -> String {
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            v["t"].as_str().unwrap().to_owned()
        };
        let client: Vec<String> = all_client_msgs()
            .into_iter()
            .map(|m| tags(serde_json::to_string(&m).unwrap()))
            .collect();
        assert_eq!(
            client,
            [
                "join",
                "offer",
                "answer",
                "ice",
                "ice",
                "chat",
                "screen_request",
                "screen_stop",
                "ping"
            ]
        );
        let server: Vec<String> = all_server_msgs()
            .into_iter()
            .map(|m| tags(serde_json::to_string(&m).unwrap()))
            .collect();
        assert_eq!(
            server,
            [
                "welcome",
                "welcome",
                "peer_joined",
                "peer_left",
                "offer",
                "answer",
                "ice",
                "chat",
                "screen_granted",
                "screen_denied",
                "screen_started",
                "screen_stopped",
                "error",
                "pong"
            ]
        );
    }

    #[test]
    fn exact_wire_format() {
        assert_eq!(
            serde_json::to_string(&ClientMsg::ScreenRequest).unwrap(),
            r#"{"t":"screen_request"}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientMsg::Join {
                display_name: name("ada")
            })
            .unwrap(),
            r#"{"t":"join","display_name":"ada"}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMsg::PeerLeft { peer_id: peer(3) }).unwrap(),
            r#"{"t":"peer_left","peer_id":"00000000-0000-4000-8000-000000000003"}"#
        );
    }

    #[test]
    fn parses_documented_examples() {
        let msg: ClientMsg = serde_json::from_str(
            r#"{"t":"ice","to":"00000000-0000-4000-8000-000000000002","candidate":"c","sdp_mid":"0","sdp_mline_index":0}"#,
        )
        .unwrap();
        assert!(matches!(msg, ClientMsg::Ice { .. }));
        // Optional fields may be omitted entirely.
        let msg: ClientMsg = serde_json::from_str(
            r#"{"t":"ice","to":"00000000-0000-4000-8000-000000000002","candidate":""}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ClientMsg::Ice {
                to: peer(2),
                candidate: String::new(),
                sdp_mid: None,
                sdp_mline_index: None
            }
        );
        // Unknown fields are ignored.
        let msg: ClientMsg = serde_json::from_str(r#"{"t":"ping","extra":1}"#).unwrap();
        assert_eq!(msg, ClientMsg::Ping);
    }

    #[test]
    fn rejects_unknown_tag_and_invalid_name() {
        assert!(serde_json::from_str::<ClientMsg>(r#"{"t":"nope"}"#).is_err());
        assert!(serde_json::from_str::<ClientMsg>(r#"{"t":"join","display_name":""}"#).is_err());
        assert!(serde_json::from_str::<ClientMsg>(r#"{"display_name":"ada"}"#).is_err());
    }
}
