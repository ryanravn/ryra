#!/bin/bash
set -euo pipefail

UNITS=(
    zammad.service
    zammad-scheduler.service
    zammad-websocket.service
    zammad-railsserver.service
    zammad-init.service
    zammad-elasticsearch.service
    zammad-memcached.service
    zammad-redis.service
    zammad-postgres.service
)
systemctl --user stop "${UNITS[@]}" || true
sleep 3

podman unshare chown -R 0:0 "$SERVICE_HOME/postgres-data" "$SERVICE_HOME/storage"
