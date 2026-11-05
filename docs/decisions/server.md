# Server decisions (to merge into DECISIONS.md)

## ADR-100: rustls with the `ring` provider, PEM parsing via `rustls-pki-types`

* Context: `axum-server`'s `tls-rustls` feature pulls in `aws-lc-rs`, which needs CMake
  and NASM and complicates the static musl cross build. `rustls-pemfile` is archived.
* Decision: use `axum-server` with `tls-rustls-no-provider`, build the `ServerConfig`
  with `rustls::crypto::ring::default_provider()`, and parse PEM with
  `rustls::pki_types::pem::PemObject`. ALPN is set to `http/1.1` only.
* Consequences: pure Rust plus `ring`'s C/asm, which `cargo zigbuild` handles. WebSocket
  upgrades always use HTTP/1.1.

## ADR-101: Writes race against the kick token and a 10 s timeout

* Context: a member whose socket stops draining blocks its own task inside a socket
  write, so it would never notice its kick token and never clean up.
* Decision: every socket write is raced against the member's kick token and bounded by
  a 10 s timeout. Final close frames are bounded by 1 s.
* Consequences: slow or stalled clients are always removed, and `peer_left` reaches the
  rest of the room.

## ADR-102: Direct replies bypass the outbound queue

* Context: replies to the sender (`pong`, `error`, `screen_granted`, `screen_denied`) are
  produced inside the connection task that owns the socket.
* Decision: they are written straight to the socket; broadcasts and relayed messages go
  through the bounded queue. `screen_granted` is written before the queued
  `screen_started`, matching PROTOCOL.md.

## ADR-103: Room-count reservation with compare-and-swap

* Context: `DashMap::len` is not atomic with insertion, so a check-then-insert could
  exceed `CHATTERCRAB_MAX_ROOMS` under concurrent `POST /rooms`.
* Decision: an `AtomicUsize` counter is reserved with a CAS loop before insertion and
  decremented only when `remove_if(Arc::ptr_eq)` actually removes the room.

## ADR-104: Shutdown deadline covers both HTTP and WebSocket tasks

* Context: upgraded WebSockets leave hyper's connection tracking, so `axum-server`'s
  graceful shutdown does not wait for them.
* Decision: each upgraded socket holds a `TaskTracker` token. After shutdown is requested
  one shared deadline (`shutdown_grace`, 10 s) bounds both the HTTP drain and the wait for
  WebSocket tasks, which close themselves with `1001` as soon as the shutdown token fires.
* Consequences: the process exits within 10 s of `SIGTERM` in all cases.

## ADR-105: Upgrades during shutdown are refused with 503

* Decision: `GET /ws/{room_id}` answers `503` (`internal`) once shutdown started, instead
  of accepting a socket only to close it immediately. Documented in PROTOCOL.md.
