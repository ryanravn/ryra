#!/bin/bash
set -euo pipefail
systemctl --user stop uptime-kuma.service || true
sleep 2
podman unshare chown -R 0:0 "$SERVICE_HOME/data"
