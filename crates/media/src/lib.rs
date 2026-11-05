//! Client-side media engine for Chattercrab.
//!
//! The GUI never touches WebRTC or audio devices directly. It starts a [`MediaEngine`],
//! sends [`MediaCommand`]s and reacts to [`MediaEvent`]s, all over bounded
//! `tokio::sync::mpsc` channels. Decoded remote screen-share frames are delivered
//! through a [`VideoFrameQueue`] (capacity 2, oldest frame dropped when full).
//!
//! The types in this file are the public API contract shared by the media engine and
//! the client (additive changes are recorded in `API_CHANGES.md`). The building blocks
//! are public modules too: [`audio`], [`jitter`], [`mixer`], [`screen`], [`peer`] and
//! [`denoise`].
//!
//! Threading: the engine is one Tokio task; capture callbacks, the Opus encoder, the
//! mixer (real-time priority), screen capture/encode and H.264 decode each run on their
//! own OS thread. See `crates/media/README.md`.

pub mod audio;
pub mod denoise;
mod engine;
pub mod jitter;
pub mod mixer;
pub mod peer;
pub mod screen;
mod video_queue;

pub use video_queue::{RgbaFrame, VideoFrameQueue};

use chattercrab_protocol::{PeerId, TurnCredentials};
use tokio::sync::mpsc;

/// Capacity of the command channel into the engine.
pub const COMMAND_CHANNEL_CAPACITY: usize = 64;
/// Capacity of the event channel out of the engine.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Errors returned by the media engine API.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// No audio device with the requested name exists.
    #[error("audio device not found: {0}")]
    DeviceNotFound(String),
    /// The audio backend failed.
    #[error("audio backend error: {0}")]
    Audio(String),
    /// Screen capture failed.
    #[error("screen capture error: {0}")]
    Screen(String),
    /// WebRTC failure.
    #[error("webrtc error: {0}")]
    WebRtc(String),
    /// Codec initialisation or coding failure.
    #[error("codec error: {0}")]
    Codec(String),
    /// The engine has shut down.
    #[error("media engine is not running")]
    Stopped,
}

/// Initial engine configuration.
#[derive(Debug, Clone, Default)]
pub struct MediaConfig {
    /// Input device name; `None` selects the system default.
    pub input_device: Option<String>,
    /// Output device name; `None` selects the system default.
    pub output_device: Option<String>,
    /// Enable RNNoise-based noise suppression on the microphone input.
    pub noise_suppression: bool,
}

/// What kind of surface a [`ScreenSource`] captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenSourceKind {
    /// A whole monitor.
    Monitor,
    /// A single window.
    Window,
}

/// A capturable monitor or window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSource {
    /// Backend-specific identifier, stable for the lifetime of the source.
    pub id: u32,
    /// Monitor or window.
    pub kind: ScreenSourceKind,
    /// Human-readable label for the UI.
    pub name: String,
}

/// State of the connection to one remote peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerConnectionState {
    /// Negotiating or gathering candidates.
    Connecting,
    /// Media is flowing.
    Connected,
    /// Temporarily lost connectivity; ICE may recover.
    Disconnected,
    /// ICE failed.
    Failed,
    /// Closed locally or remotely.
    Closed,
}

/// Signaling message the engine wants delivered to a remote peer via the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutgoingSignal {
    /// SDP offer.
    Offer {
        /// Recipient.
        to: PeerId,
        /// Session description.
        sdp: String,
    },
    /// SDP answer.
    Answer {
        /// Recipient.
        to: PeerId,
        /// Session description.
        sdp: String,
    },
    /// Trickle ICE candidate; empty `candidate` means end-of-candidates.
    Ice {
        /// Recipient.
        to: PeerId,
        /// Candidate attribute.
        candidate: String,
        /// Media stream identification tag.
        sdp_mid: Option<String>,
        /// Index of the m-line.
        sdp_mline_index: Option<u16>,
    },
}

