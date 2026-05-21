#!/bin/bash
# Restart vikunja after restic has read the data dirs. Failing to
# restart leaves the service down — surface it loudly.
set -euo pipefail
systemctl --user start vikunja.service
