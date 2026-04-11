#!/bin/bash
set -euo pipefail
# Configure Jellyfin's known proxies so it trusts X-Forwarded-* headers from Caddy.
# Without this, the SSO plugin constructs redirect_uris using the internal HTTP URL
# instead of the external HTTPS URL.
CONFIG_DIR="$RYRA_SERVICE_HOME/config/config"
NETWORK_FILE="$CONFIG_DIR/network.xml"
[ -f "$NETWORK_FILE" ] && exit 0

mkdir -p "$CONFIG_DIR"

cat > "$NETWORK_FILE" <<XML
<?xml version="1.0" encoding="utf-8"?>
<NetworkConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <BaseUrl />
  <EnableHttps>false</EnableHttps>
  <RequireHttps>false</RequireHttps>
  <InternalHttpPort>8096</InternalHttpPort>
  <InternalHttpsPort>8920</InternalHttpsPort>
  <PublicHttpPort>8096</PublicHttpPort>
  <PublicHttpsPort>8920</PublicHttpsPort>
  <EnableRemoteAccess>true</EnableRemoteAccess>
  <KnownProxies>
    <string>0.0.0.0/0</string>
    <string>::/0</string>
  </KnownProxies>
</NetworkConfiguration>
XML
