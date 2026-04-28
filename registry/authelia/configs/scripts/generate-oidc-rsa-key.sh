#!/bin/bash
set -euo pipefail
CONFIG_DIR="$SERVICE_HOME/config"
RSA_KEY="$CONFIG_DIR/oidc.jwk.rsa.pem"
[ -f "$RSA_KEY" ] && exit 0
podman run --rm -v "$CONFIG_DIR:/out:Z" docker.io/authelia/authelia:4.39 \
  authelia crypto pair rsa generate --directory /out >/dev/null 2>&1 || { echo "Warning: failed to generate RSA key for OIDC"; exit 0; }
mv "$CONFIG_DIR/private.pem" "$RSA_KEY" 2>/dev/null || echo "Warning: failed to rename RSA key"
rm -f "$CONFIG_DIR/public.pem"
