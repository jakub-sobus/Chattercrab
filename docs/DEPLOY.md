# Deploying Chattercrab

This guide installs the Chattercrab signaling server (`chattercrab-signal`) and Coturn on a
fresh Debian 12 or Ubuntu 22.04/24.04 VPS. Everything runs natively under systemd: no
containers and no reverse proxy. Media never passes through `chattercrab-signal`; Coturn
only relays media for peers that cannot reach each other directly.

Throughout this guide `chat.example.com` stands for your domain and `203.0.113.10` for
the VPS public IPv4 address.

## 1. Requirements

| Item | Minimum |
|------|---------|
| VPS | 1 vCPU, 512 MB RAM, x86_64, public IPv4 (NAT with 1:1 port forwarding also works) |
| OS | Debian 12, Ubuntu 22.04 or Ubuntu 24.04 with systemd |
| DNS | A record for the domain (AAAA optional) |
| Bandwidth | Only relayed sessions use VPS bandwidth: about 96 kbps per relayed audio stream, about 1.5 Mbps per relayed screen-share stream |

Ports opened by `firewall.sh`:

| Port | Protocol | Purpose |
|------|----------|---------|
| 22 | tcp | SSH |
| 80 | tcp | certbot standalone (certificate issuance and renewal) |
| 8443 | tcp | `chattercrab-signal` HTTPS and WSS (or 443, see section 9) |
| 3478 | udp, tcp | Coturn STUN/TURN |
| 5349 | tcp, udp | Coturn TURN over TLS/DTLS |
| 49152-49999 | udp | Coturn relay ports |

## 2. DNS

Create the records at your DNS provider and wait until they resolve:

```text
chat.example.com.   300  IN  A     203.0.113.10
chat.example.com.   300  IN  AAAA  2001:db8::10      ; optional
```

Check from your workstation:

```sh
dig +short chat.example.com A
```

The same name serves the signaling server and Coturn, so one certificate covers both.

## 3. Build the release artifacts

On a Linux build machine (or WSL, see section 13) with Rust installed:

```sh
deploy/build-release.sh
```

This produces:

```text
dist/server/chattercrab-signal        static musl binary (the script fails if it is not static)
dist/server/install.sh ...            deploy assets copied next to the binary
dist/client-linux/chattercrab
dist/client-windows/chattercrab.exe
dist/SHA256SUMS
```

`build-release.sh` uses `cargo zigbuild` for cross targets when available (zig from your
package manager or `pip install ziglang`), otherwise `cross`, otherwise plain `cargo`.
Use `--server-only` when you only need the server.

The Windows client in `dist/client-windows/` should come from `cargo zigbuild`: it links
the C++ runtime (needed by OpenH264) statically and imports only Windows system DLLs,
including the UCRT that ships with Windows 10 and later. A plain
`cargo build --release -p chattercrab-client --target x86_64-pc-windows-gnu` with the
mingw-w64 toolchain also succeeds, but the result needs `libstdc++-6.dll` from mingw-w64
next to `chattercrab.exe`.

Copy the server bundle to the VPS:

```sh
scp -r dist/server root@chat.example.com:/root/chattercrab
scp dist/SHA256SUMS root@chat.example.com:/root/chattercrab/
ssh root@chat.example.com 'cd /root/chattercrab && grep server/ SHA256SUMS | sed "s#./server/##" | sha256sum -c'
```

## 4. Obtain the TLS certificate

Log in to the VPS as root (or use `sudo`) and make sure the clock is synchronised, since
both TLS and TURN credentials depend on it:

```sh
timedatectl set-ntp true
timedatectl status | grep -E 'synchronized|NTP'
```

Install certbot and request the certificate in standalone mode. Port 80 must be reachable
(it is on a fresh VPS; if a firewall is already active run `ufw allow 80/tcp` first):

```sh
apt-get update && apt-get install -y certbot
certbot certonly --standalone -d chat.example.com \
    --agree-tos -m admin@example.com --non-interactive
```

The certificate lands in `/etc/letsencrypt/live/chat.example.com/`. The installer copies
it to `/etc/chattercrab/tls/` and installs a deploy hook so renewals are copied
automatically.

## 5. Run the installer

```sh
cd /root/chattercrab
./install.sh --domain chat.example.com
```

`install.sh` is idempotent; rerun it at any time to repair or upgrade. It:

