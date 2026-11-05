//! iced application: state, messages, update and subscriptions (Elm architecture).
//!
//! `update` never blocks: every side effect is a [`UiCommand`] sent with `try_send`
//! to the backend running on its own Tokio runtime thread.

pub mod room_model;
mod view;

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chattercrab_media::{RgbaFrame, ScreenSource, VideoFrameQueue};
use chattercrab_protocol::{DisplayName, PeerId, RoomId, RoomName, RoomSummary};
use iced::futures::stream;
use iced::widget::image;
use iced::{Subscription, Task, Theme, keyboard};
use tokio::sync::mpsc;

use crate::config::{Config, SCREEN_FPS_RANGE, ThemeChoice};
use crate::net::{AppEvent, UiCommand};
use room_model::RoomModel;

/// Label for the "use the system default device" choice.
pub const SYSTEM_DEFAULT: &str = "System default";

/// Which screen is shown.
#[derive(Debug)]
pub enum Screen {
    /// Server, name and device selection.
    Connect {
        /// A connect attempt is in flight.
        connecting: bool,
    },
    /// Room list.
    Lobby(Lobby),
    /// Inside a room.
    Room(Box<RoomScreen>),
}

/// Lobby screen state.
#[derive(Debug, Default)]
pub struct Lobby {
    /// Rooms on the server.
    pub rooms: Vec<RoomSummary>,
    /// Name typed for a new room.
    pub new_room: String,
}

/// Screen-share source picker state.
#[derive(Debug, Clone)]
pub struct SharePicker {
    /// Available sources (empty until listed).
    pub sources: Vec<SourceOption>,
    /// Chosen source.
    pub selected: Option<SourceOption>,
    /// Chosen frame rate.
    pub fps: u32,
}

/// A [`ScreenSource`] with a display label, for pick lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOption(pub ScreenSource);

impl std::fmt::Display for SourceOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.0.kind {
            chattercrab_media::ScreenSourceKind::Monitor => "Screen",
            chattercrab_media::ScreenSourceKind::Window => "Window",
        };
        write!(f, "{kind}: {}", self.0.name)
    }
}

/// Room screen state.
#[derive(Debug)]
pub struct RoomScreen {
    /// Room model fed by backend events.
    pub model: RoomModel,
    /// Chat input text.
    pub chat_input: String,
    /// Open share picker, if any.
    pub picker: Option<SharePicker>,
    /// Latest remote screen frame.
    pub frame: Option<image::Handle>,
}

/// Mutable UI state.
#[derive(Debug)]
pub struct UiState {
    /// Current screen.
    pub screen: Screen,
    /// Input device names.
    pub inputs: Vec<String>,
    /// Output device names.
    pub outputs: Vec<String>,
    /// Status line text.
    pub notice: Option<String>,
    /// Remote video frames, available once connected.
    pub video: Option<VideoFrameQueue>,
}

/// UI messages.
#[derive(Debug, Clone)]
pub enum Message {
    /// Event from the backend.
    Backend(AppEvent),
    /// A decoded remote screen frame.
    VideoFrame(RgbaFrame),
    /// Server URL edited.
    ServerUrlChanged(String),
    /// Display name edited.
    DisplayNameChanged(String),
    /// Microphone chosen.
    InputSelected(String),
    /// Speakers chosen.
    OutputSelected(String),
    /// Dark theme toggled.
    DarkTheme(bool),
    /// Noise suppression toggled (connect screen or room bar).
    NoiseSuppression(bool),
    /// Re-list audio devices.
    RefreshDevices,
    /// Connect button.
    Connect,
    /// Back to the connect screen.
    Disconnect,
    /// Reload the room list.
    RefreshRooms,
    /// New room name edited.
    NewRoomChanged(String),
    /// Create button.
    CreateRoom,
    /// Join a room.
    JoinRoom(RoomId, RoomName),
    /// Chat input edited.
    ChatChanged(String),
    /// Enter in the chat input.
    SendChat,
    /// Toggle microphone mute (button or Ctrl+M).
    ToggleMute,
    /// Toggle deafen (button or Ctrl+D).
    ToggleDeafen,
    /// Open the share picker.
    OpenSharePicker,
    /// Source picked.
    ShareSourceSelected(SourceOption),
    /// Frame rate picked.
    ShareFpsSelected(u32),
    /// Start sharing the picked source.
    StartShare,
    /// Close the picker.
    CancelShare,
    /// Stop sharing.
    StopShare,
    /// Leave the room.
    Leave,
    /// Volume slider moved.
    PeerVolumeChanged(PeerId, f32),
    /// Volume slider released.
    PeerVolumeCommitted(PeerId),
    /// Local mute of one peer toggled.
    PeerMuteToggled(PeerId, bool),
    /// Clear the status line.
    DismissNotice,
}

