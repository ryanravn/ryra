#!/bin/bash
set -euo pipefail
CONFIG_DIR="$RYRA_SERVICE_HOME/config"
mkdir -p "$CONFIG_DIR"
USERS_FILE="$CONFIG_DIR/users_database.yml"
[ -f "$USERS_FILE" ] && exit 0
[ -z "${AUTHELIA_ADMIN_PASSWORD:-}" ] && exit 0
USERNAME="${AUTHELIA_ADMIN_USER:-admin}"
EMAIL="${AUTHELIA_ADMIN_EMAIL:-admin@local}"
HASH=$(podman run --rm docker.io/authelia/authelia:4.39 authelia crypto hash generate argon2 --password "$AUTHELIA_ADMIN_PASSWORD" | grep '^Digest:' | sed 's/^Digest: //')
[ -z "$HASH" ] && exit 1
cat > "$USERS_FILE" <<YAML
---
users:
  $USERNAME:
    displayname: "$USERNAME"
    password: "$HASH"
    email: "$EMAIL"
YAML
