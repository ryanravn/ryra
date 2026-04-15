#!/bin/bash
set -euo pipefail
# Merge Caddy's self-signed root CA into certifi's CA bundle so Python/httpx
# trusts both public CAs and the internal Caddy cert for OIDC discovery.
SERVICE_HOME="$HOME/.local/share/ryra/open-webui"
CADDY_CA="$HOME/.local/share/ryra/caddy-root-ca.crt"
MERGED="$SERVICE_HOME/ca-bundle.crt"

# Only needed when --auth is used (Caddy self-signed cert needs trusting)
[ -f "$CADDY_CA" ] || exit 0

# Extract certifi's original CA bundle from the image
podman run --rm --entrypoint cat ghcr.io/open-webui/open-webui:latest \
  /usr/local/lib/python3.11/site-packages/certifi/cacert.pem > "$MERGED" 2>/dev/null || true

# Append Caddy's root CA
cat "$CADDY_CA" >> "$MERGED" 2>/dev/null || true
