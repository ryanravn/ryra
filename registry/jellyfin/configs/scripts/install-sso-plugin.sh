#!/bin/bash
set -euo pipefail
# Install the Jellyfin SSO plugin if not already present.
# Only needed when --auth is used (OAUTH_CLIENT_ID will be set).
[ -z "${OAUTH_CLIENT_ID:-}" ] && exit 0

PLUGIN_DIR="$RYRA_SERVICE_HOME/config/plugins/SSO-Auth"
[ -d "$PLUGIN_DIR" ] && [ -n "$(ls -A "$PLUGIN_DIR" 2>/dev/null)" ] && exit 0

mkdir -p "$PLUGIN_DIR"
PLUGIN_URL="https://github.com/9p4/jellyfin-plugin-sso/releases/download/v4.0.0.4/sso-authentication_4.0.0.4.zip"
TMP_ZIP="$(mktemp)"
curl -fsSL --retry 3 --retry-delay 2 -o "$TMP_ZIP" "$PLUGIN_URL"
python3 -c "import zipfile,sys; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$TMP_ZIP" "$PLUGIN_DIR"
rm -f "$TMP_ZIP"
