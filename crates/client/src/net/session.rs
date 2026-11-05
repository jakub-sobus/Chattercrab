//! Pure signaling session logic: turns server messages and media events into effects.
//!
//! No I/O happens here, which keeps the protocol choreography unit testable.

use std::collections::BTreeMap;

use chattercrab_media::{MediaCommand, MediaEvent, OutgoingSignal, ScreenSource};
use chattercrab_protocol::{
    ClientMsg, DisplayName, ErrorCode, PeerId, PeerInfo, RoomId, RoomName, ServerMsg,
    TurnCredentials,
};

use super::{AppEvent, JoinedRoom};

/// Something the backend must do as a consequence of an input.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Send a command to the media engine.
    Media(MediaCommand),
    /// Notify the UI.
    Ui(AppEvent),
    /// Send a message to the signaling server.
    Send(ClientMsg),
    /// The room cannot be used any more; leave it with this notice.
    Fatal(String),
}

/// Per-room signaling state.
#[derive(Debug, Clone)]
pub struct Session {
    room_id: RoomId,
    room_name: RoomName,
    display_name: DisplayName,
    ice: Option<TurnCredentials>,
    me: Option<PeerId>,
    peers: BTreeMap<PeerId, PeerInfo>,
    screen_publisher: Option<PeerId>,
    pending_share: Option<(ScreenSource, u32)>,
    local_sharing: bool,
}

impl Session {
    /// Creates a session for a room we are about to join.
    pub fn new(
        room_id: RoomId,
        room_name: RoomName,
        display_name: DisplayName,
        ice: Option<TurnCredentials>,
    ) -> Self {
        Self {
            room_id,
            room_name,
            display_name,
            ice,
            me: None,
            peers: BTreeMap::new(),
            screen_publisher: None,
            pending_share: None,
            local_sharing: false,
        }
    }

    /// Room id.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Our display name.
    pub fn display_name(&self) -> &DisplayName {
        &self.display_name
    }

    /// Replaces the ICE credentials used for the next `welcome`.
    pub fn set_ice(&mut self, ice: Option<TurnCredentials>) {
        self.ice = ice;
    }

