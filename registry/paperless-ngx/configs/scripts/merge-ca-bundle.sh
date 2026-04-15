#!/bin/bash
set -euo pipefail
# Merge Caddy's self-signed root CA into the system CA bundle so Python/requests
# trusts the internal Caddy cert for OIDC token exchange with Authelia.
SERVICE_HOME="$HOME/.local/share/ryra/paperless-ngx"
CADDY_CA="$HOME/.local/share/ryra/caddy-root-ca.crt"
MERGED="$SERVICE_HOME/ca-bundle.crt"

# Only needed when --auth is used (Caddy self-signed cert needs trusting)
[ -f "$CADDY_CA" ] || exit 0

# Extract the system CA bundle from the image
podman run --rm --entrypoint cat ghcr.io/paperless-ngx/paperless-ngx:latest \
  /etc/ssl/certs/ca-certificates.crt > "$MERGED" 2>/dev/null || true

# Append Caddy's root CA
cat "$CADDY_CA" >> "$MERGED" 2>/dev/null || true
