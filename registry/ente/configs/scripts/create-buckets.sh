#!/bin/bash
# Create MinIO buckets required by Ente museum.
# Runs as ExecStartPost after the minio container starts.
set -euo pipefail

for i in $(seq 1 30); do
    podman exec ente-minio mc alias set local http://localhost:3200 "$MINIO_ROOT_USER" "$MINIO_ROOT_PASSWORD" 2>/dev/null && break
    sleep 2
done

podman exec ente-minio mc mb -p local/b2-eu-cen 2>/dev/null || true
podman exec ente-minio mc mb -p local/wasabi-eu-central-2-v3 2>/dev/null || true
podman exec ente-minio mc mb -p local/scw-eu-fr-v3 2>/dev/null || true
echo "MinIO buckets ready"