    /// Handles one message from the server.
    pub fn on_server_msg(&mut self, msg: ServerMsg) -> Vec<Effect> {
        match msg {
            ServerMsg::Welcome {
                you,
                peers,
                history,
                screen_publisher,
            } => {
                self.me = Some(you);
                self.peers = peers.iter().map(|p| (p.id, p.clone())).collect();
                self.screen_publisher = screen_publisher;
                let mut effects = vec![Effect::Media(MediaCommand::Configure {
                    me: you,
                    ice: self.ice.clone(),
                })];
                // The newcomer offers to everyone already present.
                effects.extend(peers.iter().map(|p| {
                    Effect::Media(MediaCommand::AddPeer {
                        peer: p.id,
                        initiate: true,
                    })
                }));
                effects.push(Effect::Ui(AppEvent::Joined(JoinedRoom {
                    id: self.room_id.clone(),
                    name: self.room_name.clone(),
                    me: you,
                    display_name: self.display_name.clone(),
                    peers,
                    history,
                    screen_publisher,
                })));
                effects
            }
            ServerMsg::PeerJoined { peer } => {
                self.peers.insert(peer.id, peer.clone());
                vec![
                    Effect::Media(MediaCommand::AddPeer {
                        peer: peer.id,
                        initiate: false,
                    }),
                    Effect::Ui(AppEvent::PeerJoined(peer)),
                ]
            }
            ServerMsg::PeerLeft { peer_id } => {
                self.peers.remove(&peer_id);
                vec![
                    Effect::Media(MediaCommand::RemovePeer { peer: peer_id }),
                    Effect::Ui(AppEvent::PeerLeft(peer_id)),
                ]
            }
            ServerMsg::Offer { from, sdp } => {
                vec![Effect::Media(MediaCommand::RemoteOffer { from, sdp })]
            }
            ServerMsg::Answer { from, sdp } => {
                vec![Effect::Media(MediaCommand::RemoteAnswer { from, sdp })]
            }
            ServerMsg::Ice {
                from,
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => vec![Effect::Media(MediaCommand::RemoteIce {
                from,
                candidate,
                sdp_mid,
                sdp_mline_index,
            })],
            ServerMsg::Chat { msg } => vec![Effect::Ui(AppEvent::Chat(msg))],
            ServerMsg::ScreenGranted => match self.pending_share.take() {
                Some((source, fps)) => {
                    self.local_sharing = true;
                    vec![
                        Effect::Media(MediaCommand::StartScreenShare { source, fps }),
                        Effect::Ui(AppEvent::LocalShare(true)),
                    ]
                }
                // Granted after we changed our mind: give the lock back.
                None if !self.local_sharing => vec![Effect::Send(ClientMsg::ScreenStop)],
                None => Vec::new(),
            },
            ServerMsg::ScreenDenied { reason } => {
                self.pending_share = None;
                vec![Effect::Ui(AppEvent::ScreenDenied(reason))]
            }
            ServerMsg::ScreenStarted { publisher } => {
                self.screen_publisher = Some(publisher);
                vec![Effect::Ui(AppEvent::ScreenStarted(publisher))]
            }
            ServerMsg::ScreenStopped => {
                self.screen_publisher = None;
                let mut effects = Vec::new();
                if self.local_sharing {
                    self.local_sharing = false;
                    effects.push(Effect::Media(MediaCommand::StopScreenShare));
                    effects.push(Effect::Ui(AppEvent::LocalShare(false)));
                }
                effects.push(Effect::Ui(AppEvent::ScreenStopped));
                effects
            }
            ServerMsg::Error { code, message } => match code {
                ErrorCode::RoomNotFound
                | ErrorCode::RoomFull
                | ErrorCode::InvalidDisplayName
                | ErrorCode::JoinTimeout
                | ErrorCode::NotJoined => vec![Effect::Fatal(message)],
                _ => vec![Effect::Ui(AppEvent::Notice(message))],
            },
            ServerMsg::Pong => Vec::new(),
        }
    }

    /// Handles one event from the media engine.
    pub fn on_media_event(&mut self, event: MediaEvent) -> Vec<Effect> {
        match event {
            MediaEvent::Signal(signal) => vec![Effect::Send(match signal {
                OutgoingSignal::Offer { to, sdp } => ClientMsg::Offer { to, sdp },
                OutgoingSignal::Answer { to, sdp } => ClientMsg::Answer { to, sdp },
                OutgoingSignal::Ice {
                    to,
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                } => ClientMsg::Ice {
                    to,
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                },
            })],
            MediaEvent::PeerState { peer, state } => {
                vec![Effect::Ui(AppEvent::PeerState { peer, state })]
            }
            MediaEvent::Levels { local, peers } => {
                vec![Effect::Ui(AppEvent::Levels { local, peers })]
            }
            MediaEvent::RemoteScreenStarted { .. } | MediaEvent::RemoteScreenEnded { .. } => {
                // The server's screen_started/screen_stopped drive the UI.
                Vec::new()
            }
            MediaEvent::LocalScreenStarted => vec![Effect::Ui(AppEvent::LocalShare(true))],
            MediaEvent::LocalScreenStopped => {
                let mut effects = Vec::new();
                if self.local_sharing {
                    // Capture ended on its own (error, window closed): release the lock.
                    self.local_sharing = false;
                    effects.push(Effect::Send(ClientMsg::ScreenStop));
                }
                effects.push(Effect::Ui(AppEvent::LocalShare(false)));
                effects
            }
            MediaEvent::Error(message) => vec![Effect::Ui(AppEvent::Notice(message))],
        }
    }

    /// The user wants to share `source`; asks the server for the lock first.
    pub fn request_share(&mut self, source: ScreenSource, fps: u32) -> Vec<Effect> {
        if self.local_sharing {
            return Vec::new();
        }
        self.pending_share = Some((source, fps));
        vec![Effect::Send(ClientMsg::ScreenRequest)]
    }

    /// The user stops sharing.
    pub fn stop_share(&mut self) -> Vec<Effect> {
        self.pending_share = None;
        if !self.local_sharing {
            return Vec::new();
        }
        self.local_sharing = false;
        vec![
            Effect::Send(ClientMsg::ScreenStop),
            Effect::Media(MediaCommand::StopScreenShare),
            Effect::Ui(AppEvent::LocalShare(false)),
        ]
    }

    /// The signaling connection is gone (drop, leave, or fatal error).
    pub fn on_disconnected(&mut self) -> Vec<Effect> {
        self.me = None;
        self.peers.clear();
        self.screen_publisher = None;
        self.pending_share = None;
        let mut effects = vec![Effect::Media(MediaCommand::RemoveAllPeers)];
        if self.local_sharing {
            // The server releases the lock when we disconnect.
            self.local_sharing = false;
            effects.push(Effect::Media(MediaCommand::StopScreenShare));
            effects.push(Effect::Ui(AppEvent::LocalShare(false)));
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chattercrab_media::ScreenSourceKind;
    use chattercrab_protocol::ChatMessage;

    fn peer(n: u8) -> PeerId {
        format!("00000000-0000-4000-8000-0000000000{n:02x}")
            .parse()
            .unwrap()
    }

    fn info(n: u8) -> PeerInfo {
        PeerInfo {
            id: peer(n),
            display_name: DisplayName::new(&format!("p{n}")).unwrap(),
        }
    }

    fn session() -> Session {
        Session::new(
            RoomId::new("room1").unwrap(),
            RoomName::new("Room").unwrap(),
            DisplayName::new("me").unwrap(),
            None,
        )
    }

    fn source() -> ScreenSource {
        ScreenSource {
            id: 1,
            kind: ScreenSourceKind::Monitor,
            name: "Display 1".into(),
        }
    }

    fn welcome(s: &mut Session, peers: Vec<PeerInfo>) -> Vec<Effect> {
        s.on_server_msg(ServerMsg::Welcome {
            you: peer(1),
            peers,
            history: vec![],
            screen_publisher: None,
        })
    }

    #[test]
    fn welcome_configures_then_offers_to_every_peer() {
        let mut s = session();
        let ice = TurnCredentials {
            username: "u".into(),
            credential: "c".into(),
            ttl_secs: 10,
            urls: vec!["turn:x:3478".into()],
        };
        s.set_ice(Some(ice.clone()));
        let effects = welcome(&mut s, vec![info(2), info(3)]);
        assert!(matches!(
            &effects[0],
            Effect::Media(MediaCommand::Configure { me, ice: Some(i) }) if *me == peer(1) && *i == ice
        ));
        assert!(matches!(
            effects[1],
            Effect::Media(MediaCommand::AddPeer { peer: p, initiate: true }) if p == peer(2)
        ));
        assert!(matches!(
            effects[2],
            Effect::Media(MediaCommand::AddPeer { peer: p, initiate: true }) if p == peer(3)
        ));
        assert!(matches!(&effects[3], Effect::Ui(AppEvent::Joined(j)) if j.peers.len() == 2));
        assert_eq!(effects.len(), 4);
        assert_eq!(s.me, Some(peer(1)));
    }

    #[test]
    fn joined_peer_waits_for_offer_and_left_peer_is_removed() {
        let mut s = session();
        welcome(&mut s, vec![]);
        let effects = s.on_server_msg(ServerMsg::PeerJoined { peer: info(4) });
        assert!(matches!(
            effects[0],
            Effect::Media(MediaCommand::AddPeer { peer: p, initiate: false }) if p == peer(4)
        ));
        let effects = s.on_server_msg(ServerMsg::PeerLeft { peer_id: peer(4) });
        assert!(matches!(
            effects[0],
            Effect::Media(MediaCommand::RemovePeer { peer: p }) if p == peer(4)
        ));
    }

    #[test]
    fn signaling_relayed_both_ways() {
        let mut s = session();
        let effects = s.on_server_msg(ServerMsg::Offer {
            from: peer(2),
            sdp: "o".into(),
        });
        assert!(matches!(
            &effects[0],
            Effect::Media(MediaCommand::RemoteOffer { from, sdp }) if *from == peer(2) && sdp == "o"
        ));
        let effects = s.on_server_msg(ServerMsg::Ice {
            from: peer(2),
            candidate: "c".into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
        });
        assert!(matches!(
            &effects[0],
            Effect::Media(MediaCommand::RemoteIce { .. })
        ));
        let effects = s.on_media_event(MediaEvent::Signal(OutgoingSignal::Answer {
            to: peer(2),
            sdp: "a".into(),
        }));
        assert!(matches!(
            &effects[0],
            Effect::Send(ClientMsg::Answer { to, sdp }) if *to == peer(2) && sdp == "a"
        ));
        let effects = s.on_media_event(MediaEvent::Signal(OutgoingSignal::Ice {
            to: peer(3),
            candidate: String::new(),
            sdp_mid: None,
            sdp_mline_index: None,
        }));
        assert!(matches!(&effects[0], Effect::Send(ClientMsg::Ice { to, .. }) if *to == peer(3)));
    }

    #[test]
    fn chat_is_forwarded_to_ui() {
        let mut s = session();
        let msg = ChatMessage {
            id: 1,
            from: peer(2),
            display_name: DisplayName::new("p2").unwrap(),
            text: "hi".into(),
            ts: 0,
        };
        let effects = s.on_server_msg(ServerMsg::Chat { msg: msg.clone() });
        assert!(matches!(&effects[0], Effect::Ui(AppEvent::Chat(m)) if *m == msg));
    }

    #[test]
    fn screen_share_grant_flow() {
        let mut s = session();
        welcome(&mut s, vec![]);
        let effects = s.request_share(source(), 15);
        assert!(matches!(effects[0], Effect::Send(ClientMsg::ScreenRequest)));
        let effects = s.on_server_msg(ServerMsg::ScreenGranted);
        assert!(matches!(
            &effects[0],
            Effect::Media(MediaCommand::StartScreenShare { source: src, fps: 15 }) if *src == source()
        ));
        assert!(s.local_sharing);
        let effects = s.stop_share();
        assert!(matches!(effects[0], Effect::Send(ClientMsg::ScreenStop)));
        assert!(matches!(
            effects[1],
            Effect::Media(MediaCommand::StopScreenShare)
        ));
        assert!(!s.local_sharing);
        // Stopping again is a no-op.
        assert!(s.stop_share().is_empty());
    }

    #[test]
    fn screen_share_denied_flow() {
        let mut s = session();
        welcome(&mut s, vec![info(2)]);
        s.request_share(source(), 15);
        let effects = s.on_server_msg(ServerMsg::ScreenDenied {
            reason: "p2 is already sharing".into(),
        });
        assert!(matches!(&effects[0], Effect::Ui(AppEvent::ScreenDenied(r)) if r.contains("p2")));
        // A late grant without a pending request is handed back.
        let effects = s.on_server_msg(ServerMsg::ScreenGranted);
        assert!(matches!(effects[0], Effect::Send(ClientMsg::ScreenStop)));
    }

    #[test]
    fn server_stop_and_capture_failure_stop_local_share() {
        let mut s = session();
        welcome(&mut s, vec![]);
        s.request_share(source(), 10);
        s.on_server_msg(ServerMsg::ScreenGranted);
        let effects = s.on_server_msg(ServerMsg::ScreenStopped);
        assert!(matches!(
            effects[0],
            Effect::Media(MediaCommand::StopScreenShare)
        ));
        assert!(!s.local_sharing);

        s.request_share(source(), 10);
        s.on_server_msg(ServerMsg::ScreenGranted);
        let effects = s.on_media_event(MediaEvent::LocalScreenStopped);
        assert!(matches!(effects[0], Effect::Send(ClientMsg::ScreenStop)));
        assert!(!s.local_sharing);
    }

    #[test]
    fn disconnect_tears_down_peers_and_share() {
        let mut s = session();
        welcome(&mut s, vec![info(2)]);
        s.request_share(source(), 10);
        s.on_server_msg(ServerMsg::ScreenGranted);
        let effects = s.on_disconnected();
        assert!(matches!(
            effects[0],
            Effect::Media(MediaCommand::RemoveAllPeers)
        ));
        assert!(matches!(
            effects[1],
            Effect::Media(MediaCommand::StopScreenShare)
        ));
        assert_eq!(s.me, None);
        // A new welcome rebuilds the mesh.
        let effects = welcome(&mut s, vec![info(3)]);
        assert!(matches!(
            effects[1],
            Effect::Media(MediaCommand::AddPeer { peer: p, initiate: true }) if p == peer(3)
        ));
    }

    #[test]
    fn fatal_errors_vs_notices() {
        let mut s = session();
        let effects = s.on_server_msg(ServerMsg::Error {
            code: ErrorCode::RoomFull,
            message: "full".into(),
        });
        assert!(matches!(&effects[0], Effect::Fatal(m) if m == "full"));
        let effects = s.on_server_msg(ServerMsg::Error {
            code: ErrorCode::ChatTooLong,
            message: "long".into(),
        });
        assert!(matches!(&effects[0], Effect::Ui(AppEvent::Notice(m)) if m == "long"));
    }
}
