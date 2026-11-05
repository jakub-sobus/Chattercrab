#!/usr/bin/env bash
# Chattercrab VPS installer. Run as root on Debian or Ubuntu. Idempotent: rerun it
# to upgrade the binary or repair the configuration.
#
# Usage: sudo ./install.sh [options]
#
#   --binary PATH      chattercrab-signal binary (default: next to this script,
#                      then ../dist/server/chattercrab-signal)
#   --domain NAME      public DNS name; replaces chat.example.com in a new env file
#                      and is used for the health check
#   --public-ip IP     skip public IP discovery for NAT detection
#   --skip-apt         do not install packages
#   --skip-firewall    do not run firewall.sh
#   --no-start         install and enable services but do not (re)start them
#   --dry-run          print what would be done, change nothing
#   -h, --help         show this help
#
# Steps: install coturn and certbot, create the chattercrab system user, install the
# binary, env file, TURN secret, coturn config (with NAT external-ip), systemd units,
# certbot deploy hook and firewall rules, then enable and start both services.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BIN_DIR=/opt/chattercrab/bin
ETC_DIR=/etc/chattercrab
TLS_DIR=$ETC_DIR/tls
ENV_FILE=$ETC_DIR/env
TURN_CONF=/etc/turnserver.conf
UNIT_FILE=/etc/systemd/system/chattercrab-signal.service
COTURN_DROPIN_DIR=/etc/systemd/system/coturn.service.d
HOOK_FILE=/etc/letsencrypt/renewal-hooks/deploy/chattercrab.sh
SECRET_PLACEHOLDER=__GENERATED_BY_INSTALL_SH__
MANAGED_MARKER="# Managed by Chattercrab install.sh"

BINARY=""
DOMAIN=""
PUBLIC_IP=""
SKIP_APT=0
SKIP_FIREWALL=0
NO_START=0
DRY_RUN=0

log() { echo "==> $*"; }
warn() { echo "WARNING: $*" >&2; }
die() {
    echo "ERROR: $*" >&2
    exit 1
}

usage() { sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary)
            BINARY="$2"
            shift 2
            ;;
        --domain)
            DOMAIN="$2"
            shift 2
            ;;
        --public-ip)
            PUBLIC_IP="$2"
            shift 2
            ;;
        --skip-apt)
            SKIP_APT=1
            shift
            ;;
        --skip-firewall)
            SKIP_FIREWALL=1
            shift
            ;;
        --no-start)
            NO_START=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

# Runs a mutating command, or prints it in dry-run mode.
run() {
    if ((DRY_RUN)); then
        echo "[dry-run] $*"
    else
        "$@"
    fi
}

# Sets KEY=VALUE in a KEY=VALUE style file, replacing an existing line or appending.
set_kv() {
    local file="$1" key="$2" value="$3"
    if ((DRY_RUN)); then
        echo "[dry-run] set ${key} in ${file}"
        return
    fi
    local tmp
    tmp="$(mktemp)"
    awk -v k="$key" -v v="$value" '
        BEGIN { done = 0 }
        index($0, k "=") == 1 { if (!done) { print k "=" v; done = 1 }; next }
        { print }
        END { if (!done) print k "=" v }
    ' "$file" >"$tmp"
    cat "$tmp" >"$file"
    rm -f "$tmp"
}

# Prints the value of KEY from a KEY=VALUE file (empty if absent).
get_kv() {
    local file="$1" key="$2"
    [[ -r "$file" ]] || return 0
    awk -v k="$key" 'index($0, k "=") == 1 { print substr($0, length(k) + 2); exit }' "$file"
}

random_secret() {
    # 32 random bytes, hex encoded (64 characters).
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'
}

