# Media decisions (chattercrab-media)

ADR-style log for the media engine. Numbering starts at 200 to stay clear of the main
`docs/DECISIONS.md` log.

## ADR-200: Mapping the spec's WebRTC names onto webrtc 0.21

* Context: the brief names the webrtc-rs 0.1x API (`RTCPeerConnection`, `on_track`,
  `SettingEngine`, ...). webrtc 0.21.1 is a rewrite as a thin async layer over the sans-IO
  `rtc` core, with a different API surface.
* Decision: use 0.21.1 (current stable) and map as follows.

| Spec | webrtc 0.21.1 |
|------|---------------|
| `RTCPeerConnection` | `PeerConnectionBuilder::build()` returning `impl PeerConnection`, stored as `Arc<dyn PeerConnection>` |
| callbacks (`on_track`, `on_ice_candidate`, `on_negotiation_needed`, state changes) | one `PeerConnectionEventHandler` trait object per connection (`peer::Handler`), which forwards into a bounded channel with `try_send` |
| `SettingEngine` | `SettingEngineBuilder` |
| ICE timeouts (disconnected 5 s, failed 15 s) | `SettingEngineBuilder::with_ice_timeouts(Some(5s), Some(15s), Some(2s keepalive))` |
| UDP and TCP candidates only | `with_network_types([Udp4, Tcp4])`, `with_multicast_dns_mode(Disabled)`, sockets from `with_udp_addrs(["0.0.0.0:0"])` and `with_tcp_addrs(["0.0.0.0:0"])` (passive ICE-TCP) |
| `RTCBundlePolicy::MaxBundle`, `RTCRtcpMuxPolicy::Require` | same enums via `RTCConfigurationBuilder::with_bundle_policy` / `with_rtcp_mux_policy` |
| ICE servers from `TurnCredentials` | one `RTCIceServer { urls, username, credential }` |
| `TrackLocalStaticSample` | same type; samples written with `sample_writer(ssrc, payload_type).write_sample(&Sample)` |
| reading remote RTP | `TrackRemote::poll()` yielding `TrackRemoteEvent::OnRtpPacket` |
| inbound PLI/FIR at the sender | a custom interceptor marks them `DeliverToApplication`; read with `TrackLocal::poll()` (ADR-204) |
| crypto | default `crypto-ring` provider (no aws-lc / cmake / nasm needed) |

* Consequences: `rtc` is a direct dependency (same version) to name `Sample`,
  `MediaStreamTrack` codings, the H.264 depacketizer and RTCP types.

## ADR-201: One local track per connection, encoded once

* Context: the brief asks for one Opus `TrackLocalStaticSample` shared by all connections.
  In 0.21 a local track stores a single binding context: adding the same track to a
  second connection rebinds it, so a shared track would only feed one peer.
* Decision: create one `TrackLocalStaticSample` per connection (fresh SSRC each), but
  encode only once. The audio pump wraps each encoded frame in `Bytes` (refcounted) and
  writes the same buffer to every connection's track. Same for the screen track.
* Consequences: identical CPU cost for encoding; packetization and SRTP happen per
  connection, which is inherent to a mesh anyway.

## ADR-202: Negotiation model

* Context: perfect negotiation is required; the brief's "newcomer offers" flow also needs
  to work with renegotiation for screen share.
