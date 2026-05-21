#!/bin/bash
# Stop the supabase stack and wipe both data volumes so restic
# restores into a clean tree.
#
# Without the wipe, `restic restore --target=/` merges into the
# existing files: anything created AFTER the snapshot stays on
# disk, leaving postgres with a half-restored datadir that mixes
# snapshot files and post-snapshot WAL.
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

# `podman unshare` is required because the bind-mounts use `:U` —
# podman has chown'd them to the in-container user (postgres = UID
# 999, storage = UID 1000), which maps to host subuids the ryra
# user can't touch. Inside `podman unshare`, that mapping is
# reversed and rm has the equivalent of root-on-the-mapped-namespace.
podman unshare rm -rf "$SERVICE_HOME/db-data" "$SERVICE_HOME/storage-data"

# Recreate the empty directories. Podman's quadlet runtime refuses
# to start a container whose bind-mount source doesn't exist
# (`Error: statfs ...: no such file or directory`), and `:U` re-
# chowns them to the in-container UID on next container start.
mkdir -p "$SERVICE_HOME/db-data" "$SERVICE_HOME/storage-data"
