#!/bin/bash
set -euo pipefail
UNITS=(synapse.service synapse-homeserver.service synapse-db.service)
systemctl --user stop "${UNITS[@]}" || true
sleep 3
podman unshare chown -R 0:0 "$SERVICE_HOME/db" "$SERVICE_HOME/data"
