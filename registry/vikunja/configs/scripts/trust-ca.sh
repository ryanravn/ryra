#!/bin/bash
set -euo pipefail
# Trust caddy's self-signed CA cert inside the vikunja container.
# Vikunja fetches the OIDC discovery URL at startup which goes through
# caddy's HTTPS endpoint with a self-signed cert.
CADDY_CA="$(dirname "$RYRA_SERVICE_HOME")/caddy-root-ca.crt"
[ -f "$CADDY_CA" ] || exit 0
podman cp "$CADDY_CA" vikunja:/tmp/caddy-ca.crt 2>/dev/null || exit 0
podman exec vikunja sh -c "cat /tmp/caddy-ca.crt >> /etc/ssl/certs/ca-certificates.crt" 2>/dev/null || true
