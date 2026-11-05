### Noise suppression

Optional microphone noise suppression, toggled by the "Noise suppression" checkbox in
the client options (`MediaConfig::noise_suppression` /
`MediaCommand::SetNoiseSuppression`). Implemented in `denoise.rs` with RNNoise
(`nnnoiseless`, a pure-Rust port, BSD-3-Clause).

* What it does: removes steady and fluctuating background noise such as fans, hiss,
  hum-like rumble, traffic and keyboard noise, while keeping the speaker's voice.
  Measured attenuation: 19 to 53 dB on fan-like and rumble-like noise, about 37 dB on
  hiss after speech; a voiced test signal keeps its energy within about 1 dB.
* Where it runs: in the encoder thread, after resampling to 48 kHz and before Opus,
  on each 20 ms frame (two 10 ms RNNoise blocks). Skipped entirely when disabled or
  muted, so it costs nothing when off.
* Cost: about 80 microseconds per 20 ms frame in release builds (about 0.4% of one
  CPU core) and well under 100 KB of memory. `process` and `reset` never allocate.
* Latency: adds 10 ms (RNNoise's overlap-add delay). Switching it on is smoothed with a
  10 ms crossfade.
* Limitations:
  * It does not remove other voices: people talking in the background are kept.
  * Music and other tonal sounds may get slight artifacts or be partly suppressed.
  * Perfectly flat white noise is barely attenuated until speech has been heard; real
    background noise is not flat and is removed normally.
  * Switching it off drops 10 ms of audio once (the denoiser's delay is removed).

Measure it yourself with:

```bash
cargo test --release -p chattercrab-media denoise -- --nocapture
```
