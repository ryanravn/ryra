#!/bin/bash
# Postgres first, wait for ready, reset-failed, then synapse.service
# (which cascades the homeserver via Requires=).
set -euo pipefail
systemctl --user start synapse-db.service
echo "waiting for synapse-db..."
for _ in $(seq 1 90); do
    if podman exec synapse-db pg_isready -U synapse -q >/dev/null 2>&1; then break; fi
    sleep 1
done
podman exec synapse-db pg_isready -U synapse -q
systemctl --user reset-failed || true
systemctl --user start synapse.service
