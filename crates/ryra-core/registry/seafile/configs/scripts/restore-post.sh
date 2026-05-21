#!/bin/bash
# Pipe the restored SQL dump back into mariadb, then bring seafile
# back up. The dump's --add-drop-database lines drop and recreate
# ccnet_db/seafile_db/seahub_db so the import is idempotent across
# repeated restores.
set -euo pipefail

DUMP="$SERVICE_HOME/.backup/all-dbs.sql"
if [ ! -s "$DUMP" ]; then
    echo "ERROR: SQL dump not found or empty at $DUMP" >&2
    exit 1
fi

# Defensive: restore-pre didn't stop mariadb, but ping anyway in
# case the user stopped it manually between snapshot and restore.
echo "waiting for seafile-mariadb..."
for _ in $(seq 1 90); do
    if podman exec seafile-mariadb mariadb-admin ping --silent >/dev/null 2>&1; then break; fi
    sleep 1
done
podman exec seafile-mariadb mariadb-admin ping --silent

podman exec -i -e MYSQL_PWD="$MYSQL_ROOT_PASSWORD" seafile-mariadb \
    mariadb -uroot < "$DUMP"

systemctl --user reset-failed seafile.service || true
systemctl --user start seafile.service
