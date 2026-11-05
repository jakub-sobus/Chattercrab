#!/usr/bin/env bash
# Chattercrab VPS firewall rules (ufw).
#
# Usage: sudo ./firewall.sh [--signal-port PORT] [--dry-run]
#
# Opens SSH, HTTP (certbot standalone), the signaling port, and the Coturn
# listener and relay ports. Everything else inbound is denied.
set -euo pipefail

SIGNAL_PORT="${CHATTERCRAB_SIGNAL_PORT:-8443}"
DRY_RUN=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --signal-port)
            SIGNAL_PORT="$2"
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h | --help)
            sed -n '2,8p' "$0"
            exit 0
            ;;
        *)
            echo "firewall.sh: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

if ! [[ "$SIGNAL_PORT" =~ ^[0-9]+$ ]] || ((SIGNAL_PORT < 1 || SIGNAL_PORT > 65535)); then
    echo "firewall.sh: invalid signal port: $SIGNAL_PORT" >&2
    exit 2
fi

run() {
    if ((DRY_RUN)); then
        echo "[dry-run] $*"
    else
        "$@"
    fi
}

if ((!DRY_RUN)); then
    if [[ $EUID -ne 0 ]]; then
        echo "firewall.sh: must run as root" >&2
        exit 1
    fi
    if ! command -v ufw >/dev/null 2>&1; then
        echo "firewall.sh: installing ufw"
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ufw
    fi
fi

run ufw default deny incoming
run ufw default allow outgoing

# SSH first, so enabling ufw never locks us out.
run ufw allow 22/tcp comment 'ssh'
run ufw allow 80/tcp comment 'certbot standalone'
run ufw allow "${SIGNAL_PORT}/tcp" comment 'chattercrab-signal'
run ufw allow 3478/udp comment 'coturn stun/turn'
run ufw allow 3478/tcp comment 'coturn stun/turn'
run ufw allow 5349/tcp comment 'coturn turns'
run ufw allow 5349/udp comment 'coturn turns (dtls)'
run ufw allow 49152:49999/udp comment 'coturn relay range'

run ufw --force enable
run ufw status verbose
