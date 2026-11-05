# chattercrab-media API changes

Changes to the public contract in `src/lib.rs` relative to the stub committed in
`e2cb69c` ("define media engine API contract with stub engine"). All are additive.

| Change | Kind | Notes |
|--------|------|-------|
| `MediaConfig::noise_suppression: bool` | new field | Default `false`. Enable RNNoise-based noise suppression on the microphone input. `MediaConfig` still derives `Default`, so `MediaConfig { input_device, output_device, ..Default::default() }` keeps compiling; exhaustive struct literals need the new field. |
| `MediaCommand::SetNoiseSuppression(bool)` | new variant | Enable or disable microphone noise suppression live. Read by the encoder thread through an `AtomicBool`. |
| `pub mod audio`, `jitter`, `mixer`, `peer`, `screen`, `denoise` | new modules | Building blocks used by the engine, the loopback example and tests. The client should keep using `MediaEngine`, `MediaCommand` and `MediaEvent` only. |

Behavior notes for the client (no type changes):

* `MediaEngine::start` no longer fails when no audio device exists. It returns `Ok`, and
  device problems arrive as `MediaEvent::Error` (the engine keeps running, for example
  receive-only without a microphone). It returns `Err` only when called outside a Tokio
  runtime.
* `MediaCommand::RemoveAllPeers` also stops a local screen share (emitting
  `LocalScreenStopped`), because the server releases the screen lock when the socket drops.
* `MediaEvent::Levels` is emitted only between `Configure` and `RemoveAllPeers`, and is
  dropped rather than queued when the event channel is full.
* `MediaEvent::RemoteScreenEnded` fires when the remote video track ends. The client
  should also hide the stream on the server's `screen_stopped`.
