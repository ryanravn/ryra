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

podman unshare rm -rf "$SERVICE_HOME/postgres-data" "$SERVICE_HOME/storage"
mkdir -p "$SERVICE_HOME/postgres-data" "$SERVICE_HOME/storage"

# Also wipe ES — the search index is a derivable artefact of the DB,
# but starting with a leftover index from before the restore would
# yield search results that point at deleted records. zammad-init
# will trigger a reindex when ES is empty.
podman unshare rm -rf "$SERVICE_HOME/es-data"
mkdir -p "$SERVICE_HOME/es-data"
