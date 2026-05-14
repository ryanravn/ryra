#!/bin/bash
# Switch Nextcloud's background job runner from the default `ajax` (runs
# on page loads — unreliable) to `cron`, which is what the nextcloud-cron
# sidecar container drives via /cron.sh every 5 minutes.
#
# Idempotent: safe to run on every start.
set -euo pipefail

echo "Waiting for Nextcloud to finish installing..."
for i in $(seq 1 120); do
  STATUS=$(podman exec -u www-data nextcloud php occ status --output=json 2>/dev/null || true)
  echo "$STATUS" | grep -q '"installed":true' && break
  sleep 5
done

podman exec -u www-data nextcloud php occ background:cron 2>&1 \
  || echo "background:cron mode switch failed (will retry on next restart)"
