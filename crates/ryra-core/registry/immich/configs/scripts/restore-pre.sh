#!/bin/bash
# Stop the stack, wipe data volumes so restic restores into clean
# trees. mkdir + container `:U` chown round-trips ownership.
set -euo pipefail

UNITS=(
    immich.service
    immich-machine-learning.service
    immich-postgres.service
    immich-valkey.service
)
systemctl --user stop "${UNITS[@]}" || true
sleep 3

podman unshare rm -rf "$SERVICE_HOME/db-data" "$SERVICE_HOME/upload"
mkdir -p "$SERVICE_HOME/db-data" "$SERVICE_HOME/upload"