discover_public_ip() {
    local ip="" url
    for url in https://api.ipify.org https://ifconfig.me/ip https://icanhazip.com; do
        ip="$(curl -fsS4 --max-time 5 "$url" 2>/dev/null | tr -d '[:space:]')" || ip=""
        if [[ "$ip" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
            echo "$ip"
            return
        fi
    done
    # Fall back to a STUN binding request with the coturn client tools.
    if command -v turnutils_stunclient >/dev/null 2>&1; then
        ip="$(timeout 5 turnutils_stunclient stun.l.google.com -p 19302 2>/dev/null |
            grep -oE 'UDP reflexive addr: [0-9.]+' | head -n1 | awk '{print $4}')" || ip=""
        if [[ "$ip" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
            echo "$ip"
        fi
    fi
}

primary_private_ip() {
    ip -4 route get 1.1.1.1 2>/dev/null |
        awk '{ for (i = 1; i <= NF; i++) if ($i == "src") { print $(i + 1); exit } }'
}

local_ips() {
    ip -4 -o addr show scope global 2>/dev/null | awk '{ split($4, a, "/"); print a[1] }'
}

# ---------------------------------------------------------------------------
# Preconditions

if ((!DRY_RUN)) && [[ $EUID -ne 0 ]]; then
    die "must run as root (try: sudo $0 $*)"
fi

if [[ -z "$BINARY" ]]; then
    for candidate in "$SCRIPT_DIR/chattercrab-signal" "$SCRIPT_DIR/../dist/server/chattercrab-signal"; do
        if [[ -f "$candidate" ]]; then
            BINARY="$candidate"
            break
        fi
    done
fi
[[ -n "$BINARY" && -f "$BINARY" ]] || die "chattercrab-signal binary not found; pass --binary PATH"

ENV_TEMPLATE="$SCRIPT_DIR/chattercrab.env.example"
for f in "$ENV_TEMPLATE" "$SCRIPT_DIR/turnserver.conf" "$SCRIPT_DIR/chattercrab-signal.service" \
    "$SCRIPT_DIR/coturn-override.conf" "$SCRIPT_DIR/certbot-deploy-hook.sh" "$SCRIPT_DIR/firewall.sh"; do
    [[ -f "$f" ]] || die "missing deploy asset: $f"
done

# ---------------------------------------------------------------------------
# 1. Packages

if ((SKIP_APT)); then
    log "skipping package installation"
else
    log "installing coturn, certbot and curl"
    run env DEBIAN_FRONTEND=noninteractive apt-get update -qq
    run env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq coturn certbot curl iproute2
fi
if [[ -f /etc/default/coturn ]]; then
    # Older Debian packaging only starts coturn when this flag is set.
    if ! grep -q '^TURNSERVER_ENABLED=1' /etc/default/coturn; then
        set_kv /etc/default/coturn TURNSERVER_ENABLED 1
    fi
fi

# ---------------------------------------------------------------------------
# 2. System user and group

if getent group chattercrab >/dev/null; then
    log "group chattercrab exists"
else
    log "creating group chattercrab"
    run groupadd --system chattercrab
fi
if id -u chattercrab >/dev/null 2>&1; then
    log "user chattercrab exists"
else
    log "creating user chattercrab"
    run useradd --system --gid chattercrab --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin chattercrab
fi
if id -u turnserver >/dev/null 2>&1; then
    if id -nG turnserver | tr ' ' '\n' | grep -qx chattercrab; then
        log "turnserver is already in group chattercrab"
    else
        log "adding turnserver to group chattercrab (certificate access)"
        run usermod -aG chattercrab turnserver
    fi
else
    warn "user turnserver not found; is coturn installed?"
fi

# ---------------------------------------------------------------------------
# 3. Binary and directories

log "installing $BINARY to $BIN_DIR/chattercrab-signal"
run install -d -m 0755 -o root -g root "$BIN_DIR"
run install -m 0755 -o root -g root "$BINARY" "$BIN_DIR/chattercrab-signal"
run install -d -m 0750 -o root -g chattercrab "$ETC_DIR"
run install -d -m 0750 -o root -g chattercrab "$TLS_DIR"

# ---------------------------------------------------------------------------
# 4. Environment file

if [[ -f "$ENV_FILE" ]]; then
    log "$ENV_FILE exists, keeping it"
    ENV_SOURCE="$ENV_FILE"
else
    log "creating $ENV_FILE from template"
    run install -m 0640 -o root -g chattercrab "$ENV_TEMPLATE" "$ENV_FILE"
    if [[ -n "$DOMAIN" ]]; then
        run sed -i "s/chat\.example\.com/${DOMAIN//./\\.}/g" "$ENV_FILE"
    fi
    ENV_SOURCE="$ENV_FILE"
    ((DRY_RUN)) && ENV_SOURCE="$ENV_TEMPLATE"
fi
run chown root:chattercrab "$ENV_FILE"
run chmod 0640 "$ENV_FILE"

REALM="$(get_kv "$ENV_SOURCE" CHATTERCRAB_TURN_REALM)"
BIND="$(get_kv "$ENV_SOURCE" CHATTERCRAB_BIND)"
TLS_CERT="$(get_kv "$ENV_SOURCE" CHATTERCRAB_TLS_CERT)"
[[ -n "$REALM" ]] || REALM="${DOMAIN:-chat.example.com}"
if [[ -n "$DOMAIN" && "$REALM" == "chat.example.com" ]]; then
    REALM="$DOMAIN"
fi
if [[ "$REALM" == "chat.example.com" ]]; then
    warn "CHATTERCRAB_TURN_REALM is still chat.example.com; edit $ENV_FILE or pass --domain"
fi
SIGNAL_PORT="${BIND##*:}"
[[ "$SIGNAL_PORT" =~ ^[0-9]+$ ]] || SIGNAL_PORT=8443

# ---------------------------------------------------------------------------
# 5. TURN shared secret (env file and coturn must agree)

SECRET="$(get_kv "$ENV_SOURCE" CHATTERCRAB_TURN_SECRET)"
if [[ -z "$SECRET" || "$SECRET" == "$SECRET_PLACEHOLDER" ]]; then
    log "generating a new TURN shared secret"
    SECRET="$(random_secret)"
    set_kv "$ENV_FILE" CHATTERCRAB_TURN_SECRET "$SECRET"
else
    log "keeping existing TURN shared secret"
fi

if [[ -f "$TURN_CONF" ]] && grep -qxF "$MANAGED_MARKER" "$TURN_CONF"; then
    log "$TURN_CONF is managed by Chattercrab, updating it in place"
else
    if [[ -f "$TURN_CONF" && ! -f "$TURN_CONF.orig" ]]; then
        log "backing up the existing $TURN_CONF to $TURN_CONF.orig"
        run cp -p "$TURN_CONF" "$TURN_CONF.orig"
    fi
    log "installing $TURN_CONF"
    if ((DRY_RUN)); then
        echo "[dry-run] write $TURN_CONF from $SCRIPT_DIR/turnserver.conf"
    else
        {
            echo "$MANAGED_MARKER"
            cat "$SCRIPT_DIR/turnserver.conf"
        } >"$TURN_CONF"
    fi
fi
set_kv "$TURN_CONF" static-auth-secret "$SECRET"
set_kv "$TURN_CONF" realm "$REALM"
run chown root:turnserver "$TURN_CONF" 2>/dev/null || run chown root:root "$TURN_CONF"
run chmod 0640 "$TURN_CONF"

# ---------------------------------------------------------------------------
# 6. NAT detection for coturn external-ip

if [[ -z "$PUBLIC_IP" ]]; then
    log "discovering the public IPv4 address"
    PUBLIC_IP="$(discover_public_ip || true)"
fi
PRIVATE_IP="$(primary_private_ip || true)"
if [[ -z "$PUBLIC_IP" ]]; then
    warn "could not determine the public IP; leaving external-ip unchanged"
elif local_ips | grep -qxF "$PUBLIC_IP"; then
    log "public IP $PUBLIC_IP is bound locally: no NAT, external-ip not needed"
    if ((DRY_RUN)); then
        echo "[dry-run] remove external-ip= lines from $TURN_CONF"
    else
        sed -i '/^external-ip=/d' "$TURN_CONF"
    fi
elif [[ -n "$PRIVATE_IP" ]]; then
    log "behind NAT: public $PUBLIC_IP, private $PRIVATE_IP"
    set_kv "$TURN_CONF" external-ip "$PUBLIC_IP/$PRIVATE_IP"
else
    warn "public IP $PUBLIC_IP is not local but no private source IP was found"
    set_kv "$TURN_CONF" external-ip "$PUBLIC_IP"
fi

# ---------------------------------------------------------------------------
# 7. Certificates, systemd units, certbot hook, firewall

log "installing the certbot deploy hook to $HOOK_FILE"
run install -d -m 0755 "$(dirname "$HOOK_FILE")"
run install -m 0755 -o root -g root "$SCRIPT_DIR/certbot-deploy-hook.sh" "$HOOK_FILE"

if [[ -n "$DOMAIN" && -d "/etc/letsencrypt/live/$DOMAIN" ]]; then
    log "copying certificates from /etc/letsencrypt/live/$DOMAIN"
    run env RENEWED_LINEAGE="/etc/letsencrypt/live/$DOMAIN" "$HOOK_FILE"
fi
CERTS_PRESENT=1
if [[ -n "$TLS_CERT" && ! -f "$TLS_CERT" ]]; then
    CERTS_PRESENT=0
    warn "TLS certificate $TLS_CERT not found. Obtain one first:"
    warn "  certbot certonly --standalone -d <domain>"
    warn "  RENEWED_LINEAGE=/etc/letsencrypt/live/<domain> $HOOK_FILE"
fi

log "installing systemd units"
run install -m 0644 -o root -g root "$SCRIPT_DIR/chattercrab-signal.service" "$UNIT_FILE"
run install -d -m 0755 "$COTURN_DROPIN_DIR"
run install -m 0644 -o root -g root "$SCRIPT_DIR/coturn-override.conf" "$COTURN_DROPIN_DIR/override.conf"

if ((SKIP_FIREWALL)); then
    log "skipping firewall configuration"
else
    log "configuring the firewall"
    if ((DRY_RUN)); then
        bash "$SCRIPT_DIR/firewall.sh" --signal-port "$SIGNAL_PORT" --dry-run
    else
        bash "$SCRIPT_DIR/firewall.sh" --signal-port "$SIGNAL_PORT"
    fi
fi

run systemctl daemon-reload
run systemctl enable chattercrab-signal coturn

if ((NO_START)); then
    log "--no-start given: services enabled but not started"
elif ((!CERTS_PRESENT)); then
    warn "not starting services until the TLS certificate exists"
else
    log "(re)starting chattercrab-signal and coturn"
    run systemctl restart chattercrab-signal
    run systemctl restart coturn
fi

# ---------------------------------------------------------------------------
# 8. Health check

HOST="${DOMAIN:-$REALM}"
SCHEME=https
[[ -z "$TLS_CERT" ]] && SCHEME=http
HEALTH_URL="$SCHEME://$HOST:$SIGNAL_PORT/health"
log "health check: curl -fsS $HEALTH_URL"
if ((!DRY_RUN && !NO_START && CERTS_PRESENT)); then
    HEALTHY=0
    for _ in 1 2 3 4 5; do
        if curl -fsS --max-time 3 "$HEALTH_URL"; then
            echo
            HEALTHY=1
            break
        fi
        sleep 2
    done
    if ((HEALTHY)); then
        log "chattercrab-signal is healthy"
    else
        warn "health check failed; if DNS is not set up yet try: curl -fsSk https://127.0.0.1:$SIGNAL_PORT/health"
    fi
    systemctl --no-pager --lines=0 status chattercrab-signal coturn || true
fi
log "done. Logs: journalctl -u chattercrab-signal -f   and   journalctl -u coturn -f"
