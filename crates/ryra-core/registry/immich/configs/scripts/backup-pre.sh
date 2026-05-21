#!/bin/bash
# Stop immich + postgres + valkey + ML so postgres flushes WAL and
# the upload dir is quiescent. Chown both to namespace-root (= ryra
# on the host) so restic can read everything; the next container
# start re-applies `:U`.
set -euo pipefail

UNITS=(
    immich.service
    immich-machine-learning.service
    immich-postgres.service
    immich-valkey.service
)
systemctl --user stop "${UNITS[@]}" || true
sleep 3

podman unshare chown -R 0:0 "$SERVICE_HOME/db-data" "$SERVICE_HOME/upload"
