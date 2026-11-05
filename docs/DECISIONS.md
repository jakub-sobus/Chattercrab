# Decision Log

ADR-style record of choices made while building Chattercrab. Each entry: context,
decision, consequences. Newest entries at the bottom.

## ADR-001: Repository root is the workspace root

* Context: the brief describes a `chattercrab/` workspace directory; the git repository is
  already named `Chattercrab`.
* Decision: the repository root is the `chattercrab/` workspace. No extra nesting.
* Consequences: `cargo` commands run from the repository root.

## ADR-002: `PeerId` is a UUID v4

* Context: the brief allows ULID or UUID v4.
* Decision: UUID v4 serialized lowercase hyphenated. Byte order of the UUID equals the
  string order of its lowercase hex form, so "lexicographically smaller" is unambiguous.
* Consequences: depends on `uuid` with `v4` and `serde`.

## ADR-003: Rooms are created over REST and deleted when empty

* Context: the brief requires `POST /rooms` with a room cap, and deletion when empty.
* Decision: joining a nonexistent room fails (`404` before upgrade, `room_not_found`
  after). A room is deleted the moment its last member leaves. Rooms that were created
  but never joined are evicted by the idle sweeper. Rooms with members are never
  idle-evicted, since a voice call produces no signaling traffic.
* Consequences: if a lone member's connection drops, the room disappears and the
  reconnect fails with `room_not_found`; the client returns to the lobby with a notice.

## ADR-004: Server-side heartbeat uses WebSocket Ping frames

* Context: "server pings every 20 s and closes after 2 missed pongs", while the message
  list also contains application `ping`/`pong`.
* Decision: the heartbeat uses protocol-level Ping/Pong frames, which every WebSocket
  client answers automatically. Application `ping`/`pong` stays available for clients
  that want an RTT probe.

## ADR-005: Slow consumers are kicked through a cancellation token

* Context: broadcasting happens under a room lock; the slow member's cleanup itself
  needs the room lock and broadcasts.
* Decision: broadcasters `try_send`; on `Full` they cancel the member's kick token. The
  member's own task performs cleanup after the broadcaster released the lock.
* Consequences: no re-entrancy, no lock held across socket I/O.

## ADR-006: Derived STUN URLs in `TurnCredentials`

* Context: webrtc gathers server-reflexive candidates only from `stun:` URLs, and the
  environment template only lists `turn:`/`turns:` URLs.
* Decision: for every `turn:` URL with UDP transport, the server adds `stun:host:port`.
* Consequences: peers behind ordinary NAT connect directly instead of always relaying.

## ADR-007: Native build tooling on the Windows development host

* Context: the development machine is Windows with MSVC. `opus` 0.4 builds libopus via
  `opusic-sys`, which needs CMake; cross builds need a C toolchain per target.
* Decision: install CMake and Zig from PyPI (`pip install --user cmake ziglang`) and
  `cargo-zigbuild`/`cargo-deny` with `cargo install`. No system-wide installers.
* Consequences: documented in the README build section.
