#!/bin/bash
# Prepare for restic restore: stop vikunja, then wipe the live data
# directories so restic's restore lands on a clean slate. Without the
# wipe, restic merges into the existing tree — files removed in the
# snapshot would remain on disk, producing a half-restored mess.
set -euo pipefail
systemctl --user stop vikunja.service || true
sleep 2
rm -rf "$SERVICE_HOME/db" "$SERVICE_HOME/files"
