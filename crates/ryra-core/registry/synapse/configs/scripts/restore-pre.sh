#!/bin/bash
set -euo pipefail
UNITS=(synapse.service synapse-homeserver.service synapse-db.service)
systemctl --user stop "${UNITS[@]}" || true
sleep 3
podman unshare rm -rf "$SERVICE_HOME/db" "$SERVICE_HOME/data"
mkdir -p "$SERVICE_HOME/db" "$SERVICE_HOME/data"
