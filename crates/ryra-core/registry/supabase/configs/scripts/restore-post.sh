#!/bin/bash
# Sequenced startup against the freshly-restored data: start
# postgres first, wait for ready, reset-failed, then the rest of
# the stack. Sequencing prevents supabase-auth from racing
# postgres's WAL replay and crashlooping past systemd's
# StartLimitBurst.
set -euo pipefail

systemctl --user start supabase-db.service

echo "waiting for supabase-db..."
for _ in $(seq 1 90); do
    if podman exec supabase-db pg_isready -U postgres -q >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
podman exec supabase-db pg_isready -U postgres -q

systemctl --user reset-failed || true
systemctl --user start supabase.service
