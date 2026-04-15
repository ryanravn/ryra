#!/bin/bash
set -euo pipefail
[ -z "${OIDC_CLIENT_ID:-}" ] && exit 0
echo "Waiting for Forgejo API..."
for i in $(seq 1 120); do
  curl -sf http://127.0.0.1:$RYRA_PORT_HTTP/api/v1/settings/api >/dev/null 2>&1 && break
  sleep 5
done

# The OIDC discovery URL (e.g. https://auth.localhost:8443/...) must be
# reachable from inside the forgejo container with valid TLS.
#
# Two issues to fix:
# 1. .localhost always resolves to 127.0.0.1 (RFC 6761/glibc) — inject
#    the auth domain → caddy IP mapping into the container's /etc/hosts.
# 2. Caddy uses a self-signed CA — inject the CA cert so the Forgejo CLI
#    trusts it during OIDC discovery.
AUTH_HOST=$(echo "$OIDC_DISCOVERY_URL" | sed 's|https\?://||; s|[:/].*||')
CADDY_IP=$(podman inspect caddy --format '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' 2>/dev/null | awk '{print $1}')
CADDY_CA="$(dirname "$RYRA_SERVICE_HOME")/caddy-root-ca.crt"

if [ -n "$CADDY_IP" ] && [ -n "$AUTH_HOST" ]; then
  podman exec forgejo sh -c "echo '$CADDY_IP $AUTH_HOST' >> /etc/hosts" 2>/dev/null || true
fi
if [ -f "$CADDY_CA" ]; then
  podman cp "$CADDY_CA" forgejo:/tmp/caddy-ca.crt 2>/dev/null || true
  podman exec forgejo sh -c "cat /tmp/caddy-ca.crt >> /etc/ssl/certs/ca-certificates.crt" 2>/dev/null || true
fi

# Exit 0 intentionally: ExecStartPost failure would cause systemd to
# kill the service. Log the error for debugging instead.
echo "Registering OIDC provider..."
podman exec -u git forgejo forgejo admin auth add-oauth \
  --name "Authelia" \
  --provider "openidConnect" \
  --key "$OIDC_CLIENT_ID" \
  --secret "$OIDC_CLIENT_SECRET" \
  --auto-discover-url "$OIDC_DISCOVERY_URL" \
  --custom-auth-url "$OIDC_AUTH_URL" \
  --custom-token-url "$OIDC_TOKEN_URL" \
  --custom-profile-url "$OIDC_PROFILE_URL" \
  --scopes "openid email profile" 2>&1 || echo "OIDC registration failed (will retry on next restart)"
