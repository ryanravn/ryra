#!/bin/bash
# Start immich.service — Requires= cascades to postgres + valkey + ML.
set -euo pipefail
systemctl --user start immich.service
