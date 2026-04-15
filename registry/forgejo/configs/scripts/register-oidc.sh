#!/bin/bash
set -euo pipefail
[ -z "${OIDC_CLIENT_ID:-}" ] && exit 0
echo "Waiting for Forgejo API..."
for i in $(seq 1 120); do
  curl -sf http://127.0.0.1:$RYRA_PORT_HTTP/api/v1/settings/api >/dev/null 2>&1 && break
  sleep 5
done
podman exec -u git forgejo forgejo admin auth add-oauth \
  --name "Authelia" \
  --provider "openidConnect" \
  --key "$OIDC_CLIENT_ID" \
  --secret "$OIDC_CLIENT_SECRET" \
  --auto-discover-url "$OIDC_DISCOVERY_URL" \
  --custom-auth-url "$OIDC_AUTH_URL" \
  --custom-token-url "$OIDC_TOKEN_URL" \
  --custom-profile-url "$OIDC_PROFILE_URL" \
  --scopes "openid email profile" 2>&1 || true