1. Installs `coturn`, `certbot`, `curl` and `iproute2`.
2. Creates the `chattercrab` system user and group (no shell, no home).
3. Installs the binary to `/opt/chattercrab/bin/chattercrab-signal` (0755).
4. Creates `/etc/chattercrab/env` (0640, `root:chattercrab`) from
   `chattercrab.env.example` if it does not exist, replacing `chat.example.com` with
   `--domain`. An existing env file is never overwritten.
5. Generates a random 32-byte TURN secret if `CHATTERCRAB_TURN_SECRET` is unset or still
   the placeholder, and writes the same secret into `/etc/chattercrab/env` and
   `/etc/turnserver.conf` (the original Coturn config is kept as `/etc/turnserver.conf.orig`).
6. Detects NAT by comparing the public IPv4 (HTTPS echo services, then a STUN request)
   with the local addresses, and sets `external-ip=PUBLIC/PRIVATE` in
   `/etc/turnserver.conf` when the VPS is behind NAT. Pass `--public-ip` to skip discovery.
7. Adds the `turnserver` user to the `chattercrab` group so Coturn can read the
   certificates, installs the systemd unit, the Coturn drop-in
   (`/etc/systemd/system/coturn.service.d/override.conf`) and the certbot deploy hook
   (`/etc/letsencrypt/renewal-hooks/deploy/chattercrab.sh`), copies the certificate,
   runs `firewall.sh`, reloads systemd, and enables and (re)starts both services.
8. Prints and runs a health check.

Useful flags: `--dry-run` (print every action, change nothing), `--skip-firewall`,
`--no-start`, `--skip-apt`, `--binary PATH`. See `./install.sh --help`.

If the certificate is missing the installer still installs and enables everything but
does not start the services; obtain the certificate (section 4) and rerun it.

### Configuration

Edit `/etc/chattercrab/env`, then `systemctl restart chattercrab-signal`:

| Variable | Default | Meaning |
|----------|---------|---------|
| `CHATTERCRAB_BIND` | `0.0.0.0:8443` | Listen address |
| `CHATTERCRAB_TLS_CERT`, `CHATTERCRAB_TLS_KEY` | unset | PEM paths; without them the server speaks plain HTTP (development only) |
| `CHATTERCRAB_TURN_SECRET` | generated | Shared secret, must equal `static-auth-secret` in `/etc/turnserver.conf` |
| `CHATTERCRAB_TURN_REALM` | domain | TURN realm (must equal `realm` in `/etc/turnserver.conf`) |
| `CHATTERCRAB_TURN_URLS` | domain URLs | Comma separated `turn:`/`turns:` URLs handed to clients |
| `CHATTERCRAB_MAX_ROOM_SIZE` | `5` | Members per room |
| `CHATTERCRAB_MAX_ROOMS` | `100` | Rooms per server |
| `CHATTERCRAB_ROOM_IDLE_TTL_SECS` | `3600` | Eviction age for rooms that were never joined |
| `RUST_LOG` | `info,chattercrab_server=info` | Log filter |

If you change the secret or realm by hand, rerun `./install.sh` to copy them into
`/etc/turnserver.conf` and restart Coturn.

## 6. Verify the signaling server

```sh
curl -fsS https://chat.example.com:8443/health
# {"status":"ok","version":"0.1.0"}

curl -fsS https://chat.example.com:8443/rooms
curl -fsS https://chat.example.com:8443/turn-credentials
```

`/turn-credentials` must return a non-empty `urls` list; a `503 turn_unavailable` means the
secret or URLs are missing from `/etc/chattercrab/env`.

## 7. Verify TURN

The `coturn` package ships test clients. Run these from another machine if possible (on
the VPS itself loopback peers are blocked by `denied-peer-ip`).

STUN binding (should print your public address as the reflexive address):

```sh
turnutils_stunclient chat.example.com
```

TURN allocation with the shared secret (REST API mode, like the real clients):

```sh
SECRET=$(sudo grep ^CHATTERCRAB_TURN_SECRET= /etc/chattercrab/env | cut -d= -f2)
turnutils_uclient -W "$SECRET" -r chat.example.com -y -c -n 1 -m 1 chat.example.com
```

A working server ends with `tot_send_msgs=2, tot_recv_msgs=2`; a wrong secret prints
`ERROR: Cannot complete Allocation`. Repeat over TLS with `-S -p 5349`.

With credentials from the signaling server (exactly what a client receives):

