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

# Restart seahub so it picks up the OAuth config. seahub.sh restart is
# idempotent — it stops any running instance (no-op if not running) and
# starts a fresh one, so we don't need to probe for the process first.
# start.py's initial `seahub.sh start` can race with DB readiness; this
# ensures seahub is running with OAuth config loaded regardless.
echo "Restarting seahub to apply OAuth config..."
for attempt in 1 2 3; do
  if podman exec seafile /opt/seafile/seafile-server-latest/seahub.sh restart 2>&1; then
    echo "Seahub restarted (attempt $attempt)."
    # Verify seahub is listening on its port (8000) before exiting.
    for i in $(seq 1 30); do
      if podman exec seafile curl -sf -o /dev/null http://localhost:8000/ 2>/dev/null; then
        echo "Seahub is responding."
        exit 0
      fi
      sleep 2
    done
    echo "  seahub restart succeeded but not responding — retrying..."
  else
    echo "  seahub.sh restart failed — retrying..."
    sleep 5
  fi
done
echo "ERROR: seahub failed to start after 3 attempts"
exit 1
