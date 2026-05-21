#!/bin/bash
# Online backup. `mariadb-dump --single-transaction` takes an
# InnoDB-consistent snapshot of the three seafile databases without
# table locks or service downtime; restic then snapshots the live
# shared/ tree alongside the dump. Seafile's content-addressable
# blob storage tolerates concurrent writes during the file
# snapshot, and the SQL-first order matches seafile's upstream
# guidance.
#
# MYSQL_PWD is passed via `podman exec -e` rather than `-p<pw>` so
# the password doesn't show up in `ps` / `/proc/PID/cmdline`.
set -euo pipefail

mkdir -p "$SERVICE_HOME/.backup"

podman exec -e MYSQL_PWD="$MYSQL_ROOT_PASSWORD" seafile-mariadb \
    mariadb-dump \
        --single-transaction \
        --routines \
        --triggers \
        --events \
        --add-drop-database \
        --databases ccnet_db seafile_db seahub_db \
        -uroot \
    > "$SERVICE_HOME/.backup/all-dbs.sql"
