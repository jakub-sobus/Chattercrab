# Client decisions (ADR-300 series)

## ADR-300: Backend on its own runtime thread, pure session logic

* Context: `update()` must never block, and the UI must not touch webrtc or cpal.
* Decision: a dedicated OS thread runs a 2-worker Tokio runtime with one Backend task.
  The UI sends `UiCommand`s with `try_send` (bounded 64) and receives `AppEvent`s
  (bounded 256) through a single iced subscription that takes ownership of the
  receiver once. Protocol choreography lives in `net::session::Session`, a pure state
  machine returning effects, so it is unit tested without sockets.
* Consequences: if the backend queue is full a UI action is dropped with a warning
  instead of freezing the window.

## ADR-301: One signaling task per connection attempt, tagged by generation

* Context: reconnects can overlap with late events from a dead socket.
* Decision: each attempt spawns a task tagged with an increasing generation; the backend
  ignores events whose generation is not current. A 404 on the upgrade means the room is
  gone (back to the lobby); every other failure schedules a backoff retry
  (1, 2, 4, 8, 16, 30, 30 ... seconds). TURN credentials are refetched before each retry.

## ADR-302: ring as the only TLS crypto provider

* Context: `reqwest`'s `rustls` feature pulls in aws-lc-rs, which complicates cross
  builds; webrtc and the server already use ring.
* Decision: `reqwest` with `rustls-no-provider`, `tokio-tungstenite` with
  `rustls-tls-webpki-roots`, and `rustls` with `ring`. `main` installs the ring provider
  as the process default. `cargo tree -i aws-lc-rs` finds nothing.
* Consequences: HTTPS uses the OS trust store (rustls-platform-verifier); WSS uses the
  bundled webpki roots. Self-signed server certificates are not supported.

## ADR-303: Minimal iced features

* Decision: `default-features = false` with `tokio`, `image-without-codecs` (frames
  arrive as raw RGBA, so no image decoders), `wgpu`, `tiny-skia`, plus `x11` on Linux
  only. No `wayland`, `web-colors`, `linux-theme-detection`, `canvas`, `advanced`.
  Antialiasing (MSAA) is disabled because nothing is tessellated.

## ADR-304: `--software-render` and the Windows renderer default via environment

* Context: iced 0.14 selects its compositor only through `ICED_BACKEND`, and wgpu's
  backend through `WGPU_BACKEND`. There is no programmatic setting.
* Decision: `main` sets `ICED_BACKEND=tiny-skia` for `--software-render`; otherwise on
  Windows it sets `WGPU_BACKEND=dx12` unless the user already set it. Both happen before
  any thread starts, which is what makes `std::env::set_var` sound.
* Measured idle memory (release, connect screen, AMD Radeon 780M, Windows 11):
  DX12 82 MB private / 102 MB working set; Vulkan about 125 MB private; tiny-skia 15 MB
  private / 29 MB working set. Most of the GPU figure is driver allocations.
* Consequences: the 80 MB idle target holds with `--software-render` on this machine;
  with DX12 it is just above. Users can override the backend.

## ADR-305: Programmatic icon embedded via embed-resource

* Decision: `src/icon.rs` rasterizes an original crab silhouette (body ellipse, two open
  claws, eye stalks, three jointed legs per side) with 4x4 supersampling. `build.rs`
  includes the same file, writes a multi-size `.ico` (16 to 256 px, 32-bit BMP entries)
  to `OUT_DIR` and compiles a one-line `.rc` with `embed-resource`. A missing resource
  compiler is a warning, not an error.
* Consequences: verified that both the MSVC and the zig-built GNU executables contain
  the `.rsrc` section (305 KB). The 256 px BMP entry dominates; a PNG entry would be
  smaller but needs an encoder, so it was not worth a dependency.

## ADR-306: Room screen opens on `welcome` for created rooms

* Decision: "Join" opens the room screen immediately with a "Joining..." header. "Create
  and join" stays in the lobby until the server's `welcome` arrives, then opens the room.

## ADR-307: Noise suppression toggle

* Decision: `noise_suppression` (default `false`, `serde(default)` so old files load) is
  passed in `MediaConfig` at engine start and toggled live with
  `MediaCommand::SetNoiseSuppression`, from the connect screen or the room bottom bar.
  The config is saved on every change.
