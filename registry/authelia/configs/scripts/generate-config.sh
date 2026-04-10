#!/bin/bash
set -euo pipefail
CONFIG_DIR="$RYRA_SERVICE_HOME/config"
mkdir -p "$CONFIG_DIR"
CONFIG_FILE="$CONFIG_DIR/configuration.yml"
[ -f "$CONFIG_FILE" ] && exit 0
DOMAIN="${RYRA_DOMAIN:-localhost}"
if [ "$DOMAIN" = "localhost" ]; then
  COOKIE_DOMAIN="127.0.0.1"
elif echo "$DOMAIN" | grep -q '\..*\.'; then
  COOKIE_DOMAIN="${DOMAIN#*.}"
else
  COOKIE_DOMAIN="$DOMAIN"
fi
if [ "$DOMAIN" != "localhost" ] && systemctl --user is-active caddy.service >/dev/null 2>&1; then
  AUTHELIA_URL="https://$DOMAIN:8443"
else
  AUTHELIA_URL="https://$COOKIE_DOMAIN"
fi
cat > "$CONFIG_FILE" <<YAML
---
server:
  address: 'tcp://0.0.0.0:9091'
log:
  level: 'info'
authentication_backend:
  file:
    path: '/config/users_database.yml'
session:
  cookies:
    - domain: '$COOKIE_DOMAIN'
      authelia_url: '$AUTHELIA_URL'
storage:
  local:
    path: '/config/db.sqlite3'
notifier:
  filesystem:
    filename: '/config/notification.txt'
access_control:
  default_policy: 'one_factor'
YAML
