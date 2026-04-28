#!/bin/bash
## Render Element's config.json with the correct homeserver base_url.
## Without --url, Element is served at http://127.0.0.1:$PORT_HTTP
## and that's also where the browser will call /_matrix (same origin via
## element-nginx.conf). With --url, SERVICE_EXTERNAL_URL overrides.
set -euo pipefail

SERVICE_HOME="${SERVICE_HOME:-$HOME/services/synapse}"
CONFIGS="$SERVICE_HOME/configs"

# Pick the external URL Element should advertise. SERVICE_EXTERNAL_URL is
# set to the --url value (https://…) when present; otherwise fall back to
# the loopback http URL on the published port.
export ELEMENT_BASE_URL="${SERVICE_EXTERNAL_URL:-http://127.0.0.1:${PORT_HTTP}}"

envsubst < "$CONFIGS/element-config.json.tpl" > "$CONFIGS/config.json"
