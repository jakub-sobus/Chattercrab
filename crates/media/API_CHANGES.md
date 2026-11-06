# chattercrab-media API changes

Changes to the public contract in `src/lib.rs` relative to the stub committed in
`e2cb69c` ("define media engine API contract with stub engine"). Rows above the ICE change are additive.

| Change                                                          | Kind         | Notes                                                                                                                                                                                                                                                          |
| --------------------------------------------------------------- | ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `MediaCommand::StartScreenShare.scale: ScreenScale`             | new field    | `1440p` fits inside 2560×1440 (default; older `full`, `half`, and `2160p` values load as this). `1080p` fits inside 1920×1080. Frames are never scaled up. Frame rate clamp is 5..=60. The share picker pairs 1440p with 60 fps and 1080p with 30 fps. Windows encodes with the GPU's H.264 encoder when one is registered. |
| `MediaConfig::noise_suppression: bool`                          | new field    | Default `false`. Enable RNNoise-based noise suppression on the microphone input. `MediaConfig` still derives `Default`, so `MediaConfig { input_device, output_device, ..Default::default() }` keeps compiling; exhaustive struct literals need the new field. |
| `MediaCommand::SetNoiseSuppression(bool)`                       | new variant  | Enable or disable microphone noise suppression live. Read by the encoder thread through an `AtomicBool`.                                                                                                                                                       |
| `MediaDiagnostics { test_tone: Option<f32> }`                   | new struct   | Diagnostics only. `test_tone` replaces the microphone with a sine tone (3 Hz speech-like envelope) that goes through the normal encoder thread. Kept out of `MediaConfig` so the client's exhaustive `MediaConfig` literal keeps compiling.                    |
| `MediaEngine::start_with_diagnostics(config, diagnostics)`      | new function | `start(config)` is `start_with_diagnostics(config, MediaDiagnostics::default())`. Used by `examples/signal_call.rs` and `tests/two_process.rs`.                                                                                                                |
| `pub mod audio`, `jitter`, `mixer`, `peer`, `screen`, `denoise` | new modules  | Building blocks used by the engine, the loopback example and tests. The client should keep using `MediaEngine`, `MediaCommand` and `MediaEvent` only.                                                                                                          |
| `MediaCommand::Configure.ice` is `Option<IceServers>`           | breaking     | Was `Option<TurnCredentials>`. STUN servers from `GET /ice-servers` (urls only). `None` or empty `urls` means host candidates only.                                                                                                                            |

Behavior notes for the client (no type changes):

- `MediaEngine::start` no longer fails when no audio device exists. It returns `Ok`, and
  device problems arrive as `MediaEvent::Error` (the engine keeps running, for example
  receive-only without a microphone). It returns `Err` only when called outside a Tokio
  runtime.
- `MediaCommand::RemoveAllPeers` also stops a local screen share (emitting
  `LocalScreenStopped`), because the server releases the screen lock when the socket drops.
- `MediaEvent::Levels` is emitted only between `Configure` and `RemoveAllPeers`, and is
  dropped rather than queued when the event channel is full.
- Local audio and video are sent to a peer only after its connection reached
  `Connected` (and while it is transiently `Disconnected`), not during negotiation.
- `MediaEvent::RemoteScreenEnded` fires when the remote video track ends. The client
  should also hide the stream on the server's `screen_stopped`.