```sh
CREDS=$(curl -fsS https://chat.example.com:8443/turn-credentials)
USER=$(echo "$CREDS" | sed -E 's/.*"username":"([^"]+)".*/\1/')
PASS=$(echo "$CREDS" | sed -E 's/.*"credential":"([^"]+)".*/\1/')
turnutils_uclient -u "$USER" -w "$PASS" -r chat.example.com -y -c -n 1 -m 1 chat.example.com
```

## 8. Logs and operations

```sh
journalctl -u chattercrab-signal -f      # JSON log lines from the signaling server
journalctl -u coturn -f                  # Coturn (logs to syslog/journald)
systemctl status chattercrab-signal coturn
systemctl reload chattercrab-signal      # re-read TLS certificate (SIGHUP), keeps connections
systemctl restart coturn                 # Coturn cannot reload certificates
```

Certificate renewal is automatic: the certbot systemd timer renews, and the deploy hook
copies the new files into `/etc/chattercrab/tls/`, reloads `chattercrab-signal` and
restarts Coturn. Test the renewal path with:

```sh
certbot renew --dry-run --run-deploy-hooks
```

(`--run-deploy-hooks` needs certbot 2.7 or newer; on older versions run the hook by hand
as shown in the troubleshooting table.)

Memory check (idle target: under 10 MB RSS):

```sh
grep -E 'VmRSS|VmHWM' /proc/$(systemctl show -p MainPID --value chattercrab-signal)/status
```

## 9. Serving on port 443

The unit grants `CAP_NET_BIND_SERVICE`, so the server can bind 443 directly:

```sh
sed -i 's/^CHATTERCRAB_BIND=.*/CHATTERCRAB_BIND=0.0.0.0:443/' /etc/chattercrab/env
./firewall.sh --signal-port 443
ufw delete allow 8443/tcp
systemctl restart chattercrab-signal
```

Clients then use `https://chat.example.com` without a port.

## 10. Upgrade procedure

1. Build the new release (`deploy/build-release.sh --server-only`) and copy
   `dist/server/` to the VPS as in section 3.
2. Keep the previous binary for rollback and install the new one:

   ```sh
   cp -p /opt/chattercrab/bin/chattercrab-signal /opt/chattercrab/bin/chattercrab-signal.prev
   install -m 0755 -o root -g root chattercrab-signal /opt/chattercrab/bin/chattercrab-signal
   systemctl restart chattercrab-signal
   curl -fsS https://chat.example.com:8443/health
   ```

   Rerunning `./install.sh --domain chat.example.com` does the same (and also refreshes the
   unit, drop-in, hook and Coturn settings) without touching the env file or secret.
3. Rollback: `install -m 0755 /opt/chattercrab/bin/chattercrab-signal.prev /opt/chattercrab/bin/chattercrab-signal && systemctl restart chattercrab-signal`.

Rooms and chat history live in memory only. After a restart every room is gone: clients
see their signaling connection drop, try to rejoin with backoff, get `room_not_found` and
return to the lobby, where someone recreates the room. Upgrade when the server is idle.

## 11. Troubleshooting

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| ICE failed, peers never connect | UDP relay range blocked, wrong `external-ip` behind NAT, or `/turn-credentials` returns no URLs | `ufw status` must list `49152:49999/udp` and 3478; check any provider firewall too. Rerun `./install.sh` to redetect NAT and check `grep external-ip /etc/turnserver.conf`. Test with `turnutils_uclient` (section 7). Check `journalctl -u coturn` for allocations during a call. |
| One-way audio | One side cannot receive: a client firewall drops inbound UDP, relay addresses advertise a private IP (missing `external-ip`), or the sender has the wrong input device or is muted | Rerun the installer on a NAT VPS so `external-ip` is set. On the silent side's sender, check the input device in the Connect screen and the mute state. Allow `chattercrab` through the desktop firewall. |
| Black or frozen screen-share frames | Receiver has not yet received a keyframe (the publisher sends one every 2 s and on request), the publisher runs a Wayland session (only X11 capture is supported on Linux), the captured window is minimised, or the publisher's uplink is too slow | Wait 2 s. On Linux log in to an X11 session. Do not minimise the shared window. Each viewer needs about 1.5 Mbps of publisher upstream; lower the frame rate. |
| TURN `401 Unauthorized` in `journalctl -u coturn` | Clock skew: the TURN REST username embeds an expiry timestamp, so a wrong server or client clock rejects valid credentials. Also a secret mismatch between `/etc/chattercrab/env` and `/etc/turnserver.conf` | `timedatectl set-ntp true` on the VPS and check the client clock. Rerun `./install.sh` to resync the secret, then `systemctl restart coturn chattercrab-signal`. |
| `curl /health` fails, service restarting | Certificate unreadable or missing, or the port is in use | `journalctl -u chattercrab-signal -n 50`. Certificates must be `0640 root:chattercrab` in `/etc/chattercrab/tls/` (rerun the deploy hook: `RENEWED_LINEAGE=/etc/letsencrypt/live/chat.example.com /etc/letsencrypt/renewal-hooks/deploy/chattercrab.sh`). `ss -ltnp | grep 8443`. |
| Coturn starts without TLS (`5349` closed) | `turnserver` user cannot read the certificate | `id turnserver` must list `chattercrab`; `systemctl restart coturn` after fixing. |
| certbot fails | Port 80 blocked or DNS not yet propagated | `ufw allow 80/tcp`, check `dig +short chat.example.com`. |
| `429 rate_limited` from `POST /rooms` | Too many room creations from one IP | Wait a minute. |

