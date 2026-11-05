//! Network and media runtime: REST, signaling WebSocket, reconnect, and the bridge to
//! the media engine. Runs on its own Tokio runtime thread; the UI talks to it only
//! through [`UiCommand`] and [`AppEvent`] channels.

pub mod backend;
pub mod backoff;
pub mod rest;
pub mod session;
pub mod signaling;

use chattercrab_media::{PeerConnectionState, ScreenSource, VideoFrameQueue};
use chattercrab_protocol::{
    ChatMessage, DisplayName, PeerId, PeerInfo, RoomId, RoomName, RoomSummary,
};

/// Capacity of the UI to backend command channel.
pub const UI_COMMAND_CAPACITY: usize = 64;
/// Capacity of the backend to UI event channel.
pub const APP_EVENT_CAPACITY: usize = 256;

/// Commands from the UI to the backend.
#[derive(Debug, Clone)]
pub enum UiCommand {
    /// Enumerate audio devices.
    ListDevices,
    /// Connect to a signaling server and open the audio devices.
    Connect {
        /// Server base URL (`https://host:port` or `http://host:port`).
        server_url: String,
        /// Display name for joins.
        display_name: DisplayName,
        /// Microphone; `None` = system default.
        input_device: Option<String>,
        /// Speakers; `None` = system default.
        output_device: Option<String>,
        /// Microphone noise suppression.
        noise_suppression: bool,
    },
    /// Forget the server and return to the connect screen.
    Disconnect,
    /// Fetch the room list.
    RefreshRooms,
    /// Create a room and join it.
    CreateRoom(RoomName),
    /// Join an existing room.
    JoinRoom {
        /// Room id.
        id: RoomId,
        /// Room name, for display.
        name: RoomName,
    },
    /// Leave the current room.
    LeaveRoom,
    /// Send a chat message to the room.
    SendChat(String),
    /// Mute or unmute the microphone.
    SetMuted(bool),
    /// Deafen or undeafen playback.
    SetDeafened(bool),
    /// Enable or disable microphone noise suppression.
    SetNoiseSuppression(bool),
    /// Playback volume for one peer (0.0 to 2.0).
    SetPeerVolume {
        /// Remote peer.
        peer: PeerId,
        /// Linear gain.
        volume: f32,
    },
    /// Enumerate monitors and windows.
    ListScreenSources,
    /// Ask the server for the screen lock and publish `source` once granted.
    StartScreenShare {
        /// What to capture.
        source: ScreenSource,
        /// Frame rate.
        fps: u32,
    },
    /// Stop publishing the screen.
    StopScreenShare,
    /// Stop the backend.
    Shutdown,
}

/// State delivered by `welcome`.
#[derive(Debug, Clone)]
pub struct JoinedRoom {
    /// Room id.
    pub id: RoomId,
    /// Room name.
    pub name: RoomName,
    /// Our peer id.
    pub me: PeerId,
    /// Our display name.
    pub display_name: DisplayName,
    /// Other members.
    pub peers: Vec<PeerInfo>,
    /// Chat history, oldest first.
    pub history: Vec<ChatMessage>,
    /// Current screen publisher.
    pub screen_publisher: Option<PeerId>,
}

/// Events from the backend to the UI.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Available audio devices.
    Devices {
        /// Input device names.
        inputs: Vec<String>,
        /// Output device names.
        outputs: Vec<String>,
    },
    /// Connected to the server; media engine running.
    Connected {
        /// Initial room list.
        rooms: Vec<RoomSummary>,
        /// Decoded remote screen-share frames.
        video: VideoFrameQueue,
    },
    /// Connecting failed.
    ConnectFailed(String),
    /// Updated room list.
    Rooms(Vec<RoomSummary>),
    /// Joined (or re-joined after a reconnect) a room.
    Joined(JoinedRoom),
    /// Signaling dropped; retrying after a delay.
    Reconnecting {
        /// Retry number, starting at 1.
        attempt: u32,
        /// Delay before the retry.
        delay_secs: u64,
    },
    /// The room is gone or refused us; back to the lobby.
    RoomClosed(String),
    /// We left the room on request.
    Left,
    /// A member joined.
    PeerJoined(PeerInfo),
    /// A member left.
    PeerLeft(PeerId),
    /// A chat message (including our own echo).
    Chat(ChatMessage),
    /// A member started sharing.
    ScreenStarted(PeerId),
    /// The screen share ended.
    ScreenStopped,
    /// Our screen request was refused.
    ScreenDenied(String),
    /// Whether we are publishing our screen.
    LocalShare(bool),
    /// Capturable monitors and windows.
    ScreenSources(Vec<ScreenSource>),
    /// Connection state to one peer.
    PeerState {
        /// Remote peer.
        peer: PeerId,
        /// New state.
        state: PeerConnectionState,
    },
    /// Audio levels.
    Levels {
        /// Local microphone level.
        local: f32,
        /// Per remote peer.
        peers: Vec<(PeerId, f32)>,
    },
    /// A message to show in the status line.
    Notice(String),
}
