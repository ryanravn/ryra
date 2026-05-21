#!/bin/bash
# Stop authelia so sqlite finishes its WAL checkpoint before restic
# reads config/. No `:U` on the bind-mount, so files are already
# owned by ryra on the host — no chown gymnastics needed.
set -euo pipefail
systemctl --user stop authelia.service || true
sleep 2
