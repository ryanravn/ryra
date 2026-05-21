#!/bin/bash
set -euo pipefail

systemctl --user start twenty-postgres.service

echo "waiting for twenty-postgres..."
for _ in $(seq 1 90); do
    if podman exec twenty-postgres pg_isready -U twenty -q >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
podman exec twenty-postgres pg_isready -U twenty -q

systemctl --user reset-failed || true

systemctl --user start twenty.service
systemctl --user start twenty-worker.service
