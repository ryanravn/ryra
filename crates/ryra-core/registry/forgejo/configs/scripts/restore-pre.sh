#!/bin/bash
set -euo pipefail

UNITS=(forgejo.service forgejo-postgres.service)
systemctl --user stop "${UNITS[@]}" || true
sleep 3

podman unshare rm -rf "$SERVICE_HOME/db-data" "$SERVICE_HOME/data"
mkdir -p "$SERVICE_HOME/db-data" "$SERVICE_HOME/data"
