#!/bin/bash
## Render homeserver.yaml (and optional OIDC overlay) into the synapse data
## dir on each start. envsubst keeps the templates readable YAML instead of
## a heredoc in shell. Runs as ExecStartPre so the config is fresh every
## time environment or secrets change.
set -euo pipefail

SERVICE_HOME="${SERVICE_HOME:-$HOME/services/synapse}"
CONFIGS="$SERVICE_HOME/configs"
DATA="$SERVICE_HOME/data"

mkdir -p "$DATA/conf.d" "$DATA/media_store"

envsubst < "$CONFIGS/homeserver.yaml.tpl" > "$DATA/homeserver.yaml"

# OIDC overlay — only when --auth wired OAUTH_CLIENT_ID into .env.
if [ -n "${OAUTH_CLIENT_ID:-}" ]; then
  envsubst < "$CONFIGS/oidc.yaml.tpl" > "$DATA/conf.d/oidc.yaml"
else
  # Ensure stale overlay from a prior --auth install is gone.
  rm -f "$DATA/conf.d/oidc.yaml"
fi
