#!/bin/bash
# Register the auth provider as an OIDC login source via the `user_oidc` app.
#
# Requires `--auth` (ryra injects OAUTH_CLIENT_ID / OAUTH_CLIENT_SECRET /
# OAUTH_ISSUER_URL into .env). The script exits 0 silently otherwise.
set -euo pipefail
[ -z "${OAUTH_CLIENT_ID:-}" ] && exit 0

echo "Waiting for Nextcloud to finish installing..."
for i in $(seq 1 120); do
  STATUS=$(podman exec -u www-data nextcloud php occ status --output=json 2>/dev/null || true)
  echo "$STATUS" | grep -q '"installed":true' && break
  sleep 5
done

# The OIDC issuer URL (e.g. https://auth.internal:8443/...) must resolve
# and terminate TLS cleanly from inside the nextcloud container:
#
# 1. `.internal` isn't a real DNS zone — we inject the auth domain →
#    caddy IP mapping into the container's /etc/hosts so nextcloud's
#    PHP cURL can reach caddy. (We deliberately moved off `.localhost`
#    because Debian-patched libcurl hardcodes `*.localhost` → 127.0.0.1
#    and ignores /etc/hosts, which broke this exact call path.)
# 2. Caddy uses a self-signed CA. Append it to the container's trust
#    bundle in-place — `update-ca-certificates` fails on a running
#    container with "Device or resource busy" because /etc/ssl/certs
#    is backed by overlay fs and it can't atomically replace the bundle.
#    Appending to the existing ca-certificates.crt is what forgejo's
#    equivalent script does and works reliably.
AUTH_HOST=$(echo "$OAUTH_ISSUER_URL" | sed 's|https\?://||; s|[:/].*||')
CADDY_IP=$(podman inspect caddy --format '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' 2>/dev/null | awk '{print $1}')
CADDY_CA="$(dirname "$SERVICE_HOME")/caddy-root-ca.crt"

# /etc/hosts entry for the auth domain — PHP cURL, Guzzle, getent, and
# curl -v all honour this for non-`.localhost` hostnames.
if [ -n "$CADDY_IP" ] && [ -n "$AUTH_HOST" ]; then
  podman exec nextcloud sh -c "echo '$CADDY_IP $AUTH_HOST' >> /etc/hosts" 2>/dev/null || true
fi

# Import the caddy CA into Nextcloud's own trusted-certs store so
# server-side calls (discovery, token exchange) don't fail cert
# validation. Nextcloud mounts /etc/ssl/certs/ca-certificates.crt read-only
# from the host, so appending to that bundle fails — occ has a dedicated
# command that adds the cert to a Nextcloud-managed location instead.
if [ -f "$CADDY_CA" ]; then
  podman cp "$CADDY_CA" nextcloud:/tmp/caddy-ca.crt 2>/dev/null || true
  podman exec -u www-data nextcloud php occ security:certificates:import /tmp/caddy-ca.crt 2>&1 || true
fi

# Install and enable user_oidc if not already.
podman exec -u www-data nextcloud php occ app:install user_oidc 2>&1 \
  || podman exec -u www-data nextcloud php occ app:enable user_oidc 2>&1 \
  || echo "user_oidc install/enable failed (non-fatal)"

# Authelia's token endpoint default is client_secret_basic; user_oidc defaults
# the other way. Force client_secret_post so the discovery handshake works.
podman exec -u www-data nextcloud php occ config:system:set \
  user_oidc default_token_endpoint_auth_method --value=client_secret_post 2>&1 || true

# Nextcloud's HTTP client blocks outbound requests to loopback and
# private-range addresses as an SSRF precaution. Caddy's container IP
# (which `auth.internal` resolves to via /etc/hosts) is in podman's
# default 10.89.0.0/16 range, so user_oidc's discovery fetch raises
# LocalServerException and the SSO flow dies before redirect. Allow
# local targets explicitly — ryra's auth bridge is trusted and runs
# on the same host.
podman exec -u www-data nextcloud php occ config:system:set \
  allow_local_remote_servers --value=true --type=bool 2>&1 || true

# Register (or update) the provider. The `--unique-uid=0` flag keeps the
# user-ID mapping stable across auth providers, which matches ryra's
# single-IdP model.
PROVIDER_NAME="${OAUTH_PROVIDER:-authelia}"
DISCOVERY_URI="${OAUTH_ISSUER_URL}/.well-known/openid-configuration"

# `user_oidc:provider` upserts by name. Exit 0 on failure so systemd
# doesn't kill the service — log the error for debugging instead.
podman exec -u www-data nextcloud php occ user_oidc:provider "$PROVIDER_NAME" \
  --clientid="$OAUTH_CLIENT_ID" \
  --clientsecret="$OAUTH_CLIENT_SECRET" \
  --discoveryuri="$DISCOVERY_URI" \
  --scope="openid profile email" \
  --mapping-uid=preferred_username \
  --mapping-email=email \
  --mapping-display-name=name \
  --unique-uid=0 2>&1 \
  || echo "OIDC provider registration failed (will retry on next restart)"
