#!/bin/bash
# Postgres first, wait for ready, reset-failed, then zammad
# (cascades the railsserver + init + websocket + scheduler +
# elasticsearch + memcached + redis chain).
set -euo pipefail

systemctl --user start zammad-postgres.service

echo "waiting for zammad-postgres..."
for _ in $(seq 1 90); do
    if podman exec zammad-postgres pg_isready -U zammad -q >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
podman exec zammad-postgres pg_isready -U zammad -q

systemctl --user reset-failed || true

systemctl --user start zammad.service