* Decision: `peer::negotiation::Negotiator` is a pure state machine (impolite = smaller
  `PeerId`). Offers are driven by `on_negotiation_needed`, which webrtc fires
  spec-compliantly when tracks are added or removed. The newcomer (`AddPeer { initiate:
  true }`) attaches its tracks immediately and therefore offers; the existing member
  (`initiate: false`) attaches nothing until the first remote offer is applied, then adds
  its tracks (reusing the offer's transceivers) before answering, so audio flows both ways
  after one round trip. Glare is resolved by ignore (impolite) or rollback (polite), with
  a queued re-offer after the answer.
* Consequences: verified by unit tests (glare in both delivery orders, renegotiation,
  200 randomized races) and by `tests/peer_loopback.rs`, where both sides start sharing
  simultaneously over real connections.

## ADR-203: ICE recovery and candidate scope

* Decision: on `Failed`, the impolite side calls `restart_ice()` (up to 2 attempts per
  peer; the counter resets on `Connected`). The polite side waits for the restart offer.
  Only IPv4 UDP/TCP host candidates are gathered locally; server-reflexive and relay
  candidates come from the configured STUN/TURN servers. mDNS candidates are disabled.
* Rationale: an ICE restart keeps DTLS and media state, avoiding the races of tearing down
  and recreating connections from both sides. IPv6 is left out to keep candidate counts
  small for a 5-peer mesh; Coturn covers IPv6-only paths if ever needed.

## ADR-204: Keyframe requests through an interceptor

* Context: in 0.21 inbound RTCP stops at the end of the interceptor chain unless marked
  for the application.
* Decision: `peer::rtcp::KeyframeRequestForwarder` (slot 14 000, after all built-ins)
  passes PLI and FIR packets to the application and drops other RTCP. A task per video
  sender polls the track and sets the shared keyframe flag, which the encoder thread reads
  every frame. Adding a new viewer also sets the flag.

## ADR-205: Real-time priority without dbus

* Decision: `audio_thread_priority` with default features off. On Windows it uses MMCSS;
  on Linux without the `dbus` feature it promotes the mixer thread directly with
  `SCHED_FIFO`, which needs an `RLIMIT_RTPRIO` grant (for example via
  `/etc/security/limits.d`). If promotion fails the mixer logs a warning and runs at
  normal priority.
* Rationale: avoids a `libdbus-1-dev` build dependency and keeps cross-compilation simple.
  rtkit support can be enabled later by turning the feature back on.

## ADR-206: Opus settings and DTX framing

* Decision: VoIP, mono 48 kHz, 32 kbps, DTX on, in-band FEC on, complexity 5, and
  `packet_loss_perc = 10`. Every 20 ms frame is sent, including 1 to 3 byte DTX frames.
* Rationale: libopus only embeds FEC data when it expects loss, so a non-zero loss
  percentage is required for FEC to have any effect. Sending DTX frames keeps the RTP
  timestamp and sequence advancing exactly 960 per packet, which keeps the jitter buffer
  simple; the cost is about 1.2 kbit/s during silence.

## ADR-207: Clocking, buffering and resampling

* Decision: capture ring 500 ms (device rate), playback ring 200 ms (device rate). The
  mixer produces a frame whenever the playback ring holds less than 40 ms, so the output
  device clock paces mixing. Without an output device it paces itself on a 20 ms timer.
  Rate conversion uses rubato's `Async` cubic polynomial resampler (no sinc filter), in
  fixed-input mode, on the encoder thread (device rate to 48 kHz) and on the mixer thread
  (48 kHz to device rate).
* Rationale: the polynomial resampler is the voice-call option rubato recommends and is
  allocation-free in `process_into_buffer`. Clock drift between peers is absorbed by the
  jitter buffer (PLC on underrun, frame drop when more than 60 ms above target).

## ADR-208: Jitter buffer

* Decision: 16 fixed slots keyed by sequence number, playout target 2 to 6 frames
  (40 to 120 ms) from `1 + ceil(3 * jitter / 20 ms)` using the RFC 3550 estimator, raised by
  one frame on each underrun and decayed after 5 s without loss. Missing frame: decode the
  next packet with FEC if present, otherwise PLC; after 5 concealed frames with an empty
  buffer, stop and re-buffer. Packets more than 120 ms ahead resynchronise the playout point.

## ADR-209: Screen capture threading and platform dependencies

* Decision: the capture source is opened on the capture thread because `xcap` handles are
  not `Send` on Windows; `ScreenPublisher::start` waits for the open result over a channel.
  OpenH264 is built from source by `openh264-sys2` (no nasm needed; assembly is skipped if
  nasm is absent).
* Consequences: on Linux, `xcap` links `libxcb`, `libpipewire-0.3`, `libdbus` (via zbus,
  pure Rust) and needs `libclang` at build time for the PipeWire bindings. Wayland capture
  goes through the desktop portal and is best effort; X11 is the supported path.

## ADR-210: Engine start never fails on missing devices

* Decision: `MediaEngine::start` returns `Ok` whenever a Tokio runtime is present. Devices
  open in the background; failures are reported as `MediaEvent::Error` and the engine stays
  usable (receive-only without a microphone; without a speaker the mixer keeps draining
  peer buffers on its own 20 ms clock and discards the output).
* Rationale: a missing or busy microphone should not prevent joining a room and chatting.
