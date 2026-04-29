#!/bin/bash
set -euo pipefail
[ -z "${OAUTH_CLIENT_ID:-}" ] && exit 0

CONF=$SERVICE_HOME/shared/seafile/conf

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

if ! grep -q seahub_settings_oauth "$CONF/seahub_settings.py"; then
  echo "exec(open('/shared/seafile/conf/seahub_settings_oauth.py').read())" >> "$CONF/seahub_settings.py"
  # Tell restart-seahub.sh that start.py started seahub without these
  # settings loaded — a one-shot restart is required this boot.
  touch "$CONF/.seahub-restart-needed"
fi

echo "OAuth config injected into seahub_settings.py"
