#!/bin/bash
set -euo pipefail
UNITS=(seafile.service seafile-mariadb.service seafile-redis.service)
systemctl --user stop "${UNITS[@]}" || true
sleep 3
podman unshare rm -rf "$SERVICE_HOME/db-data" "$SERVICE_HOME/shared"
mkdir -p "$SERVICE_HOME/db-data" "$SERVICE_HOME/shared"
