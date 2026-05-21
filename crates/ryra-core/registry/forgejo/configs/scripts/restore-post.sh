#!/bin/bash
# Postgres first, wait for ready, reset-failed, then forgejo
# (cascades). Same shape as supabase/nextcloud/immich.
set -euo pipefail

systemctl --user start forgejo-postgres.service

echo "waiting for forgejo-postgres..."
for _ in $(seq 1 90); do
    if podman exec forgejo-postgres pg_isready -U forgejo -q >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
podman exec forgejo-postgres pg_isready -U forgejo -q

systemctl --user reset-failed || true

systemctl --user start forgejo.service