/// The application: UI state, persisted config, and the command channel to the backend.
pub struct App {
    /// UI state.
    pub state: UiState,
    /// Persisted settings (also the connect form).
    pub config: Config,
    /// Commands to the backend.
    pub net_tx: mpsc::Sender<UiCommand>,
    bridge: EventBridge,
    config_path: Option<PathBuf>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").field("state", &self.state).finish()
    }
}

/// Shared slot holding the backend event receiver until the subscription takes it.
pub type EventSlot = Arc<Mutex<Option<mpsc::Receiver<AppEvent>>>>;

/// Wraps the backend event receiver so a subscription can take ownership of it once.
#[derive(Clone)]
struct EventBridge(EventSlot);

impl Hash for EventBridge {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "chattercrab-backend-events".hash(state);
    }
}

/// Identity wrapper for the video subscription.
struct VideoSource(VideoFrameQueue);

impl Hash for VideoSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "chattercrab-video".hash(state);
    }
}

impl App {
    /// Creates the app. The backend must already be running behind `net_tx`; its
    /// event receiver sits in `events` until the subscription takes it.
    pub fn new(
        config: Config,
        config_path: Option<PathBuf>,
        net_tx: mpsc::Sender<UiCommand>,
        events: EventSlot,
    ) -> Self {
        let app = Self {
            state: UiState {
                screen: Screen::Connect { connecting: false },
                inputs: Vec::new(),
                outputs: Vec::new(),
                notice: None,
                video: None,
            },
            config,
            net_tx,
            bridge: EventBridge(events),
            config_path,
        };
        app.send(UiCommand::ListDevices);
        app
    }

    /// Window title.
    pub fn title(&self) -> String {
        match &self.state.screen {
            Screen::Room(room) => format!("{} - Chattercrab", room.model.name),
            _ => "Chattercrab".to_owned(),
        }
    }

    /// Light or dark theme.
    pub fn theme(&self) -> Theme {
        match self.config.theme {
            ThemeChoice::Light => Theme::Light,
            ThemeChoice::Dark => Theme::Dark,
        }
    }

    fn send(&self, cmd: UiCommand) {
        if let Err(e) = self.net_tx.try_send(cmd) {
            tracing::warn!(error = %e, "backend command dropped");
        }
    }

    fn save_config(&mut self) {
        if let Some(path) = &self.config_path
            && let Err(e) = self.config.save(path)
        {
            self.state.notice = Some(format!("Could not save settings: {e:#}"));
        }
    }

    fn room_mut(&mut self) -> Option<&mut RoomScreen> {
        match &mut self.state.screen {
            Screen::Room(room) => Some(room),
            _ => None,
        }
    }

