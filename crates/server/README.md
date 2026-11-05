# chattercrab-server

Signaling server for Chattercrab, built as the `chattercrab-signal` binary. It hosts rooms,
relays SDP offers/answers and ICE candidates between members, keeps the last 200 chat
messages per room in memory, arbitrates the single screen-share slot per room and issues
TURN REST credentials for Coturn. It never relays media. The wire format is specified in
[`docs/PROTOCOL.md`](../../docs/PROTOCOL.md).

## Configuration

All settings come from environment variables (see `deploy/chattercrab.env.example`):

| Variable | Default | Meaning |
|----------|---------|---------|
| `CHATTERCRAB_BIND` | `0.0.0.0:8443` | Listen address. |
| `CHATTERCRAB_TLS_CERT`, `CHATTERCRAB_TLS_KEY` | unset | PEM certificate chain and key. Both or neither; without them the server speaks plain HTTP/WS (local development only). |
| `CHATTERCRAB_TURN_SECRET` | unset | Coturn `static-auth-secret`. Unset or the template placeholder disables `/turn-credentials` (503). |
| `CHATTERCRAB_TURN_REALM` | empty | TURN realm (informational). |
| `CHATTERCRAB_TURN_URLS` | unset | Comma-separated `turn:`/`turns:` URLs. A `stun:` URL is derived for every UDP `turn:` URL. |
| `CHATTERCRAB_MAX_ROOM_SIZE` | `5` | Members per room. |
| `CHATTERCRAB_MAX_ROOMS` | `100` | Rooms per server. |
| `CHATTERCRAB_ROOM_IDLE_TTL_SECS` | `3600` | Empty rooms idle this long are evicted (checked every 60 s). |
| `RUST_LOG` | `info` | `tracing` filter. Logs are JSON lines on stdout. |

Fixed limits: join timeout 5 s, WebSocket Ping every 20 s (closed after 2 missed Pongs),
outbound queue 256 messages per connection, WebSocket messages up to 64 KiB, REST bodies
up to 16 KiB, `POST /rooms` 10 per minute per IP (burst 5), `GET /turn-credentials` 30 per
minute per IP.

Signals: `SIGHUP` reloads the certificate without dropping connections (a failed reload
keeps the old one); `SIGTERM`/`SIGINT` stop accepting, send close `1001` to every socket
and exit within 10 s.

## Run locally

```sh
CHATTERCRAB_BIND=127.0.0.1:8443 RUST_LOG=debug cargo run -p chattercrab-server
curl http://127.0.0.1:8443/health
curl -X POST -H 'content-type: application/json' -d '{"name":"test"}' http://127.0.0.1:8443/rooms
```

## Test

```sh
cargo test -p chattercrab-server
```

Unit tests cover configuration parsing and TURN credentials (against a reference HMAC
vector). Integration tests in `tests/` start the server in-process on `127.0.0.1:0` with
shortened timers and drive it over HTTP and WebSocket: rooms, limits, join/welcome,
relay, heartbeat, join timeout, room full, screen lock, chat history, slow-consumer kick,
room deletion, graceful shutdown, and HTTPS with certificate hot reload.

## Static Linux build

```sh
cargo zigbuild --release -p chattercrab-server --target x86_64-unknown-linux-musl
```

Produces a fully static ELF (no `PT_INTERP`, no `DT_NEEDED`), about 3.9 MB.
