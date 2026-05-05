#!/bin/bash
# Enable Seafile's WebDAV (SeafDAV) extension. The Docker image already
# proxies /seafdav/* through its built-in nginx, so no Caddy/quadlet
# changes are required: we only need to flip `enabled = true` and set
# `share_name = /seafdav` in seafdav.conf, then restart the seafile
# daemon so seafile-controller respawns seafdav with the new config.
#
# Idempotent: on subsequent boots seafdav.conf is already populated and
# the script exits early without bouncing the daemon.
set -euo pipefail

CONF=$SERVICE_HOME/shared/seafile/conf
SEAFDAV=$CONF/seafdav.conf

echo "Waiting for $CONF to appear (Seafile creates it on first boot)..."
for i in $(seq 1 60); do
  [ -d "$CONF" ] && break
  sleep 10
done
[ -d "$CONF" ] || { echo "ERROR: $CONF not found after 600s"; exit 1; }

if [ -f "$SEAFDAV" ] && grep -q '^enabled = true' "$SEAFDAV"; then
  echo "SeafDAV already enabled, skipping."
  exit 0
fi

cat > "$SEAFDAV" << 'EOF'
[WEBDAV]
enabled = true
port = 8080
share_name = /seafdav
EOF

echo "SeafDAV enabled. Restarting seafile to pick up the change..."
if ! podman exec seafile /opt/seafile/seafile-server-latest/seafile.sh restart 2>&1; then
  echo "WARN: seafile.sh restart failed; SeafDAV will activate on next container restart."
fi
