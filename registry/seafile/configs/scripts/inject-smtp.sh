#!/bin/bash
set -euo pipefail
[ -z "${SMTP_HOST:-}" ] && exit 0

CONF=$SERVICE_HOME/shared/seafile/conf

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

if ! grep -q seahub_settings_smtp "$CONF/seahub_settings.py"; then
  echo "exec(open('/shared/seafile/conf/seahub_settings_smtp.py').read())" >> "$CONF/seahub_settings.py"
  touch "$CONF/.seahub-restart-needed"
fi

echo "SMTP config injected into seahub_settings.py"
