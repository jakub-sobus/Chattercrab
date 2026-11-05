# Linux and deployment decisions (ADR-400 series)

## ADR-400: WSL Ubuntu 24.04 with systemd as the Linux verification host

* Context: the development machine runs Windows; the brief requires Linux builds,
  `systemd-analyze verify` and Linux memory measurements.
* Decision: install `Ubuntu-24.04` with `wsl --install --no-launch`, enable systemd in
  `/etc/wsl.conf`, default user root. `deploy/wsl-verify.sh` clones the repository into
  the WSL filesystem (`~/chattercrab-verify`) because builds on `/mnt/<drive>` are slow.
  Windows git worktrees cannot be read by git inside WSL (their `.git` file holds a
  drive-letter path), so the script falls back to copying the tree with tar.
* Consequences: verification is reproducible with one command from Git Bash
  (`MSYS_NO_PATHCONV=1` is required, otherwise Git Bash rewrites `/mnt/...` paths).

## ADR-401: install.sh owns /etc/turnserver.conf, never the env file

* Context: the installer must be idempotent, keep the TURN secret identical in two files,
  and not destroy operator edits.
* Decision: the first run backs up the distribution `/etc/turnserver.conf` to
  `/etc/turnserver.conf.orig` and installs `deploy/turnserver.conf` with a
  `# Managed by Chattercrab install.sh` marker. Later runs only rewrite
  `static-auth-secret`, `realm` and `external-ip` in place. `/etc/chattercrab/env` is
  created once and afterwards only its `CHATTERCRAB_TURN_SECRET` is filled in when unset
  or still the placeholder. The secret is always copied from env into Coturn, so an
  operator-chosen secret also propagates.
* Consequences: rerunning the installer after editing the env file resyncs Coturn.

## ADR-402: NAT detection

* Decision: discover the public IPv4 from HTTPS echo services (ipify, ifconfig.me,
  icanhazip), falling back to a STUN binding request with `turnutils_stunclient`.
  If that address is bound to a local interface there is no NAT and any `external-ip`
  line is removed; otherwise `external-ip=PUBLIC/PRIVATE` is set with the private source
  address of the default route. `--public-ip` overrides discovery; if discovery fails the
  setting is left unchanged with a warning.

## ADR-403: build-release.sh builder selection and static check

* Decision: the host's own target is always built with plain `cargo` (it links the host
  system libraries the Linux client needs); foreign targets use `cargo zigbuild`, then
  `cross`, then plain `cargo` with a warning. `CHATTERCRAB_BUILDER` forces a builder.
  On a non-Linux host the Linux client is skipped unless
  `CHATTERCRAB_FORCE_LINUX_CLIENT=1`, because it needs ALSA/X11 development libraries.
  Static linking is verified by parsing the ELF program headers with `od` and failing
  on a `PT_INTERP` header, which works without `file`/`ldd` (Git Bash); `file` and `ldd`
  output is printed when available. `--locked` is used when `Cargo.lock` exists.
* Consequences: verified on WSL Ubuntu 24.04 and on Windows Git Bash (musl server via
  zigbuild with zig from the PyPI `ziglang` package). `dist/server/` also contains the
  deploy assets so `install.sh` runs from the copied directory.

## ADR-404: Coturn drop-in

* Decision: `coturn-override.conf` raises `LimitNOFILE`, orders after
  `network-online.target`, restarts on failure, and sets
  `SupplementaryGroups=chattercrab` in addition to adding `turnserver` to the group
  (the drop-in takes effect even if group membership was changed while Coturn ran).

## ADR-405: Services are not started without a certificate

* Context: with `CHATTERCRAB_TLS_CERT` set and no file, the signaling server would fail
  and restart in a loop.
* Decision: `install.sh` installs and enables everything but skips the (re)start and
  prints the certbot commands when the configured certificate is missing.

## ADR-406: Certificate deploy hook writes atomically

* Decision: the certbot hook copies `fullchain.pem`/`privkey.pem` to temporary files in
  `/etc/chattercrab/tls/` (`root:chattercrab`, 0640) and renames them into place, then
  reloads `chattercrab-signal` (SIGHUP hot swap) only if active and restarts Coturn only
  if enabled. Without `RENEWED_LINEAGE` it uses the single lineage under
  `/etc/letsencrypt/live`, so it can also be run by hand.

## ADR-407: Firewall ordering

* Decision: `firewall.sh` sets default deny incoming and allows SSH before `ufw --force
  enable`, so enabling never cuts the current SSH session. The signal port is a flag
  (`--signal-port`, default 8443) so the 443 variant needs no script edits. It has a
  `--dry-run` mode; it was not executed for real inside WSL.