    /// Elm update.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Backend(event) => self.on_backend(event),
            Message::VideoFrame(frame) => {
                if let Some(room) = self.room_mut()
                    && room.model.remote_share_active()
                {
                    let pixels = bytes::Bytes::from_owner(frame.pixels);
                    room.frame = Some(image::Handle::from_rgba(frame.width, frame.height, pixels));
                }
            }
            Message::ServerUrlChanged(url) => self.config.server_url = url,
            Message::DisplayNameChanged(name) => self.config.display_name = name,
            Message::InputSelected(name) => {
                self.config.input_device = (name != SYSTEM_DEFAULT).then_some(name);
            }
            Message::OutputSelected(name) => {
                self.config.output_device = (name != SYSTEM_DEFAULT).then_some(name);
            }
            Message::DarkTheme(dark) => {
                self.config.theme = if dark {
                    ThemeChoice::Dark
                } else {
                    ThemeChoice::Light
                };
                self.save_config();
            }
            Message::NoiseSuppression(on) => {
                self.config.noise_suppression = on;
                self.save_config();
                // Applied live once the engine runs; otherwise passed on connect.
                if !matches!(self.state.screen, Screen::Connect { .. }) {
                    self.send(UiCommand::SetNoiseSuppression(on));
                }
            }
            Message::RefreshDevices => self.send(UiCommand::ListDevices),
            Message::Connect => self.connect(),
            Message::Disconnect => {
                self.send(UiCommand::Disconnect);
                self.state.screen = Screen::Connect { connecting: false };
            }
            Message::RefreshRooms => self.send(UiCommand::RefreshRooms),
            Message::NewRoomChanged(name) => {
                if let Screen::Lobby(lobby) = &mut self.state.screen {
                    lobby.new_room = name;
                }
            }
            Message::CreateRoom => {
                if let Screen::Lobby(lobby) = &mut self.state.screen {
                    match RoomName::new(&lobby.new_room) {
                        Ok(name) => {
                            lobby.new_room.clear();
                            // The room screen opens when `welcome` arrives.
                            self.send(UiCommand::CreateRoom(name));
                        }
                        Err(e) => self.state.notice = Some(e.to_string()),
                    }
                }
            }
            Message::JoinRoom(id, name) => {
                self.enter_room(id.clone(), name.clone());
                self.send(UiCommand::JoinRoom { id, name });
            }
            Message::ChatChanged(text) => {
                if let Some(room) = self.room_mut() {
                    room.chat_input = text;
                }
            }
            Message::SendChat => {
                let text = self
                    .room_mut()
                    .map(|room| std::mem::take(&mut room.chat_input));
                if let Some(text) = text {
                    match chattercrab_protocol::validate_chat_text(&text) {
                        Ok(valid) => self.send(UiCommand::SendChat(valid.to_owned())),
                        Err(_) if text.trim().is_empty() => {}
                        Err(e) => {
                            if let Some(room) = self.room_mut() {
                                room.chat_input = text;
                            }
                            self.state.notice = Some(e.to_string());
                        }
                    }
                }
            }
            Message::ToggleMute => {
                if let Some(room) = self.room_mut() {
                    room.model.muted = !room.model.muted;
                    let muted = room.model.muted;
                    self.send(UiCommand::SetMuted(muted));
                }
            }
            Message::ToggleDeafen => {
                if let Some(room) = self.room_mut() {
                    room.model.deafened = !room.model.deafened;
                    let deaf = room.model.deafened;
                    self.send(UiCommand::SetDeafened(deaf));
                }
            }
            Message::OpenSharePicker => {
                let fps = self.config.screen_fps;
                if let Some(room) = self.room_mut() {
                    room.picker = Some(SharePicker {
                        sources: Vec::new(),
                        selected: None,
                        fps,
                    });
                    self.send(UiCommand::ListScreenSources);
                }
            }
            Message::ShareSourceSelected(source) => {
                if let Some(picker) = self.room_mut().and_then(|r| r.picker.as_mut()) {
                    picker.selected = Some(source);
                }
            }
            Message::ShareFpsSelected(fps) => {
                if let Some(picker) = self.room_mut().and_then(|r| r.picker.as_mut()) {
                    picker.fps = fps.clamp(*SCREEN_FPS_RANGE.start(), *SCREEN_FPS_RANGE.end());
                }
            }
            Message::StartShare => {
                let picked = self
                    .room_mut()
                    .and_then(|r| r.picker.take())
                    .and_then(|p| p.selected.map(|s| (s.0, p.fps)));
                if let Some((source, fps)) = picked {
                    self.config.screen_fps = fps;
                    self.save_config();
                    self.send(UiCommand::StartScreenShare { source, fps });
                }
            }
            Message::CancelShare => {
                if let Some(room) = self.room_mut() {
                    room.picker = None;
                }
            }
            Message::StopShare => self.send(UiCommand::StopScreenShare),
            Message::Leave => {
                self.send(UiCommand::LeaveRoom);
                self.state.screen = Screen::Lobby(Lobby::default());
            }
            Message::PeerVolumeChanged(peer, volume) => {
                if let Some(m) = self.room_mut().and_then(|r| r.model.member_mut(peer)) {
                    m.volume = volume.clamp(0.0, 2.0);
                }
            }
            Message::PeerVolumeCommitted(peer) => self.push_peer_volume(peer),
            Message::PeerMuteToggled(peer, muted) => {
                if let Some(m) = self.room_mut().and_then(|r| r.model.member_mut(peer)) {
                    m.muted = muted;
                }
                self.push_peer_volume(peer);
            }
            Message::DismissNotice => self.state.notice = None,
        }
        Task::none()
    }

    fn push_peer_volume(&mut self, peer: PeerId) {
        let volume = self
            .room_mut()
            .and_then(|r| r.model.member_mut(peer))
            .map(|m| m.effective_volume());
        if let Some(volume) = volume {
            self.send(UiCommand::SetPeerVolume { peer, volume });
        }
    }

    fn connect(&mut self) {
        let name = match DisplayName::new(&self.config.display_name) {
            Ok(name) => name,
            Err(e) => {
                self.state.notice = Some(e.to_string());
                return;
            }
        };
        self.config.display_name = name.to_string();
        self.save_config();
        self.state.notice = None;
        self.state.screen = Screen::Connect { connecting: true };
        self.send(UiCommand::Connect {
            server_url: self.config.server_url.clone(),
            display_name: name,
            input_device: self.config.input_device.clone(),
            output_device: self.config.output_device.clone(),
            noise_suppression: self.config.noise_suppression,
        });
    }

    fn enter_room(&mut self, id: RoomId, name: RoomName) {
        self.state.notice = None;
        self.state.screen = Screen::Room(Box::new(RoomScreen {
            model: RoomModel::joining(id, name, self.config.display_name.clone()),
            chat_input: String::new(),
            picker: None,
            frame: None,
        }));
    }

    fn on_backend(&mut self, event: AppEvent) {
        match event {
            AppEvent::Devices { inputs, outputs } => {
                self.state.inputs = inputs;
                self.state.outputs = outputs;
            }
            AppEvent::Connected { rooms, video } => {
                self.state.video = Some(video);
                self.state.screen = Screen::Lobby(Lobby {
                    rooms,
                    new_room: String::new(),
                });
            }
            AppEvent::ConnectFailed(why) => {
                self.state.notice = Some(why);
                self.state.screen = Screen::Connect { connecting: false };
            }
            AppEvent::Rooms(rooms) => {
                if let Screen::Lobby(lobby) = &mut self.state.screen {
                    lobby.rooms = rooms;
                }
            }
            AppEvent::RoomClosed(why) => {
                self.state.notice = Some(why);
                self.state.screen = Screen::Lobby(Lobby::default());
            }
            AppEvent::Left => {
                if !matches!(self.state.screen, Screen::Lobby(_)) {
                    self.state.screen = Screen::Lobby(Lobby::default());
                }
            }
            AppEvent::ScreenSources(sources) => {
                if let Some(picker) = self.room_mut().and_then(|r| r.picker.as_mut()) {
                    picker.sources = sources.into_iter().map(SourceOption).collect();
                    picker.selected = picker.sources.first().cloned();
                }
            }
            AppEvent::ScreenDenied(reason) => {
                self.state.notice = Some(format!("Screen share denied: {reason}"));
            }
            AppEvent::Notice(text) => self.state.notice = Some(text),
            AppEvent::Joined(joined) => {
                if self.room_mut().is_none() {
                    self.enter_room(joined.id.clone(), joined.name.clone());
                }
                if let Some(room) = self.room_mut() {
                    room.model.apply_joined(joined);
                    room.frame = None;
                }
            }
            other => {
                if let Some(room) = self.room_mut() {
                    room.model.apply(&other);
                    if !room.model.remote_share_active() {
                        room.frame = None;
                    }
                }
            }
        }
    }

    /// Backend events, keyboard shortcuts, and remote video frames while a share is shown.
    pub fn subscription(&self) -> Subscription<Message> {
        let backend = Subscription::run_with(self.bridge.clone(), |bridge| {
            let rx = bridge.0.lock().ok().and_then(|mut slot| slot.take());
            stream::unfold(rx, |rx| async move {
                let mut rx = rx?;
                let event = rx.recv().await?;
                Some((Message::Backend(event), Some(rx)))
            })
        });
        let keys = iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if modifiers.command() =>
            {
                match key.as_ref() {
                    keyboard::Key::Character("m") => Some(Message::ToggleMute),
                    keyboard::Key::Character("d") => Some(Message::ToggleDeafen),
                    _ => None,
                }
            }
            _ => None,
        });
        let mut subs = vec![backend, keys];
        if let (Screen::Room(room), Some(video)) = (&self.state.screen, &self.state.video)
            && room.model.remote_share_active()
        {
            subs.push(Subscription::run_with(
                VideoSource(video.clone()),
                |source| {
                    stream::unfold(source.0.clone(), |queue| async move {
                        let frame = queue.next().await;
                        Some((Message::VideoFrame(frame), queue))
                    })
                },
            ));
        }
        Subscription::batch(subs)
    }

    /// Elm view.
    pub fn view(&self) -> iced::Element<'_, Message> {
        view::view(self)
    }
}
