#!/bin/bash
set -euo pipefail
# Merge Caddy's self-signed root CA into certifi's CA bundle so Python/httpx
# trusts both public CAs and the internal Caddy cert for OIDC discovery.
SERVICE_HOME="$HOME/.local/share/ryra/open-webui"
CADDY_CA="$HOME/.local/share/ryra/caddy-root-ca.crt"
MERGED="$SERVICE_HOME/ca-bundle.crt"

# Extract certifi's original CA bundle from the image
podman run --rm --entrypoint cat ghcr.io/open-webui/open-webui:latest \
  /usr/local/lib/python3.11/site-packages/certifi/cacert.pem > "$MERGED" 2>/dev/null

# Append Caddy's root CA if it exists (only when --auth with caddy)
[ -f "$CADDY_CA" ] && cat "$CADDY_CA" >> "$MERGED"