/// Commands accepted by the engine.
#[derive(Debug, Clone)]
pub enum MediaCommand {
    /// Set the local peer id (from `welcome`) and the ICE servers to use for every
    /// peer connection created afterwards. `None` means host candidates only.
    Configure {
        /// Our own peer id, used for perfect-negotiation politeness.
        me: PeerId,
        /// TURN/STUN credentials from `GET /turn-credentials`.
        ice: Option<TurnCredentials>,
    },
    /// Create a peer connection to `peer`. If `initiate` is true the engine sends the
    /// first offer (the newcomer does this for every peer listed in `welcome`);
    /// otherwise it waits for the remote offer (peers learned from `peer_joined`).
    AddPeer {
        /// Remote peer.
        peer: PeerId,
        /// Whether we send the initial offer.
        initiate: bool,
    },
    /// Close and forget the connection to `peer`.
    RemovePeer {
        /// Remote peer.
        peer: PeerId,
    },
    /// Close every peer connection (leave room or signaling reconnect). Audio devices
    /// stay open.
    RemoveAllPeers,
    /// Remote SDP offer relayed by the server.
    RemoteOffer {
        /// Sender.
        from: PeerId,
        /// Session description.
        sdp: String,
    },
    /// Remote SDP answer relayed by the server.
    RemoteAnswer {
        /// Sender.
        from: PeerId,
        /// Session description.
        sdp: String,
    },
    /// Remote ICE candidate relayed by the server.
    RemoteIce {
        /// Sender.
        from: PeerId,
        /// Candidate attribute; empty means end-of-candidates.
        candidate: String,
        /// Media stream identification tag.
        sdp_mid: Option<String>,
        /// Index of the m-line.
        sdp_mline_index: Option<u16>,
    },
    /// Mute or unmute the microphone (the stream keeps running and sends silence).
    SetMuted(bool),
    /// Deafen or undeafen playback.
    SetDeafened(bool),
    /// Enable or disable microphone noise suppression.
    SetNoiseSuppression(bool),
    /// Per-peer playback volume, clamped to 0.0..=2.0.
    SetPeerVolume {
        /// Remote peer.
        peer: PeerId,
        /// Linear gain.
        volume: f32,
    },
    /// Enable or disable microphone noise suppression.
    SetNoiseSuppression(bool),
    /// Switch the input device (`None` = system default).
    SelectInput(Option<String>),
    /// Switch the output device (`None` = system default).
    SelectOutput(Option<String>),
    /// Start publishing a screen share. Only send after the server granted the lock.
    StartScreenShare {
        /// What to capture.
        source: ScreenSource,
        /// Target frame rate, clamped to 5..=30.
        fps: u32,
    },
    /// Stop publishing the screen share.
    StopScreenShare,
    /// Stop everything and close the event channel.
    Shutdown,
}

/// Events emitted by the engine.
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// Deliver this message to the server.
    Signal(OutgoingSignal),
    /// Connection state of one peer changed.
    PeerState {
        /// Remote peer.
        peer: PeerId,
        /// New state.
        state: PeerConnectionState,
    },
    /// Audio levels (RMS, 0.0..=1.0), emitted about every 100 ms while in a room.
    Levels {
        /// Local microphone level (0.0 while muted).
        local: f32,
        /// Per remote peer.
        peers: Vec<(PeerId, f32)>,
    },
    /// A remote screen share started arriving; frames follow on the [`VideoFrameQueue`].
    RemoteScreenStarted {
        /// Publisher.
        peer: PeerId,
    },
    /// The remote screen share track ended.
    RemoteScreenEnded {
        /// Publisher.
        peer: PeerId,
    },
    /// Local screen publishing started successfully.
    LocalScreenStarted,
    /// Local screen publishing stopped (by command or by error).
    LocalScreenStopped,
    /// A non-fatal error to show in the UI.
    Error(String),
}

/// Handle to a running media engine.
#[derive(Debug)]
pub struct MediaEngine {
    /// Command sender.
    pub commands: mpsc::Sender<MediaCommand>,
    /// Event receiver.
    pub events: mpsc::Receiver<MediaEvent>,
    /// Decoded remote screen-share frames.
    pub video: VideoFrameQueue,
}

impl MediaEngine {
    /// Starts the engine on the current Tokio runtime. The engine opens the audio devices
    /// in the background; failures are reported as [`MediaEvent::Error`] and the engine
    /// keeps running (for example without a microphone).
    ///
    /// Must be called from within a Tokio runtime.
    pub fn start(config: MediaConfig) -> Result<Self, MediaError> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| MediaError::Audio("MediaEngine::start needs a Tokio runtime".into()))?;
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel::<MediaEvent>(EVENT_CHANNEL_CAPACITY);
        let video = VideoFrameQueue::new();
        handle.spawn(engine::run(config, cmd_rx, event_tx, video.clone()));
        Ok(Self {
            commands: cmd_tx,
            events: event_rx,
            video,
        })
    }
}

/// Names of the available audio input devices.
pub fn list_input_devices() -> Result<Vec<String>, MediaError> {
    audio::input_device_names()
}

/// Names of the available audio output devices.
pub fn list_output_devices() -> Result<Vec<String>, MediaError> {
    audio::output_device_names()
}

/// Monitors and windows that can be shared.
pub fn list_screen_sources() -> Result<Vec<ScreenSource>, MediaError> {
    screen::list_sources()
}
