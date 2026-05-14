#!/bin/bash
set -euo pipefail
IMMICH_URL="http://127.0.0.1:$SERVICE_PORT_HTTP"
ADMIN_EMAIL="$INIT_IMMICH_ADMIN_EMAIL"
ADMIN_PASSWORD="$INIT_IMMICH_ADMIN_PASSWORD"

echo "Waiting for Immich API to be ready (up to 10 min)..."
for i in $(seq 1 60); do
  CODE=$(curl -so /dev/null -w '%{http_code}' --connect-timeout 5 --max-time 10 "$IMMICH_URL/api/server/ping" 2>/dev/null || true)
  [ "$CODE" = "200" ] && break
  echo "  not yet — retrying in 10s (${i}0s elapsed)"
  sleep 10
done
[ "$CODE" = "200" ] || { echo "ERROR: Immich not ready after 600s"; exit 1; }
echo "Immich API ready"

JSON=$(printf '{"email":"%s","password":"%s","name":"Admin"}' "$ADMIN_EMAIL" "$ADMIN_PASSWORD")
curl -sf --max-time 30 -X POST "$IMMICH_URL/api/auth/admin-sign-up" \
  -H "Content-Type: application/json" \
  -d "$JSON" \
  >/dev/null 2>&1 || true
echo "Admin account ready"
