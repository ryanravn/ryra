#!/bin/bash
set -euo pipefail
systemctl --user stop uptime-kuma.service || true
sleep 2
podman unshare rm -rf "$SERVICE_HOME/data"
mkdir -p "$SERVICE_HOME/data"
