#!/bin/bash
# Robustly restart seahub so it picks up any seahub_settings_*.py injected
# by earlier ExecStartPost scripts. Runs as the last ExecStartPost in
# seafile.container, so only one restart happens per boot — avoids the
# race between multiple back-to-back `seahub.sh restart` calls, where
# seafile's stop_seahub errors out ("Failed to stop seahub") when pkill
# can't kill a freshly-forked gunicorn in its 1-second grace window.
#
# Also doubles as recovery for the first-boot race where start.py's
# `seahub.sh start` times out waiting for gunicorn to appear in pgrep.
set -euo pipefail

# Nothing to apply if neither integration is configured — start.py already
# started seahub; no need to bounce it.
if [ -z "${OAUTH_CLIENT_ID:-}" ] && [ -z "${SMTP_HOST:-}" ]; then
  exit 0
fi

echo "Restarting seahub to apply injected config..."
for attempt in 1 2 3; do
  if podman exec seafile /opt/seafile/seafile-server-latest/seahub.sh restart 2>&1; then
    for i in $(seq 1 30); do
      if podman exec seafile curl -sf -o /dev/null http://localhost:8000/ 2>/dev/null; then
        echo "Seahub is responding (attempt $attempt)."
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
