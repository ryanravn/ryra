#!/bin/bash
# Restart vikunja against the restored data.
set -euo pipefail
systemctl --user start vikunja.service
