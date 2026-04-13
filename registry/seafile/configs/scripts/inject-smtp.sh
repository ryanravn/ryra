#!/bin/bash
[ -z "${SMTP_HOST:-}" ] && exit 0

CONF=$RYRA_SERVICE_HOME/shared/seafile/conf

echo "Waiting for $CONF/seahub_settings.py to appear..."
for i in $(seq 1 60); do
  [ -f "$CONF/seahub_settings.py" ] && break
  sleep 10
done
[ -f "$CONF/seahub_settings.py" ] || { echo "ERROR: $CONF/seahub_settings.py not found"; exit 1; }

# Determine TLS settings from SMTP_SECURITY env var
USE_TLS="False"
USE_SSL="False"
case "${SMTP_SECURITY:-off}" in
  starttls) USE_TLS="True" ;;
  force_tls) USE_SSL="True" ;;
esac

cat > "$CONF/seahub_settings_smtp.py" << EOF
EMAIL_USE_TLS = $USE_TLS
EMAIL_USE_SSL = $USE_SSL
EMAIL_HOST = '$SMTP_HOST'
EMAIL_HOST_USER = '$SMTP_USERNAME'
EMAIL_HOST_PASSWORD = '$SMTP_PASSWORD'
EMAIL_PORT = ${SMTP_PORT:-587}
DEFAULT_FROM_EMAIL = '$SMTP_FROM'
SERVER_EMAIL = '$SMTP_FROM'
EOF

grep -q seahub_settings_smtp "$CONF/seahub_settings.py" || \
  echo "exec(open('/shared/seafile/conf/seahub_settings_smtp.py').read())" >> "$CONF/seahub_settings.py"

echo "SMTP config injected into seahub_settings.py"

# Restart seahub so it picks up the SMTP config.
# This runs after inject-oauth.sh, so both OAuth and SMTP are loaded.
echo "Waiting for seahub to start before restarting with SMTP config..."
for i in $(seq 1 30); do
  if podman exec seafile pgrep -f "seahub" >/dev/null 2>&1; then
    echo "Restarting seahub to apply SMTP config..."
    podman exec seafile /opt/seafile/seafile-server-latest/seahub.sh restart 2>&1 || true
    echo "Seahub restarted with SMTP config."
    exit 0
  fi
  sleep 2
done
echo "WARNING: Could not restart seahub — run 'systemctl --user restart seafile' manually"
