#!/bin/bash
# Stop the entire supabase stack so postgres flushes WAL and exits
# cleanly before restic snapshots the data directory. A live
# postgres datadir has uncommitted WAL pages and open file handles
# — capturing it mid-flight can produce a snapshot that won't
# replay.
#
# Each container is its own systemd .service; Requires= governs
# startup, not shutdown, so we list every unit explicitly.
set -euo pipefail

UNITS=(
    supabase.service
    supabase-supavisor.service
    supabase-studio.service
    supabase-storage.service
    supabase-realtime.service
    supabase-rest.service
    supabase-meta.service
    supabase-imgproxy.service
    supabase-auth.service
    supabase-db.service
)
systemctl --user stop "${UNITS[@]}" || true
sleep 3

# Chown the bind-mount trees to namespace-root so restic (running
# as the unprivileged ryra user) can read everything. The next
# container start re-applies `:U` and chowns back to the container
# user; ownership round-trips.
podman unshare chown -R 0:0 "$SERVICE_HOME/db-data" "$SERVICE_HOME/storage-data"
