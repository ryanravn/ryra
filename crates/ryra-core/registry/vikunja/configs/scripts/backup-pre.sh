#!/bin/bash
# Stop vikunja so the sqlite db at $SERVICE_HOME/db/vikunja.db is in a
# quiescent state when restic reads it. A live sqlite file can have
# uncommitted WAL pages that won't replay cleanly on restore.
set -euo pipefail
systemctl --user stop vikunja.service || true
# Give the unit a beat to fully release file handles. Without it,
# restic occasionally races and reports `Database is locked`-style
# warnings even though the unit reports inactive.
sleep 2
