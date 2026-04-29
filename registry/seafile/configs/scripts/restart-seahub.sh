#!/bin/bash
# Restart seahub *only when needed* — i.e. when the inject-* scripts
# actually appended a new include to seahub_settings.py this boot.
# On subsequent boots the includes are already there from /shared, so
# start.py inside the container starts seahub with the OAuth/SMTP
# settings already loaded; restarting is unnecessary and triggers
# seafile's stop_seahub gunicorn-pkill race ("Failed to stop seahub")
# which fails ExecStartPost and puts the unit into a restart loop.
set -euo pipefail

# Nothing to apply if neither integration is configured.
if [ -z "${OAUTH_CLIENT_ID:-}" ] && [ -z "${SMTP_HOST:-}" ]; then
  exit 0
fi

CONF=$SERVICE_HOME/shared/seafile/conf
MARKER=$CONF/.seahub-restart-needed
if [ ! -f "$MARKER" ]; then
  echo "Configs already loaded by start.py — skipping restart."
  exit 0
fi

echo "Restarting seahub to apply newly-injected config..."
for attempt in 1 2 3; do
  if podman exec seafile /opt/seafile/seafile-server-latest/seahub.sh restart 2>&1; then
    for i in $(seq 1 30); do
      if podman exec seafile curl -sf -o /dev/null http://localhost:8000/ 2>/dev/null; then
        echo "Seahub is responding (attempt $attempt)."
        rm -f "$MARKER"
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
