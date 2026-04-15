#!/bin/bash
set -euo pipefail
[ -z "${OAUTH_CLIENT_ID:-}" ] && exit 0

CONF=$RYRA_SERVICE_HOME/shared/seafile/conf

echo "Waiting for $CONF/seahub_settings.py to appear (Seafile creates it on first boot)..."
for i in $(seq 1 60); do
  [ -f "$CONF/seahub_settings.py" ] && break
  ELAPSED=$((i * 10))
  echo "  not yet — retrying in 10s (${ELAPSED}s elapsed)"
  sleep 10
done
[ -f "$CONF/seahub_settings.py" ] || { echo "ERROR: $CONF/seahub_settings.py not found after 600s"; exit 1; }

cat > "$CONF/seahub_settings_oauth.py" << EOF
ENABLE_OAUTH = True
OAUTH_CREATE_UNKNOWN_USER = True
OAUTH_ACTIVATE_USER_AFTER_CREATION = True
OAUTH_CLIENT_ID = "$OAUTH_CLIENT_ID"
OAUTH_CLIENT_SECRET = "$OAUTH_CLIENT_SECRET"
OAUTH_REDIRECT_URL = "$OAUTH_REDIRECT_URL"
OAUTH_PROVIDER_DOMAIN = "$OAUTH_PROVIDER_DOMAIN"
OAUTH_PROVIDER = "authelia"
OAUTH_AUTHORIZATION_URL = "${OAUTH_PROVIDER_DOMAIN}/api/oidc/authorization"
OAUTH_TOKEN_URL = "${OAUTH_INTERNAL_DOMAIN}/api/oidc/token"
OAUTH_USER_INFO_URL = "${OAUTH_INTERNAL_DOMAIN}/api/oidc/userinfo"
OAUTH_SCOPE = ["openid", "profile", "email"]
OAUTH_ATTRIBUTE_MAP = {"email": (True, "contact_email"), "name": (False, "name"), "sub": (False, "uid")}
EOF

grep -q seahub_settings_oauth "$CONF/seahub_settings.py" || \
  echo "exec(open('/shared/seafile/conf/seahub_settings_oauth.py').read())" >> "$CONF/seahub_settings.py"

echo "OAuth config injected into seahub_settings.py"

# Wait for seahub to be running inside the container, then restart it
# so it picks up the OAuth config. Seahub starts after the settings file
# is created, so we need to wait for it to be listening.
echo "Waiting for seahub to start before restarting with OAuth config..."
for i in $(seq 1 30); do
  if podman exec seafile pgrep -f "seahub" >/dev/null 2>&1; then
    echo "Restarting seahub to apply OAuth config..."
    podman exec seafile /opt/seafile/seafile-server-latest/seahub.sh restart 2>&1 || true
    echo "Seahub restarted with OAuth config."
    exit 0
  fi
  sleep 2
done
echo "WARNING: Could not restart seahub — run 'systemctl --user restart seafile' manually"
