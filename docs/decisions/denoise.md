# Decisions: microphone noise suppression

## ADR-500: RNNoise via `nnnoiseless` for optional microphone noise suppression

* Context: users asked for an optional "Noise suppression" checkbox that removes
  background noise (fans, hiss, rumble, keyboard) and keeps the speaker's voice,
  without much CPU. The project is Rust-only and prefers pure-Rust or vendored builds
  for cross compilation (musl server, Linux and windows-gnu clients).
* Decision: use `nnnoiseless` 0.5.2 (BSD-3-Clause), a pure-Rust port of Xiph's RNNoise,
  with `default-features = false` (no CLI, no `dasp`). It is wrapped in
  `chattercrab_media::denoise::Denoiser`, which processes one 20 ms, 48 kHz mono frame
  in place, between resampling and Opus encoding in the encoder thread.
* Why RNNoise:
  * Pure Rust, no C or C++ toolchain, so it cross-builds wherever the rest of the
    workspace does.
  * Native 48 kHz with 10 ms blocks: one 20 ms Opus frame is exactly two blocks, so no
    resampling or buffering is needed.
  * Low cost: about 80 microseconds per 20 ms frame in release builds on the
    development machine (about 0.4% of one core), and a model of about 88 KB of
    static weights compiled into the binary.
  * A trained speech/noise model: measured 19 to 53 dB of attenuation on fan-like and
    rumble-like noise, about 37 dB on hiss after speech, while a voiced test signal
    keeps its energy within about 1 dB.
* Alternatives rejected:
  * WebRTC Audio Processing Module (noise suppression, AGC, AEC): better feature set,
    but a large C++ build (Meson or vendored sources), which complicates the musl and
    windows-gnu cross builds and breaks the pure-Rust preference.
  * Spectral subtraction or Wiener filtering written by hand: no dependency, but
    noticeably lower quality, with "musical noise" artifacts and poor handling of
    non-stationary noise such as keyboard clicks.
* Consequences:
  * Enabling suppression adds 10 ms of latency (RNNoise overlap-add delay). Capture to
    encode stays within the 25 ms budget: one 20 ms frame of buffering plus the
    denoiser's sub-millisecond processing; the 10 ms is algorithmic delay in the
    signal, not processing time.
  * Switching on repeats 10 ms of audio. The first block after a start or reset is
    passed through dry and the second is an equal-power crossfade into the denoised
    signal, so the splice is smooth.
  * Flat white noise with no speech ever heard is only attenuated by 1 to 2 dB
    (broadband flat noise looks partly speech-like to the network). Real background
    noise is colored and is removed strongly, as is white noise once speech has been
    present.
  * RNNoise does not separate voices: other people talking in the room are kept.

## ADR-501: Allocation-free reset by swapping a preallocated state

* Context: `Denoiser::reset` is called on the audio encoder thread when suppression is
  switched on (and should be called after any gap in the frame sequence, such as
  unmuting). `nnnoiseless::DenoiseState` holds heap buffers, has no in-place reset, and
  cloning it allocates. FFT plans are cached per thread on first use.
* Decision: `Denoiser::new` allocates an active state and a fresh spare state, and runs
  one silent block through a throwaway state to fill the calling thread's FFT plan
  caches. The first `reset` swaps the spare in (exact reset); the used state moves to
  the spare slot instead of being freed. Later resets keep the current state and only
  restart the dry-then-crossfade handling: the discarded first block hides the stale
  overlap-add tail, and the recurrent state re-adapts within about 100 ms.
* Consequences: `process` and `reset` never allocate or free when the denoiser was
  constructed on the calling thread. This was verified with a counting global
  allocator over 200 frames and 4 resets (zero allocation or free calls). Memory is
  two RNNoise states, well under 100 KB. Rebuilding the audio stream creates a new
  denoiser and restores exact resets.

## ADR-502: dtln-rs evaluated and not adopted

* Context: the user suggested DataDog's `dtln-rs` (<https://github.com/DataDog/dtln-rs>),
  a Rust wrapper around the DTLN (Dual-signal Transformation LSTM Network) noise
  suppression models, as an alternative to RNNoise.
* Findings:
  * It does not build for our targets. `build.rs` is a no-op on Windows, so TensorFlow
    Lite is never linked, and there is no Linux branch. Prebuilt TensorFlow Lite is
    provided only for macOS and wasm32.
  * It depends on the TensorFlow Lite C++ runtime through `cxx`, plus CMake and Conan
    off macOS. That violates the Rust-only, pure-Rust or vendored build constraint and
    complicates the musl and windows-gnu cross builds. It also pulls in `neon`
    (Node.js bindings).
  * It processes 16 kHz audio with a 512-sample window and a 128-sample hop (32 ms
    window, 8 ms hop). That would require a 48 kHz to 16 kHz to 48 kHz resampling round
    trip, discard everything above 8 kHz (fullband Opus voice gets duller), add more
    latency than the 25 ms capture-to-encode budget allows, and run two LSTM models
    every 8 ms, which costs several times RNNoise's CPU.
  * It is not published on crates.io, has 8 commits (last on 2025-03-21), and its
    README is partly templated.
* Decision: stay with RNNoise via `nnnoiseless` (ADR-500).
* Possible follow-up: if higher suppression quality is ever needed, run the DTLN
  `.tflite` or ONNX models through a pure-Rust inference engine (for example `tract`)
  behind the same `Denoiser` interface. The interface (one 20 ms, 48 kHz frame in
  place, plus `reset`) is deliberately engine-agnostic so that swap would not touch
  the encoder thread or the client.
