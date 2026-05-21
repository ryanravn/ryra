#!/bin/bash
set -euo pipefail

UNITS=(
    twenty.service
    twenty-worker.service
    twenty-postgres.service
    twenty-redis.service
)
systemctl --user stop "${UNITS[@]}" || true
sleep 3

podman unshare chown -R 0:0 "$SERVICE_HOME/db-data" "$SERVICE_HOME/data"
