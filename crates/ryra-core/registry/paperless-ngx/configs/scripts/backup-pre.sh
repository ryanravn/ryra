#!/bin/bash
# Stop paperless + redis. The sqlite database lives in `data/`,
# so a quiescent service is enough to guarantee a consistent
# snapshot. Chown to namespace-root for restic to read.
set -euo pipefail

UNITS=(paperless-ngx.service paperless-ngx-redis.service)
systemctl --user stop "${UNITS[@]}" || true
sleep 3

podman unshare chown -R 0:0 "$SERVICE_HOME/data" "$SERVICE_HOME/media"
