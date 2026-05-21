#!/bin/bash
set -euo pipefail

UNITS=(paperless-ngx.service paperless-ngx-redis.service)
systemctl --user stop "${UNITS[@]}" || true
sleep 3

podman unshare rm -rf "$SERVICE_HOME/data" "$SERVICE_HOME/media"
mkdir -p "$SERVICE_HOME/data" "$SERVICE_HOME/media"
