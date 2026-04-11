#!/bin/bash
# Create MinIO buckets required by PostHog.
for i in $(seq 1 30); do
    podman exec posthog-objectstorage mc alias set local http://localhost:19000 "$OBJECT_STORAGE_ACCESS_KEY_ID" "$OBJECT_STORAGE_SECRET_ACCESS_KEY" 2>/dev/null && break
    sleep 2
done

podman exec posthog-objectstorage mc mb -p local/posthog 2>/dev/null || true
echo "MinIO buckets ready"
