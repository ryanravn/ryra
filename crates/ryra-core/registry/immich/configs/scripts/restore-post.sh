#!/bin/bash
# Sequenced startup: postgres first, wait for ready, reset-failed,
# then immich.service (cascades the rest). Avoids the gotcha where
# immich races postgres's WAL replay on restore and crashloops past
# systemd's StartLimitBurst.
set -euo pipefail

systemctl --user start immich-postgres.service

echo "waiting for immich-postgres..."
for _ in $(seq 1 90); do
    if podman exec immich-postgres pg_isready -U postgres -q >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
podman exec immich-postgres pg_isready -U postgres -q

systemctl --user reset-failed || true

systemctl --user start immich.service
