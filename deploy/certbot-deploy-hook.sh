#!/usr/bin/env bash
# Certbot deploy hook for Chattercrab.
#
# Installed by install.sh into /etc/letsencrypt/renewal-hooks/deploy/.
# Certbot runs it after every successful issuance or renewal with
# RENEWED_LINEAGE set to the live directory (e.g. /etc/letsencrypt/live/chat.example.com).
# It copies the certificate and key into /etc/chattercrab/tls/ readable by the
# chattercrab group, reloads the signaling server (SIGHUP, no dropped
# connections) and restarts coturn (which cannot reload certificates).
#
# Manual use: RENEWED_LINEAGE=/etc/letsencrypt/live/<domain> ./certbot-deploy-hook.sh
set -euo pipefail

TLS_DIR=/etc/chattercrab/tls
LINEAGE="${RENEWED_LINEAGE:-}"

if [[ -z "$LINEAGE" ]]; then
    # Fall back to the only lineage, if there is exactly one.
    mapfile -t lineages < <(find /etc/letsencrypt/live -mindepth 1 -maxdepth 1 -type d 2>/dev/null)
    if [[ ${#lineages[@]} -ne 1 ]]; then
        echo "certbot-deploy-hook: RENEWED_LINEAGE not set and ${#lineages[@]} lineages found" >&2
        exit 1
    fi
    LINEAGE="${lineages[0]}"
fi

for f in fullchain.pem privkey.pem; do
    if [[ ! -r "$LINEAGE/$f" ]]; then
        echo "certbot-deploy-hook: missing $LINEAGE/$f" >&2
        exit 1
    fi
done

install -d -m 0750 -o root -g chattercrab "$TLS_DIR"
# Write to temporary files first so readers never see a half-written key.
install -m 0640 -o root -g chattercrab "$LINEAGE/fullchain.pem" "$TLS_DIR/.fullchain.pem.new"
install -m 0640 -o root -g chattercrab "$LINEAGE/privkey.pem" "$TLS_DIR/.privkey.pem.new"
mv -f "$TLS_DIR/.fullchain.pem.new" "$TLS_DIR/fullchain.pem"
mv -f "$TLS_DIR/.privkey.pem.new" "$TLS_DIR/privkey.pem"

if systemctl is-active --quiet chattercrab-signal; then
    systemctl reload chattercrab-signal
fi
if systemctl is-enabled --quiet coturn 2>/dev/null; then
    systemctl restart coturn
fi
echo "certbot-deploy-hook: installed certificates from $LINEAGE"
