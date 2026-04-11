#!/bin/bash
set -euo pipefail
# Write SSO plugin configuration for OIDC.
# Runs as ExecStartPre — writes the XML config before Jellyfin starts.
# Jellyfin reads this config on boot to register the OIDC provider.
# Only runs when --auth is used (OAUTH_CLIENT_ID will be set).
[ -z "${OAUTH_CLIENT_ID:-}" ] && exit 0

CONFIG_DIR="$RYRA_SERVICE_HOME/config/plugins/configurations"
CONFIG_FILE="$CONFIG_DIR/SSO-Auth.xml"

# If the config already contains our client_id, skip
if [ -f "$CONFIG_FILE" ] && grep -q "$OAUTH_CLIENT_ID" "$CONFIG_FILE"; then
  exit 0
fi

mkdir -p "$CONFIG_DIR"

# Write branding config with SSO login button
BRANDING_DIR="$RYRA_SERVICE_HOME/config/config"
BRANDING_FILE="$BRANDING_DIR/branding.xml"
if [ ! -f "$BRANDING_FILE" ] || ! grep -q "sso/OID" "$BRANDING_FILE"; then
  mkdir -p "$BRANDING_DIR"
  cat > "$BRANDING_FILE" <<'BRANDING'
<?xml version="1.0" encoding="utf-8"?>
<BrandingOptions xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <LoginDisclaimer>&lt;form action="/sso/OID/start/authelia"&gt;&lt;button class="raised block emby-button button-submit"&gt;Sign in with SSO&lt;/button&gt;&lt;/form&gt;</LoginDisclaimer>
</BrandingOptions>
BRANDING
fi

cat > "$CONFIG_FILE" <<XML
<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <SamlConfigs />
  <OidConfigs>
    <item>
      <key>
        <string>authelia</string>
      </key>
      <value>
        <PluginConfiguration>
          <OidEndpoint>${OAUTH_ISSUER_URL}</OidEndpoint>
          <OidClientId>${OAUTH_CLIENT_ID}</OidClientId>
          <OidSecret>${OAUTH_CLIENT_SECRET}</OidSecret>
          <Enabled>true</Enabled>
          <EnableAuthorization>true</EnableAuthorization>
          <EnableAllFolders>true</EnableAllFolders>
          <EnabledFolders />
          <AdminRoles />
          <Roles />
          <EnableFolderRoles>false</EnableFolderRoles>
          <EnableLiveTvRoles>false</EnableLiveTvRoles>
          <EnableLiveTv>false</EnableLiveTv>
          <EnableLiveTvManagement>false</EnableLiveTvManagement>
          <LiveTvRoles />
          <LiveTvManagementRoles />
          <FolderRoleMappings />
          <RoleClaim>groups</RoleClaim>
          <OidScopes>
            <string>groups</string>
          </OidScopes>
          <DisableHttps>false</DisableHttps>
          <DisablePushedAuthorization>true</DisablePushedAuthorization>
          <DoNotValidateEndpoints>true</DoNotValidateEndpoints>
          <DoNotValidateIssuerName>true</DoNotValidateIssuerName>
          <DoNotLoadProfile>false</DoNotLoadProfile>
          <CanonicalLinks />
        </PluginConfiguration>
      </value>
    </item>
  </OidConfigs>
</PluginConfiguration>
XML
