#!/bin/bash
set -euo pipefail
systemctl --user reset-failed || true
systemctl --user start vaultwarden.service
