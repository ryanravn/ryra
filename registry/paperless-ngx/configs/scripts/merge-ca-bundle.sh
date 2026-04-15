#!/bin/bash
set -euo pipefail
# Merge Caddy's self-signed root CA into the system CA bundle so Python/requests
# trusts the internal Caddy cert for OIDC token exchange with Authelia.
SERVICE_HOME="$HOME/.local/share/ryra/paperless-ngx"
CADDY_CA="$HOME/.local/share/ryra/caddy-root-ca.crt"
MERGED="$SERVICE_HOME/ca-bundle.crt"

# Extract the system CA bundle from the image
podman run --rm --entrypoint cat ghcr.io/paperless-ngx/paperless-ngx:latest \
  /etc/ssl/certs/ca-certificates.crt > "$MERGED" 2>/dev/null

# Append Caddy's root CA if it exists (only when --auth with caddy)
[ -f "$CADDY_CA" ] && cat "$CADDY_CA" >> "$MERGED"
