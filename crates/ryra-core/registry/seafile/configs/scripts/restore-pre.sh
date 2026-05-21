#!/bin/bash
# Online-restore. Mariadb stays running; restore-post.sh pipes the
# SQL dump back in and the --add-drop-database lines in the dump
# drop+recreate the three seafile databases cleanly. Only
# seafile.service is stopped so seahub/seafdav don't fight us
# while restic rewrites the shared/ tree.
set -euo pipefail

systemctl --user stop seafile.service || true
sleep 2

# Clear shared/ so `restic restore` writes onto a known-empty tree;
# without this, files added since the snapshot would linger and
# desync the on-disk state from the SQL we're about to import.
# db-data/ is left untouched: mariadb keeps serving and the dump
# replaces its content.
podman unshare rm -rf "$SERVICE_HOME/shared"
mkdir -p "$SERVICE_HOME/shared"
