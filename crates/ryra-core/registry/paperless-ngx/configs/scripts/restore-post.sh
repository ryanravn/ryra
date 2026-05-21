#!/bin/bash
# Sqlite-backed: no init-script race to worry about, but we still
# reset-failed in case anything stuck during the prior cycle, then
# start paperless (which Requires= redis and cascades).
set -euo pipefail
systemctl --user reset-failed || true
systemctl --user start paperless-ngx.service
