#!/bin/bash
# MariaDB first, wait for ready, reset-failed, then seafile.service
# (cascades the rest).
set -euo pipefail
systemctl --user start seafile-mariadb.service
echo "waiting for seafile-mariadb..."
for _ in $(seq 1 90); do
    if podman exec seafile-mariadb mariadb-admin ping --silent >/dev/null 2>&1; then break; fi
    sleep 1
done
podman exec seafile-mariadb mariadb-admin ping --silent
systemctl --user reset-failed || true
systemctl --user start seafile.service