## 12. Uninstall

```sh
systemctl disable --now chattercrab-signal coturn
rm -f /etc/systemd/system/chattercrab-signal.service
rm -rf /etc/systemd/system/coturn.service.d /opt/chattercrab /etc/chattercrab
rm -f /etc/letsencrypt/renewal-hooks/deploy/chattercrab.sh
[ -f /etc/turnserver.conf.orig ] && mv /etc/turnserver.conf.orig /etc/turnserver.conf
userdel chattercrab; groupdel chattercrab 2>/dev/null || true
systemctl daemon-reload
```

## 13. Building on Linux (client prerequisites)

The server needs only Rust plus a musl toolchain (zig, or `musl-tools` for plain cargo).
The desktop client links system libraries for audio, windowing and screen capture.

Debian/Ubuntu packages:

```sh
sudo apt-get install -y build-essential cmake pkg-config clang libclang-dev nasm \
    libasound2-dev \
    libx11-dev libxcb1-dev libxcb-randr0-dev libxcb-shm0-dev libxcb-xfixes0-dev libxrandr-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
    libpipewire-0.3-dev libdbus-1-dev libegl1-mesa-dev libgbm-dev \
    musl-tools mingw-w64
```

| Package group | Needed by |
|---------------|-----------|
| `cmake`, `build-essential` | bundled libopus (`opusic-sys`), OpenH264 (C++) |
| `libasound2-dev` | `cpal` (ALSA backend; PulseAudio and PipeWire are reached through their ALSA plugins) |
| `libx11`, `libxcb*`, `libxrandr` | `xcap` X11 screen capture, `winit` |
| `libxkbcommon*`, `libwayland-dev`, `libegl1-mesa-dev` | `winit`/`iced` windowing and rendering |
| `libpipewire-0.3-dev`, `clang`, `libclang-dev`, `libgbm-dev` | `xcap` Wayland/PipeWire support (bindgen); capture on Wayland is a documented follow-up |
| `libdbus-1-dev` | `audio_thread_priority` (rtkit) if built with dbus |
| `musl-tools`, `mingw-w64` | plain-cargo cross builds when zig is not available |

Runtime on the desktop: an X11 session (Xorg or XWayland for the window; screen capture
requires X11), and a working ALSA/PulseAudio/PipeWire audio stack.

### Verifying in WSL (development machines running Windows)

`deploy/wsl-verify.sh` runs the full Linux verification (fmt, clippy, tests, release build,
`build-release.sh`, shellcheck, `systemd-analyze verify`, idle RSS of the musl server) in
a WSL Ubuntu distro with systemd enabled. From Git Bash:

```sh
wsl --install -d Ubuntu-24.04 --no-launch
MSYS_NO_PATHCONV=1 wsl -d Ubuntu-24.04 -u root --exec sh -c \
    'printf "[boot]\nsystemd=true\n" > /etc/wsl.conf'
wsl --terminate Ubuntu-24.04
MSYS_NO_PATHCONV=1 wsl -d Ubuntu-24.04 -u root --exec bash \
    /mnt/d/Chattercrab/deploy/wsl-verify.sh --setup /mnt/d/Chattercrab
```

The script builds in `~/chattercrab-verify` inside WSL because builds on `/mnt/d` are slow.
`systemd-analyze verify` needs the installed binary path and the `chattercrab` user; run
`deploy/install.sh --skip-firewall --no-start --binary dist/server/chattercrab-signal`
inside WSL once to provide them (do not run `firewall.sh` in WSL).
