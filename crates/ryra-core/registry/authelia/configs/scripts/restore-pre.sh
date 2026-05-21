#!/bin/bash
set -euo pipefail
systemctl --user stop authelia.service || true
sleep 2
# No `:U` on the volume, so plain rm works as ryra.
rm -rf "$SERVICE_HOME/config"
mkdir -p "$SERVICE_HOME/config"
